//! File checksums: CRC32, MD5, SHA-1, SHA-256 in a single streaming pass.

use crate::Result;
use md5::Md5;
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct ChecksumSet {
    pub size: u64,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

/// Compute all four checksums of a file in one streaming pass (64 KiB buffer),
/// so even multi-GB files only read once and never fully load into memory.
pub fn checksum_file(path: impl AsRef<Path>) -> Result<ChecksumSet> {
    let mut f = File::open(path.as_ref())?;
    let mut crc = crc32fast::Hasher::new();
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        crc.update(chunk);
        md5.update(chunk);
        sha1.update(chunk);
        sha256.update(chunk);
        total += n as u64;
    }
    Ok(ChecksumSet {
        size: total,
        crc32: format!("{:08x}", crc.finalize()),
        md5: hex(&md5.finalize()),
        sha1: hex(&sha1.finalize()),
        sha256: hex(&sha256.finalize()),
    })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn known_vectors_for_abc() {
        // "abc" — canonical test vectors.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"abc").unwrap();
        let c = checksum_file(f.path()).unwrap();
        assert_eq!(c.size, 3);
        assert_eq!(c.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(c.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            c.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(c.crc32, "352441c2");
    }

    #[test]
    fn empty_file() {
        let f = NamedTempFile::new().unwrap();
        let c = checksum_file(f.path()).unwrap();
        assert_eq!(c.size, 0);
        assert_eq!(c.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            c.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
