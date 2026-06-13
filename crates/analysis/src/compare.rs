//! Byte-level file comparison: equal? first-differing offset? size delta?

use crate::checksums::{checksum_file, ChecksumSet};
use crate::Result;
use serde::Serialize;
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct CompareResult {
    pub identical: bool,
    pub size_a: u64,
    pub size_b: u64,
    /// Byte offset of the first difference, or null if identical up to the
    /// shorter file's length (in which case the longer file has extra trailing
    /// bytes starting at min(size_a, size_b)).
    pub first_diff_offset: Option<u64>,
    /// SHA-256 of each, for a quick "are these the same content" answer.
    pub sha256_a: String,
    pub sha256_b: String,
}

/// Compare two files byte-by-byte (streaming, 64 KiB buffers) and also report
/// each side's SHA-256. Short-circuits on the first differing byte.
pub fn compare_files(a: impl AsRef<Path>, b: impl AsRef<Path>) -> Result<CompareResult> {
    let csa: ChecksumSet = checksum_file(a.as_ref())?;
    let csb: ChecksumSet = checksum_file(b.as_ref())?;

    let mut fa = File::open(a.as_ref())?;
    let mut fb = File::open(b.as_ref())?;
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    let mut offset: u64 = 0;
    let mut first_diff: Option<u64> = None;

    // Compare over the common prefix length.
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        let common = na.min(nb);
        for i in 0..common {
            if ba[i] != bb[i] {
                first_diff = Some(offset + i as u64);
                break;
            }
        }
        if first_diff.is_some() {
            break;
        }
        if na != nb {
            // One stream ended before the other within this block: the extra
            // bytes begin at offset + common.
            if na != nb {
                // Only a real difference if the shorter actually ended.
                if na < nb || nb < na {
                    // first_diff stays None here; the size difference captures it.
                }
            }
            break;
        }
        if na == 0 {
            break; // both EOF
        }
        offset += na as u64;
    }

    let identical = csa.sha256 == csb.sha256;
    Ok(CompareResult {
        identical,
        size_a: csa.size,
        size_b: csb.size,
        first_diff_offset: if identical { None } else { first_diff },
        sha256_a: csa.sha256,
        sha256_b: csb.sha256,
    })
}

/// Read until the buffer is full or EOF; returns bytes read.
fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn tmp(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn identical_files() {
        let a = tmp(b"hello world");
        let b = tmp(b"hello world");
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(r.identical);
        assert_eq!(r.first_diff_offset, None);
    }

    #[test]
    fn differ_in_middle() {
        let a = tmp(b"hello world");
        let b = tmp(b"hello WORLD");
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(!r.identical);
        assert_eq!(r.first_diff_offset, Some(6)); // 'w' vs 'W'
    }

    #[test]
    fn prefix_then_extra() {
        let a = tmp(b"abc");
        let b = tmp(b"abcdef");
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(!r.identical);
        assert_eq!(r.size_a, 3);
        assert_eq!(r.size_b, 6);
        // No differing byte in the common prefix; the difference is trailing length.
        assert_eq!(r.first_diff_offset, None);
    }
}
