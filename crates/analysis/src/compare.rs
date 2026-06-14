//! Byte-level file comparison: equal? first-differing offset? size delta?

use crate::checksums::hex;
use crate::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct CompareResult {
    pub identical: bool,
    pub size_a: u64,
    pub size_b: u64,
    /// Byte offset of the first difference within the common prefix, or null if
    /// the two files agree over the whole shorter file. When null but the files
    /// are not identical, the difference is purely trailing length — see
    /// `length_only_diff`.
    pub first_diff_offset: Option<u64>,
    /// True when the files share a common prefix but one is longer than the
    /// other (no differing byte in the overlap). Lets the UI say "same content,
    /// extra trailing bytes" instead of an ambiguous null offset.
    pub length_only_diff: bool,
    /// SHA-256 of each, for a quick "are these the same content" answer.
    pub sha256_a: String,
    pub sha256_b: String,
}

/// Compare two files in a single streaming pass (64 KiB buffers): each file is
/// read exactly once while we simultaneously locate the first differing byte
/// and compute each side's SHA-256. The earlier implementation read every file
/// twice (once in `checksum_file`, once in the byte loop) and carried a dead
/// no-op branch; this reads half as much I/O for the same result.
///
/// We keep draining both files to EOF even after the first difference is found,
/// because the SHA-256 of each whole file is part of the result.
pub fn compare_files(a: impl AsRef<Path>, b: impl AsRef<Path>) -> Result<CompareResult> {
    let mut fa = File::open(a.as_ref())?;
    let mut fb = File::open(b.as_ref())?;
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    let mut ha = Sha256::new();
    let mut hb = Sha256::new();
    let mut size_a: u64 = 0;
    let mut size_b: u64 = 0;
    // Absolute position of the start of the current block within the aligned
    // common prefix. Blocks stay aligned (full 64 KiB on both sides) until at
    // least one file hits EOF, so this is the correct base for `first_diff`.
    let mut aligned_offset: u64 = 0;
    let mut first_diff: Option<u64> = None;

    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na == 0 && nb == 0 {
            break;
        }
        ha.update(&ba[..na]);
        hb.update(&bb[..nb]);
        size_a += na as u64;
        size_b += nb as u64;

        if first_diff.is_none() {
            let common = na.min(nb);
            for i in 0..common {
                if ba[i] != bb[i] {
                    first_diff = Some(aligned_offset + i as u64);
                    break;
                }
            }
            aligned_offset += na.min(nb) as u64;
        }
    }

    let sha256_a = hex(&ha.finalize());
    let sha256_b = hex(&hb.finalize());
    let identical = size_a == size_b && first_diff.is_none();
    let length_only_diff = !identical && first_diff.is_none();

    Ok(CompareResult {
        identical,
        size_a,
        size_b,
        first_diff_offset: first_diff,
        length_only_diff,
        sha256_a,
        sha256_b,
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
        assert!(!r.length_only_diff);
        assert_eq!(r.first_diff_offset, None);
        // SHA-256 is computed in the same pass and must match for equal content.
        assert_eq!(r.sha256_a, r.sha256_b);
    }

    #[test]
    fn differ_in_middle() {
        let a = tmp(b"hello world");
        let b = tmp(b"hello WORLD");
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(!r.identical);
        assert!(!r.length_only_diff);
        assert_eq!(r.first_diff_offset, Some(6)); // 'w' vs 'W'
        assert_ne!(r.sha256_a, r.sha256_b);
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
        assert!(r.length_only_diff);
    }

    #[test]
    fn empty_vs_empty_is_identical() {
        let a = tmp(b"");
        let b = tmp(b"");
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(r.identical);
        assert!(!r.length_only_diff);
        assert_eq!(r.size_a, 0);
    }

    #[test]
    fn diff_spans_buffer_boundary() {
        // Force a difference well past the 64 KiB read block so the aligned-
        // offset accounting across blocks is exercised. Byte 100_000 differs.
        let n = 100_000usize;
        let mut va = vec![b'x'; n + 10];
        let mut vb = va.clone();
        va[n] = b'A';
        vb[n] = b'B';
        let a = tmp(&va);
        let b = tmp(&vb);
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(!r.identical);
        assert!(!r.length_only_diff);
        assert_eq!(r.first_diff_offset, Some(n as u64));
    }

    #[test]
    fn extra_bytes_past_buffer_boundary() {
        // Common prefix longer than one block, then B is longer: length-only.
        let a = tmp(&vec![b'z'; 70_000]);
        let mut vb = vec![b'z'; 70_000];
        vb.extend_from_slice(b"tail");
        let b = tmp(&vb);
        let r = compare_files(a.path(), b.path()).unwrap();
        assert!(!r.identical);
        assert!(r.length_only_diff);
        assert_eq!(r.first_diff_offset, None);
        assert_eq!(r.size_a, 70_000);
        assert_eq!(r.size_b, 70_004);
    }
}
