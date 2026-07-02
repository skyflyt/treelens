//! NTFS data-run decoder.
//!
//! A non-resident attribute stores its extents as a sequence of "runs":
//! `(run-length-in-clusters, LCN-delta-from-previous-run)` pairs, packed in a
//! compact variable-length encoding. This is pure byte-slice parsing with no
//! Win32 dependency, so it's fully unit-testable without a real volume.
//!
//! Encoding (per run):
//! - One header byte: low nibble = byte-length of the run-length field, high
//!   nibble = byte-length of the LCN-offset field.
//! - Then `length_size` bytes: run length in clusters, little-endian, unsigned.
//! - Then `offset_size` bytes: LCN delta from the previous run's LCN, little-endian,
//!   **signed** (two's-complement, sign-extended from the last byte present).
//!   `offset_size == 0` marks a *sparse* run (no LCN -- logically zero-filled).
//! - A header byte of `0x00` terminates the run list.
//!
//! The absolute LCN of a run is the previous non-sparse run's LCN plus the
//! delta (deltas can be negative when the file is fragmented backwards on
//! disk); sparse runs don't affect the running LCN.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Starting LCN (logical cluster number) of this run, or `None` if sparse.
    pub lcn: Option<i64>,
    /// Length of this run, in clusters.
    pub length: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RunError {
    #[error("truncated data-run stream")]
    Truncated,
    #[error("run length/offset field too wide (>8 bytes)")]
    FieldTooWide,
    #[error("zero-length run")]
    ZeroLength,
}

/// Decode a data-run byte stream into a list of runs.
///
/// `data` is the raw bytes of a non-resident attribute's "mapping pairs"
/// array (i.e. the bytes starting at the attribute's `data_runs_offset`).
pub fn decode_runs(data: &[u8]) -> Result<Vec<Run>, RunError> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut current_lcn: i64 = 0;

    while pos < data.len() {
        let header = data[pos];
        if header == 0 {
            break; // terminator
        }
        pos += 1;

        let length_size = (header & 0x0F) as usize;
        let offset_size = ((header >> 4) & 0x0F) as usize;
        if length_size > 8 || offset_size > 8 {
            return Err(RunError::FieldTooWide);
        }

        if pos + length_size > data.len() {
            return Err(RunError::Truncated);
        }
        let length = read_unsigned_le(&data[pos..pos + length_size]);
        pos += length_size;
        if length == 0 {
            return Err(RunError::ZeroLength);
        }

        if offset_size == 0 {
            // Sparse run: no LCN field, logically zero-filled, running LCN unchanged.
            runs.push(Run { lcn: None, length });
            continue;
        }

        if pos + offset_size > data.len() {
            return Err(RunError::Truncated);
        }
        let delta = read_signed_le(&data[pos..pos + offset_size]);
        pos += offset_size;

        current_lcn = current_lcn.wrapping_add(delta);
        runs.push(Run {
            lcn: Some(current_lcn),
            length,
        });
    }

    Ok(runs)
}

/// Total length in clusters across all runs (sparse included), useful for
/// sanity-checking against an attribute's allocated size.
pub fn total_clusters(runs: &[Run]) -> u64 {
    runs.iter().map(|r| r.length).sum()
}

fn read_unsigned_le(bytes: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as u64) << (8 * i);
    }
    v
}

/// Read a little-endian two's-complement signed integer of `bytes.len()`
/// bytes (1..=8), sign-extended to i64 based on the high bit of the most
/// significant byte present.
fn read_signed_le(bytes: &[u8]) -> i64 {
    debug_assert!(!bytes.is_empty() && bytes.len() <= 8);
    let mut v: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as u64) << (8 * i);
    }
    let bits = bytes.len() * 8;
    if bits < 64 {
        let sign_bit = 1u64 << (bits - 1);
        if v & sign_bit != 0 {
            // Sign-extend: set all higher bits.
            v |= !0u64 << bits;
        }
    }
    v as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_run_positive_offset() {
        // header 0x31: length_size=1, offset_size=3. length=0x10, offset=0x001234
        let data = [0x31, 0x10, 0x34, 0x12, 0x00, 0x00];
        let runs = decode_runs(&data).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].length, 0x10);
        assert_eq!(runs[0].lcn, Some(0x001234));
    }

    #[test]
    fn multiple_runs_running_lcn() {
        // Run 1: header 0x21, length_size=1, offset_size=2 -> length=5, delta=100 => lcn=100
        // Run 2: header 0x21, length=3, delta=-20 (two's complement 2 bytes: 0xEC 0xFF) => lcn=80
        let mut data = vec![0x21, 0x05];
        data.extend_from_slice(&100i16.to_le_bytes());
        data.push(0x21);
        data.push(0x03);
        data.extend_from_slice(&(-20i16).to_le_bytes());
        data.push(0x00); // terminator

        let runs = decode_runs(&data).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].lcn, Some(100));
        assert_eq!(runs[0].length, 5);
        assert_eq!(runs[1].lcn, Some(80));
        assert_eq!(runs[1].length, 3);
    }

    #[test]
    fn negative_offset_single_byte() {
        // header 0x11: length_size=1, offset_size=1. length=2, offset=-5 (0xFB).
        let data = [0x11, 0x02, 0xFB, 0x00];
        let runs = decode_runs(&data).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lcn, Some(-5));
    }

    #[test]
    fn sparse_run_has_no_lcn_and_does_not_shift_running_lcn() {
        // Run 1: real, lcn=50. Run 2: sparse (offset_size=0), length=10.
        // Run 3: real, delta=+5 relative to the running lcn (50, unaffected by the sparse run) => lcn=55.
        let mut data = vec![0x11, 0x04];
        data.extend_from_slice(&[50u8]); // offset_size=1, +50
        data.push(0x02); // header: length_size=2, offset_size=0 (sparse)
        data.extend_from_slice(&10u16.to_le_bytes());
        data.push(0x11);
        data.push(0x03);
        data.extend_from_slice(&[5u8]); // +5 from running lcn (50) => 55
        data.push(0x00);

        let runs = decode_runs(&data).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].lcn, Some(50));
        assert_eq!(runs[1].lcn, None);
        assert_eq!(runs[1].length, 10);
        assert_eq!(runs[2].lcn, Some(55));
        assert_eq!(total_clusters(&runs), 4 + 10 + 3);
    }

    #[test]
    fn empty_stream_yields_no_runs() {
        let runs = decode_runs(&[]).unwrap();
        assert!(runs.is_empty());
        let runs = decode_runs(&[0x00]).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn truncated_length_field_errors() {
        // header claims length_size=4 but only 1 byte follows.
        let data = [0x04, 0x01];
        assert_eq!(decode_runs(&data), Err(RunError::Truncated));
    }

    #[test]
    fn truncated_offset_field_errors() {
        let data = [0x21, 0x05, 0x00]; // offset_size=2 but only 1 byte follows
        assert_eq!(decode_runs(&data), Err(RunError::Truncated));
    }

    #[test]
    fn zero_length_run_errors() {
        let data = [0x11, 0x00, 0x05];
        assert_eq!(decode_runs(&data), Err(RunError::ZeroLength));
    }

    #[test]
    fn field_too_wide_errors() {
        let data = [0x0F + 0x10, 0x00];
        // length_size = 15 -> invalid (> 8)
        assert_eq!(decode_runs(&data), Err(RunError::FieldTooWide));
    }

    #[test]
    fn eight_byte_offset_sign_extends_correctly() {
        // header 0x81: length_size=1, offset_size=8. offset = -1 (all 0xFF).
        let mut data = vec![0x81, 0x01];
        data.extend_from_slice(&(-1i64).to_le_bytes());
        data.push(0x00);
        let runs = decode_runs(&data).unwrap();
        assert_eq!(runs[0].lcn, Some(-1));
    }
}
