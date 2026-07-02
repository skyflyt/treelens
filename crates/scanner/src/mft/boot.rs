//! Volume bootstrap: `FSCTL_GET_NTFS_VOLUME_DATA` + reading `$MFT`'s own data
//! runs out of FILE record 0, so we know where the rest of the MFT lives on
//! disk before we can stream it.

use super::record::{apply_fixups_and_parse_header, parse_attributes, ATTR_DATA};
use super::runs::{decode_runs, Run};
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("failed to open volume handle: {0}")]
    OpenVolume(String),
    #[error("FSCTL_GET_NTFS_VOLUME_DATA failed: {0}")]
    VolumeData(String),
    #[error("failed to read $MFT record 0: {0}")]
    ReadMftRecord0(String),
    #[error("record parse error: {0}")]
    Record(#[from] super::record::RecordError),
    #[error("$MFT has no $DATA runs (unexpected)")]
    NoDataRuns,
}

/// Everything we need to start streaming FILE records.
#[derive(Debug, Clone)]
pub struct VolumeLayout {
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record: u32,
    pub mft_valid_data_length: u64,
    /// Data runs of `$MFT` itself, resolved to absolute byte ranges on the
    /// volume (offset in bytes, length in bytes), in file-offset order.
    pub mft_byte_runs: Vec<(u64, u64)>,
    pub total_bytes: u64,
}
impl VolumeLayout {
    /// Total number of FILE record slots implied by the MFT valid data length.
    pub fn total_record_slots(&self) -> u64 {
        if self.bytes_per_file_record == 0 {
            0
        } else {
            self.mft_valid_data_length / self.bytes_per_file_record as u64
        }
    }

    /// Total volume capacity in bytes (diagnostic / perf-log use, e.g. to
    /// report scan throughput as a fraction of the volume size).
    pub fn total_capacity_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// True if every discovered MFT byte run starts on a cluster boundary --
    /// a basic sanity check that the LCN-to-byte-offset math used
    /// bytes_per_cluster consistently.
    pub fn runs_are_cluster_aligned(&self) -> bool {
        let bpc = self.bytes_per_cluster as u64;
        bpc != 0 && self.mft_byte_runs.iter().all(|(off, _)| off % bpc == 0)
    }
}

/// Convert volume-relative data runs (in clusters) into absolute byte ranges,
/// skipping sparse runs (which should not occur for `$MFT` itself but we
/// tolerate them defensively).
pub fn runs_to_byte_ranges(runs: &[Run], bytes_per_cluster: u64) -> Vec<(u64, u64)> {
    runs.iter()
        .filter_map(|r| {
            r.lcn.map(|lcn| {
                let offset = (lcn as u64).saturating_mul(bytes_per_cluster);
                let length = r.length.saturating_mul(bytes_per_cluster);
                (offset, length)
            })
        })
        .collect()
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Open `\\.\X:` for raw sequential reads.
fn open_volume_handle(drive_letter: char) -> Result<HANDLE, BootError> {
    let path = format!(r"\\.\{drive_letter}:");
    let wpath = to_wide(&path);
    unsafe {
        CreateFileW(
            PCWSTR(wpath.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| BootError::OpenVolume(e.to_string()))
    }
}

/// Drive letter from a root path like `C:\` -- caller has already checked
/// this is a volume root before calling into the MFT path.
pub fn drive_letter_of(root: &Path) -> Option<char> {
    let s = root.to_string_lossy();
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next() == Some(':') && c.is_ascii_alphabetic() {
        Some(c.to_ascii_uppercase())
    } else {
        None
    }
}

/// Query `FSCTL_GET_NTFS_VOLUME_DATA`, then bootstrap by reading FILE record 0
/// (`$MFT`'s own record) to discover where the rest of the MFT lives.
pub fn bootstrap(root: &Path) -> Result<VolumeLayout, BootError> {
    let letter = drive_letter_of(root).ok_or_else(|| {
        BootError::OpenVolume(format!("not a drive-letter root: {}", root.display()))
    })?;
    let handle = open_volume_handle(letter)?;
    let result = bootstrap_with_handle(handle);
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

fn bootstrap_with_handle(handle: HANDLE) -> Result<VolumeLayout, BootError> {
    let mut vdata = NTFS_VOLUME_DATA_BUFFER::default();
    let mut bytes_returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&mut vdata as *mut _ as *mut _),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut bytes_returned),
            None,
        )
        .map_err(|e| BootError::VolumeData(e.to_string()))?;
    }

    let bytes_per_cluster = vdata.BytesPerCluster;
    let bytes_per_file_record = vdata.BytesPerFileRecordSegment;
    let mft_start_lcn = vdata.MftStartLcn;
    let total_bytes = (vdata.NumberSectors * vdata.BytesPerSector as i64).max(0) as u64;

    // Read the first `bytes_per_file_record` bytes of the MFT (FILE record 0,
    // the $MFT's own record) directly from its starting cluster -- we don't
    // yet know the full run list (that's IN this record), but record 0 is
    // always resident at the MFT's first cluster by construction.
    let record0_offset = (mft_start_lcn as u64).saturating_mul(bytes_per_cluster as u64);
    let mut buf = vec![0u8; bytes_per_file_record as usize];
    read_at(handle, record0_offset, &mut buf)
        .map_err(|e| BootError::ReadMftRecord0(e.to_string()))?;

    let header = apply_fixups_and_parse_header(&mut buf)?;
    let parsed = parse_attributes(&buf, header.first_attr_offset as usize)?;

    // We need the raw $DATA attribute's mapping pairs, which `parse_attributes`
    // doesn't retain (it only keeps sizes). Re-scan directly for the $DATA
    // attribute's non-resident mapping-pairs bytes.
    let mapping_pairs = find_data_mapping_pairs(&buf, header.first_attr_offset as usize)
        .ok_or(BootError::NoDataRuns)?;
    let runs = decode_runs(mapping_pairs).map_err(|_| BootError::NoDataRuns)?;
    // Cross-check: the run decoder's own cluster total, converted to bytes,
    // should be at least the $DATA attribute's reported allocated size --
    // catches a mis-decoded run list early (before we trust it for the whole
    // volume) rather than silently reading the wrong byte ranges.
    if let Some(cluster_total) = super::record::data_runs_cluster_total(mapping_pairs) {
        let decoded_bytes = cluster_total.saturating_mul(bytes_per_cluster as u64);
        if decoded_bytes < parsed.allocated_size {
            return Err(BootError::NoDataRuns);
        }
    }
    let mft_byte_runs = runs_to_byte_ranges(&runs, bytes_per_cluster as u64);
    if mft_byte_runs.is_empty() {
        return Err(BootError::NoDataRuns);
    }

    // mft_valid_data_length is only reported by GetNtfsVolumeData sometimes;
    // fall back to the $DATA attribute's real_size from record 0 if the volume
    // struct doesn't carry a usable value.
    let mft_valid_data_length = if vdata.MftValidDataLength > 0 {
        vdata.MftValidDataLength as u64
    } else {
        parsed.logical_size
    };

    Ok(VolumeLayout {
        bytes_per_cluster,
        bytes_per_file_record,
        mft_valid_data_length,
        mft_byte_runs,
        total_bytes,
    })
}

/// Re-walk the attribute list of a raw record buffer looking for the unnamed
/// `$DATA` attribute's mapping-pairs bytes (non-resident only -- `$MFT`'s
/// $DATA is always non-resident on any real volume).
fn find_data_mapping_pairs(buf: &[u8], first_attr_offset: usize) -> Option<&[u8]> {
    let mut off = first_attr_offset;
    loop {
        if off + 4 > buf.len() {
            return None;
        }
        let type_code = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        if type_code == 0xFFFF_FFFF {
            return None;
        }
        if off + 16 > buf.len() {
            return None;
        }
        let attr_len =
            u32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]) as usize;
        if attr_len == 0 || off + attr_len > buf.len() {
            return None;
        }
        let non_resident = buf[off + 8] != 0;
        let name_len_chars = buf[off + 9];
        if type_code == ATTR_DATA && non_resident && name_len_chars == 0 {
            if off + 0x22 > buf.len() {
                return None;
            }
            let mp_offset = u16::from_le_bytes([buf[off + 0x20], buf[off + 0x21]]) as usize;
            let body_start = off + mp_offset;
            if body_start > off + attr_len || body_start > buf.len() {
                return None;
            }
            return Some(&buf[body_start..off + attr_len]);
        }
        off += attr_len;
    }
}

fn read_at(handle: HANDLE, offset: u64, buf: &mut [u8]) -> Result<(), String> {
    unsafe {
        SetFilePointerEx(handle, offset as i64, None, FILE_BEGIN)
            .map_err(|e| format!("seek: {e}"))?;
        let mut read: u32 = 0;
        ReadFile(handle, Some(buf), Some(&mut read), None).map_err(|e| format!("read: {e}"))?;
        if (read as usize) < buf.len() {
            return Err(format!("short read: got {read} of {} bytes", buf.len()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_to_byte_ranges_converts_and_skips_sparse() {
        let runs = vec![
            Run {
                lcn: Some(10),
                length: 2,
            },
            Run {
                lcn: None,
                length: 5,
            },
            Run {
                lcn: Some(20),
                length: 1,
            },
        ];
        let ranges = runs_to_byte_ranges(&runs, 4096);
        assert_eq!(ranges, vec![(10 * 4096, 2 * 4096), (20 * 4096, 4096)]);
    }

    #[test]
    fn drive_letter_extraction() {
        assert_eq!(drive_letter_of(Path::new(r"C:\")), Some('C'));
        assert_eq!(drive_letter_of(Path::new(r"d:\")), Some('D'));
        assert_eq!(drive_letter_of(Path::new(r"C:\Users\foo")), Some('C'));
        assert_eq!(drive_letter_of(Path::new(r"\\server\share")), None);
    }

    #[test]
    fn volume_layout_helper_accessors() {
        let layout = VolumeLayout {
            bytes_per_cluster: 4096,
            bytes_per_file_record: 1024,
            mft_valid_data_length: 1024 * 100,
            mft_byte_runs: vec![(4096 * 10, 4096 * 5), (4096 * 20, 4096 * 2)],
            total_bytes: 1_000_000_000,
        };
        assert_eq!(layout.total_record_slots(), 100);
        assert_eq!(layout.total_capacity_bytes(), 1_000_000_000);
        assert!(layout.runs_are_cluster_aligned());

        let misaligned = VolumeLayout {
            mft_byte_runs: vec![(4097, 4096)],
            ..layout
        };
        assert!(!misaligned.runs_are_cluster_aligned());
    }
}
