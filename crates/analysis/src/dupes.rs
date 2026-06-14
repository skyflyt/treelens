//! Duplicate-file finder.
//!
//! Three-stage funnel so we only ever fully hash files that still might match:
//!   1. group by exact size (free — sizes come from the scan, no I/O);
//!   2. within each multi-file size group, hash a 4 KiB prefix to split off
//!      files that merely happen to share a size;
//!   3. within each multi-file prefix group, hash the full file to confirm.
//!
//! Files that can't be read (locked, permission denied) are skipped, not fatal.

use crate::checksums::hex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

const PREFIX_LEN: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct DupeGroup {
    /// Size of each file in the group, in bytes.
    pub size: u64,
    /// SHA-256 of the shared content.
    pub sha256: String,
    /// Absolute paths of the identical files (≥ 2).
    pub paths: Vec<String>,
    /// Bytes that could be reclaimed by keeping one copy: (count - 1) * size.
    pub redundant_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupeReport {
    pub groups: Vec<DupeGroup>,
    pub total_groups: usize,
    /// Sum of `redundant_bytes` across all returned groups.
    pub total_redundant_bytes: u64,
    /// True if more groups existed than `max_groups` and the list was trimmed.
    pub truncated: bool,
}

/// Hash the first `PREFIX_LEN` bytes of a file (or the whole file if shorter).
fn prefix_hash(path: &str) -> Option<String> {
    let mut f = File::open(Path::new(path)).ok()?;
    let mut buf = vec![0u8; PREFIX_LEN];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).ok()?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut h = Sha256::new();
    h.update(&buf[..filled]);
    Some(hex(&h.finalize()))
}

/// Full SHA-256 of a file, streamed in 64 KiB chunks.
fn full_hash(path: &str) -> Option<String> {
    let mut f = File::open(Path::new(path)).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex(&h.finalize()))
}

/// Find groups of byte-identical files among `files` (each `(path, size)`).
/// Returns the most impactful groups first (largest reclaimable space),
/// trimmed to `max_groups`.
pub fn find_duplicates(files: &[(String, u64)], max_groups: usize) -> DupeReport {
    // Stage 1: bucket by size.
    let mut by_size: HashMap<u64, Vec<&str>> = HashMap::new();
    for (path, size) in files {
        if *size == 0 {
            continue; // empty files are all "identical"; not useful as dupes
        }
        by_size.entry(*size).or_default().push(path.as_str());
    }

    let mut groups: Vec<DupeGroup> = Vec::new();
    for (size, paths) in by_size {
        if paths.len() < 2 {
            continue;
        }
        // Stage 2: split the size bucket by 4 KiB prefix hash.
        let mut by_prefix: HashMap<String, Vec<&str>> = HashMap::new();
        for p in paths {
            if let Some(ph) = prefix_hash(p) {
                by_prefix.entry(ph).or_default().push(p);
            }
        }
        for (_ph, ppaths) in by_prefix {
            if ppaths.len() < 2 {
                continue;
            }
            // Stage 3: confirm with the full hash.
            let mut by_full: HashMap<String, Vec<String>> = HashMap::new();
            for p in ppaths {
                if let Some(fh) = full_hash(p) {
                    by_full.entry(fh).or_default().push(p.to_string());
                }
            }
            for (fh, mut fpaths) in by_full {
                if fpaths.len() < 2 {
                    continue;
                }
                fpaths.sort_unstable();
                let redundant_bytes = (fpaths.len() as u64 - 1) * size;
                groups.push(DupeGroup {
                    size,
                    sha256: fh,
                    paths: fpaths,
                    redundant_bytes,
                });
            }
        }
    }

    // Most reclaimable first.
    groups.sort_unstable_by(|a, b| {
        b.redundant_bytes
            .cmp(&a.redundant_bytes)
            .then_with(|| a.sha256.cmp(&b.sha256))
    });
    let total_groups = groups.len();
    let truncated = total_groups > max_groups;
    if truncated {
        groups.truncate(max_groups);
    }
    let total_redundant_bytes = groups.iter().map(|g| g.redundant_bytes).sum();
    DupeReport {
        groups,
        total_groups,
        total_redundant_bytes,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn finds_identical_files_and_ignores_unique() {
        let dir = tempdir().unwrap();
        let a = write(dir.path(), "a.bin", b"hello world contents");
        let b = write(dir.path(), "b.bin", b"hello world contents"); // dup of a
        let c = write(dir.path(), "c.bin", b"something else entirely");
        let sizes = |p: &str| std::fs::metadata(p).unwrap().len();
        let files = vec![
            (a.clone(), sizes(&a)),
            (b.clone(), sizes(&b)),
            (c.clone(), sizes(&c)),
        ];
        let rep = find_duplicates(&files, 100);
        assert_eq!(rep.total_groups, 1);
        assert_eq!(rep.groups[0].paths.len(), 2);
        assert_eq!(rep.groups[0].redundant_bytes, sizes(&a));
    }

    #[test]
    fn same_size_different_content_is_not_a_dupe() {
        let dir = tempdir().unwrap();
        // Identical length, differ only after the 4 KiB prefix boundary.
        let mut va = vec![b'x'; 5000];
        let mut vb = va.clone();
        va[4500] = b'A';
        vb[4500] = b'B';
        let a = write(dir.path(), "a.bin", &va);
        let b = write(dir.path(), "b.bin", &vb);
        let files = vec![(a, 5000), (b, 5000)];
        let rep = find_duplicates(&files, 100);
        assert_eq!(rep.total_groups, 0, "same size, different tail — not dupes");
    }

    #[test]
    fn three_way_dupe_counts_redundancy_correctly() {
        let dir = tempdir().unwrap();
        let body = vec![b'z'; 2048];
        let a = write(dir.path(), "a", &body);
        let b = write(dir.path(), "b", &body);
        let c = write(dir.path(), "c", &body);
        let files = vec![(a, 2048), (b, 2048), (c, 2048)];
        let rep = find_duplicates(&files, 100);
        assert_eq!(rep.total_groups, 1);
        assert_eq!(rep.groups[0].paths.len(), 3);
        // Keep one of three → two copies reclaimable.
        assert_eq!(rep.groups[0].redundant_bytes, 2 * 2048);
        assert_eq!(rep.total_redundant_bytes, 2 * 2048);
    }

    #[test]
    fn zero_byte_files_are_ignored() {
        let dir = tempdir().unwrap();
        let a = write(dir.path(), "a", b"");
        let b = write(dir.path(), "b", b"");
        let files = vec![(a, 0), (b, 0)];
        let rep = find_duplicates(&files, 100);
        assert_eq!(rep.total_groups, 0);
    }
}
