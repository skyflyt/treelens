//! Treelens analysis: checksums, file comparison, and a local steganography
//! analysis toolkit (LSB / whitespace-SNOW / format-based).
//!
//! Intended use is **forensic and educational analysis of files you own**:
//! detect and reverse data hidden inside local files, verify integrity via
//! checksums, and diff files. The embed side exists for round-trip testing and
//! watermarking your own files — it operates only on local paths the user
//! selects, never on a network and never on third-party data.

pub mod checksums;
pub mod compare;
pub mod stego;

// Re-exported so consumers (e.g. the app's --selftest) can synthesize test
// images without taking their own `image` dependency.
pub use image;

pub use checksums::{checksum_file, ChecksumSet};
pub use compare::{compare_files, CompareResult};

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("image: {0}")]
    Image(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no hidden data found")]
    NotFound,
    #[error("capacity exceeded: payload {payload} bytes > capacity {capacity} bytes")]
    Capacity { payload: u64, capacity: u64 },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AnalysisError>;

/// Human-readable byte formatting shared by analysis result messages.
pub fn human_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Verdict {
    /// True if the detector thinks hidden data is present.
    pub suspicious: bool,
    /// 0.0–1.0 confidence.
    pub confidence: f32,
}
