//! NTFS FILE record parsing: header, USA fixups, and attribute extraction.
//!
//! Every function here is a pure function over byte slices (no Win32, no
//! disk I/O) so the parser is fully exercisable from unit tests using
//! hand-built synthetic FILE-record fixtures.

use super::runs::decode_runs;
use std::collections::HashMap;

pub const FILE_RECORD_MAGIC: [u8; 4] = *b"FILE";

/// $STANDARD_INFORMATION attribute type code.
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
/// $ATTRIBUTE_LIST attribute type code.
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
/// $FILE_NAME attribute type code.
pub const ATTR_FILE_NAME: u32 = 0x30;
/// $DATA attribute type code.
pub const ATTR_DATA: u32 = 0x80;
/// End-of-attributes marker.
pub const ATTR_END: u32 = 0xFFFFFFFF;

/// FILE record header flags (from the 16-bit `flags` field).
pub const RECORD_FLAG_IN_USE: u16 = 0x0001;
pub const RECORD_FLAG_DIRECTORY: u16 = 0x0002;

/// $FILE_NAME namespace values (byte 0x41 of the $FILE_NAME attribute body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Namespace {
    Posix = 0,
    Win32 = 1,
    Dos = 2,
    Win32AndDos = 3,
}

impl Namespace {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Namespace::Posix),
            1 => Some(Namespace::Win32),
            2 => Some(Namespace::Dos),
            3 => Some(Namespace::Win32AndDos),
            _ => None,
        }
    }

    /// True if this namespace is preferred over a plain POSIX or DOS-only name
    /// (i.e. it's Win32 or the combined Win32+DOS form).
    fn is_win32_like(self) -> bool {
        matches!(self, Namespace::Win32 | Namespace::Win32AndDos)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("record too short")]
    TooShort,
    #[error("bad magic (not a FILE record)")]
    BadMagic,
    #[error("USA fixup mismatch at sector {0} (corrupt record)")]
    FixupMismatch(usize),
    #[error("USA extends past record bounds")]
    FixupOutOfBounds,
    #[error("attribute at offset {0} extends past record bounds")]
    AttrOutOfBounds(usize),
}

/// Parsed FILE record header fields we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub flags: u16,
    pub base_record_ref: u64, // FRN of the base record (0 if this IS the base record)
    pub first_attr_offset: u16,
    pub bytes_in_use: u32,
}

impl RecordHeader {
    pub fn is_in_use(&self) -> bool {
        self.flags & RECORD_FLAG_IN_USE != 0
    }
    pub fn is_directory(&self) -> bool {
        self.flags & RECORD_FLAG_DIRECTORY != 0
    }
    /// True if this record is an *extension* record (its attributes belong to
    /// a different base FILE record), i.e. `base_record_ref`'s low 48 bits != 0.
    pub fn is_extension_record(&self) -> bool {
        (self.base_record_ref & 0x0000_FFFF_FFFF_FFFF) != 0
    }
}

/// Apply the Update Sequence Array (USA) fixup in place and parse the header.
///
/// NTFS protects each on-disk sector of a FILE record with a 2-byte "update
/// sequence number" (USN) that overwrites the sector's real last 2 bytes;
/// the real bytes are stashed in the USA array right after the record header.
/// Before reading anything else we must: verify each sector's last 2 bytes
/// equal the USN (else the record is torn/corrupt), then replace them with
/// the stashed real bytes. This function mutates `buf` in place and returns
/// the parsed header.
pub fn apply_fixups_and_parse_header(buf: &mut [u8]) -> Result<RecordHeader, RecordError> {
    if buf.len() < 48 {
        return Err(RecordError::TooShort);
    }
    if buf[0..4] != FILE_RECORD_MAGIC {
        return Err(RecordError::BadMagic);
    }
    let usa_offset = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let usa_count = u16::from_le_bytes([buf[6], buf[7]]) as usize; // includes the USN itself
    let bytes_in_use = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let first_attr_offset = u16::from_le_bytes([buf[20], buf[21]]);
    let flags = u16::from_le_bytes([buf[22], buf[23]]);
    let base_record_ref = u64::from_le_bytes([
        buf[32], buf[33], buf[34], buf[35], buf[36], buf[37], buf[38], buf[39],
    ]);

    if usa_count > 0 {
        if usa_offset + usa_count * 2 > buf.len() {
            return Err(RecordError::FixupOutOfBounds);
        }
        let usn = [buf[usa_offset], buf[usa_offset + 1]];
        // usa_count includes the USN slot itself; the remaining (usa_count-1)
        // entries are the real bytes for sectors 1..=usa_count-1.
        let sector_size = 512usize;
        for i in 0..usa_count.saturating_sub(1) {
            let sector_end = (i + 1) * sector_size;
            if sector_end > buf.len() {
                break; // record shorter than a full sector array entry; tolerate (e.g. tests)
            }
            let check_off = sector_end - 2;
            if buf[check_off] != usn[0] || buf[check_off + 1] != usn[1] {
                return Err(RecordError::FixupMismatch(i));
            }
            let real_off = usa_offset + 2 + i * 2;
            buf[check_off] = buf[real_off];
            buf[check_off + 1] = buf[real_off + 1];
        }
    }

    Ok(RecordHeader {
        flags,
        base_record_ref,
        first_attr_offset,
        bytes_in_use,
    })
}

/// One raw attribute located in a FILE record.
struct RawAttr<'a> {
    type_code: u32,
    non_resident: bool,
    body: &'a [u8], // resident: the value bytes; non-resident: the mapping-pairs bytes
    // Non-resident-only fields:
    allocated_size: u64,
    real_size: u64,
    name_len_chars: u8,
}

/// Iterate the attributes of a (fixed-up) FILE record buffer, starting at
/// `first_attr_offset`, calling `f` for each. Stops at the 0xFFFFFFFF end
/// marker or the buffer bound.
fn for_each_attr<'a>(
    buf: &'a [u8],
    first_attr_offset: usize,
    mut f: impl FnMut(RawAttr<'a>) -> Result<(), RecordError>,
) -> Result<(), RecordError> {
    let mut off = first_attr_offset;
    loop {
        if off + 4 > buf.len() {
            break;
        }
        let type_code = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        if type_code == ATTR_END {
            break;
        }
        if off + 16 > buf.len() {
            return Err(RecordError::AttrOutOfBounds(off));
        }
        let attr_len =
            u32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]) as usize;
        if attr_len == 0 || off + attr_len > buf.len() {
            return Err(RecordError::AttrOutOfBounds(off));
        }
        let non_resident = buf[off + 8] != 0;
        let name_len_chars = buf[off + 9];

        if non_resident {
            // Non-resident header layout (offsets relative to attr start):
            // 0x10 starting VCN (u64), 0x18 ending VCN (u64), 0x20 mapping-pairs offset (u16),
            // 0x28 allocated size (u64), 0x30 real size (u64), 0x38 initialized size (u64).
            if off + 0x30 + 8 > buf.len() {
                return Err(RecordError::AttrOutOfBounds(off));
            }
            let mp_offset = u16::from_le_bytes([buf[off + 0x20], buf[off + 0x21]]) as usize;
            let allocated_size =
                u64::from_le_bytes(buf[off + 0x28..off + 0x30].try_into().unwrap());
            let real_size = u64::from_le_bytes(buf[off + 0x30..off + 0x38].try_into().unwrap());
            let body_start = off + mp_offset;
            let body = if body_start <= off + attr_len && body_start <= buf.len() {
                &buf[body_start..off + attr_len]
            } else {
                &[]
            };
            f(RawAttr {
                type_code,
                non_resident: true,
                body,
                allocated_size,
                real_size,
                name_len_chars,
            })?;
        } else {
            // Resident header: 0x10 value length (u32), 0x14 value offset (u16).
            if off + 0x18 > buf.len() {
                return Err(RecordError::AttrOutOfBounds(off));
            }
            let value_len = u32::from_le_bytes([
                buf[off + 0x10],
                buf[off + 0x11],
                buf[off + 0x12],
                buf[off + 0x13],
            ]) as usize;
            let value_off = u16::from_le_bytes([buf[off + 0x14], buf[off + 0x15]]) as usize;
            let body_start = off + value_off;
            let body_end = body_start + value_len;
            if body_end > buf.len() || body_start > body_end {
                return Err(RecordError::AttrOutOfBounds(off));
            }
            f(RawAttr {
                type_code,
                non_resident: false,
                body: &buf[body_start..body_end],
                allocated_size: value_len as u64,
                real_size: value_len as u64,
                name_len_chars,
            })?;
        }

        off += attr_len;
    }
    Ok(())
}

/// A parsed `$FILE_NAME` attribute.
#[derive(Debug, Clone)]
pub struct FileNameAttr {
    pub parent_frn: u64, // MFT record number of the parent (low 48 bits already masked)
    pub name: String,
    pub namespace: Namespace,
}

/// Everything extracted from a single base FILE record (after merging any
/// $ATTRIBUTE_LIST extension records the caller supplies).
#[derive(Debug, Clone, Default)]
pub struct ParsedRecord {
    pub mtime_100ns: Option<i64>, // Windows FILETIME (100ns ticks since 1601), from $STANDARD_INFORMATION
    pub file_names: Vec<FileNameAttr>,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub has_data_attr: bool,
    /// Names of $ATTRIBUTE_LIST-referenced attribute types not resolvable
    /// from the supplied extension records (signal for per-file stat fallback).
    pub unresolved_attribute_list: bool,
}

/// Parse the attributes of a single (already fixed-up) FILE record buffer.
/// `extra_data_attrs` and `extra_names`, when supplied by the caller after
/// resolving an `$ATTRIBUTE_LIST`, are merged in (used by `mod.rs`).
pub fn parse_attributes(buf: &[u8], first_attr_offset: usize) -> Result<ParsedRecord, RecordError> {
    let mut out = ParsedRecord::default();

    for_each_attr(buf, first_attr_offset, |attr| {
        match attr.type_code {
            ATTR_STANDARD_INFORMATION => {
                if !attr.non_resident && attr.body.len() >= 16 {
                    // Layout: creation time (u64) @0, then modified time (u64) @8.
                    let mtime = i64::from_le_bytes(attr.body[8..16].try_into().unwrap());
                    out.mtime_100ns = Some(mtime);
                }
            }
            ATTR_FILE_NAME => {
                if !attr.non_resident {
                    if let Some(fna) = parse_file_name_attr(attr.body) {
                        out.file_names.push(fna);
                    }
                }
            }
            ATTR_DATA => {
                // Only the unnamed stream (name_len_chars == 0) is "the" file data;
                // named streams (ADS) are out of scope for v0.7 (matches the walk).
                if attr.name_len_chars == 0 {
                    out.has_data_attr = true;
                    if attr.non_resident {
                        out.logical_size = attr.real_size;
                        out.allocated_size = attr.allocated_size;
                    } else {
                        // Resident data: it lives inside the MFT record itself; both
                        // logical and allocated size equal the resident byte count.
                        out.logical_size = attr.body.len() as u64;
                        out.allocated_size = attr.body.len() as u64;
                    }
                }
            }
            ATTR_ATTRIBUTE_LIST => {
                out.unresolved_attribute_list = true;
            }
            _ => {}
        }
        Ok(())
    })?;

    Ok(out)
}

fn parse_file_name_attr(body: &[u8]) -> Option<FileNameAttr> {
    // $FILE_NAME body layout:
    // 0x00 parent directory reference (u64, low 48 bits = FRN)
    // 0x08 creation time, 0x10 modified time, 0x18 MFT-modified, 0x20 access time
    // 0x28 allocated size, 0x30 real size, 0x38 flags (u32), 0x3C reparse tag (u32)
    // 0x40 name length in UTF-16 chars (u8), 0x41 namespace (u8), 0x42.. name (UTF-16LE)
    if body.len() < 0x42 {
        return None;
    }
    let parent_ref = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let parent_frn = parent_ref & 0x0000_FFFF_FFFF_FFFF;
    let name_len_chars = body[0x40] as usize;
    let namespace = Namespace::from_u8(body[0x41])?;
    let name_bytes_start = 0x42;
    let name_bytes_end = name_bytes_start + name_len_chars * 2;
    if name_bytes_end > body.len() {
        return None;
    }
    let utf16: Vec<u16> = body[name_bytes_start..name_bytes_end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&utf16);
    Some(FileNameAttr {
        parent_frn,
        name,
        namespace,
    })
}

/// Pick the single "canonical" $FILE_NAME to represent a record, matching the
/// directory-walk's semantics: prefer a Win32 (or combined Win32+DOS) name;
/// among equally-preferred names, the first one encountered wins (stable,
/// deterministic, and cheap -- exact hard-link tie-break order has no
/// user-visible effect since Explorer/the walk also just show "a" name).
/// Pure DOS-only (namespace 2) short names are ignored whenever at least one
/// Win32-like name exists, matching "ignore DOS-only duplicates" in the spec.
pub fn pick_canonical_name(names: &[FileNameAttr]) -> Option<&FileNameAttr> {
    if names.is_empty() {
        return None;
    }
    let any_win32 = names.iter().any(|n| n.namespace.is_win32_like());
    names.iter().find(|n| {
        if any_win32 {
            n.namespace.is_win32_like()
        } else {
            true // no Win32 name at all (rare) -- fall back to whatever exists (POSIX or DOS)
        }
    })
}

/// Decode the logical/allocated size of a non-resident `$DATA` mapping-pairs
/// blob via the run decoder, for cross-checking against the header-reported
/// allocated size (used opportunistically; the header value is authoritative
/// per the spec, this is available for callers that want a sanity check).
pub fn data_runs_cluster_total(mapping_pairs: &[u8]) -> Option<u64> {
    decode_runs(mapping_pairs)
        .ok()
        .map(|runs| super::runs::total_clusters(&runs))
}

/// Hard-link dedup: given all (base FRN, canonical name) pairs seen across a
/// scan, keep only the first FRN->name mapping per base FRN. Returns a map of
/// FRN -> chosen (parent_frn, name). This directly implements "count the file
/// once (first Win32 link wins)".
pub fn dedup_hardlinks<'a>(
    entries: impl IntoIterator<Item = (u64, &'a FileNameAttr)>,
) -> HashMap<u64, (u64, String)> {
    let mut chosen: HashMap<u64, (u64, String, Namespace)> = HashMap::new();
    for (frn, fna) in entries {
        match chosen.get(&frn) {
            None => {
                chosen.insert(frn, (fna.parent_frn, fna.name.clone(), fna.namespace));
            }
            Some((_, _, existing_ns)) => {
                // Upgrade only if the existing pick wasn't Win32-like and this one is
                // (keeps "first Win32 link wins" even if a DOS-only name for the same
                // FRN happened to be enumerated first).
                if !existing_ns.is_win32_like() && fna.namespace.is_win32_like() {
                    chosen.insert(frn, (fna.parent_frn, fna.name.clone(), fna.namespace));
                }
            }
        }
    }
    chosen
        .into_iter()
        .map(|(frn, (parent, name, _))| (frn, (parent, name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal, valid 1024-byte FILE record buffer with a real USA
    /// fixup (2 sectors of 512 bytes) and the given attributes appended
    /// starting at `first_attr_offset`. `attrs_bytes` should already include
    /// the 0xFFFFFFFF end marker.
    fn build_record(flags: u16, base_record_ref: u64, attrs_bytes: &[u8]) -> Vec<u8> {
        let record_size = 1024usize;
        let mut buf = vec![0u8; record_size];
        buf[0..4].copy_from_slice(&FILE_RECORD_MAGIC);
        let usa_offset: u16 = 48; // right after the header fields we use
        let usa_count: u16 = 3; // USN + 2 sector-fixup entries (1024/512 = 2 sectors)
        buf[4..6].copy_from_slice(&usa_offset.to_le_bytes());
        buf[6..8].copy_from_slice(&usa_count.to_le_bytes());
        let first_attr_offset: u16 = 56; // usa_offset(48) + usa_count*2(6) = 54, round to 56
        buf[20..22].copy_from_slice(&first_attr_offset.to_le_bytes());
        buf[22..24].copy_from_slice(&flags.to_le_bytes());
        let bytes_in_use = (first_attr_offset as usize + attrs_bytes.len()) as u32;
        buf[24..28].copy_from_slice(&bytes_in_use.to_le_bytes());
        buf[32..40].copy_from_slice(&base_record_ref.to_le_bytes());

        // USN value used for both sector-end fixups.
        let usn: [u8; 2] = [0xAB, 0xCD];
        buf[usa_offset as usize..usa_offset as usize + 2].copy_from_slice(&usn);
        // Real bytes to restore at each sector's last 2 bytes (arbitrary sentinel).
        let real_sector1: [u8; 2] = [0x11, 0x22];
        let real_sector2: [u8; 2] = [0x33, 0x44];
        buf[usa_offset as usize + 2..usa_offset as usize + 4].copy_from_slice(&real_sector1);
        buf[usa_offset as usize + 4..usa_offset as usize + 6].copy_from_slice(&real_sector2);
        // Plant the USN at each sector's last 2 bytes (simulating on-disk state).
        buf[510..512].copy_from_slice(&usn);
        buf[1022..1024].copy_from_slice(&usn);

        // Place attribute bytes at first_attr_offset.
        let start = first_attr_offset as usize;
        buf[start..start + attrs_bytes.len()].copy_from_slice(attrs_bytes);

        buf
    }

    fn resident_attr_header(type_code: u32, value: &[u8]) -> Vec<u8> {
        let value_off: u16 = 0x18;
        let attr_len = (value_off as usize + value.len()).next_multiple_of(8) as u32;
        let mut a = vec![0u8; attr_len as usize];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&attr_len.to_le_bytes());
        a[8] = 0; // resident
        a[9] = 0; // name_len_chars
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&value_off.to_le_bytes());
        a[value_off as usize..value_off as usize + value.len()].copy_from_slice(value);
        a
    }

    fn end_marker() -> [u8; 4] {
        ATTR_END.to_le_bytes()
    }

    fn standard_information_value(mtime: i64) -> Vec<u8> {
        let mut v = vec![0u8; 0x30];
        v[0..8].copy_from_slice(&0i64.to_le_bytes()); // creation time
        v[8..16].copy_from_slice(&mtime.to_le_bytes()); // modified time
        v
    }

    fn file_name_value(parent_frn: u64, name: &str, namespace: u8) -> Vec<u8> {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + utf16.len() * 2];
        let parent_ref = parent_frn; // sequence number 0 for tests
        v[0..8].copy_from_slice(&parent_ref.to_le_bytes());
        v[0x40] = utf16.len() as u8;
        v[0x41] = namespace;
        for (i, c) in utf16.iter().enumerate() {
            v[0x42 + i * 2..0x42 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        v
    }

    #[test]
    fn fixup_restores_sector_end_bytes_and_parses_header() {
        let mut attrs = Vec::new();
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        assert!(header.is_in_use());
        assert!(!header.is_directory());
        // Sector-end bytes replaced with the stashed real bytes, not the USN.
        assert_eq!(&buf[510..512], &[0x11, 0x22]);
        assert_eq!(&buf[1022..1024], &[0x33, 0x44]);
    }

    #[test]
    fn fixup_mismatch_detected_when_usn_does_not_match() {
        let mut attrs = Vec::new();
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        // Corrupt sector 1's end bytes so they no longer match the USN.
        buf[510] = 0x00;
        buf[511] = 0x00;
        let err = apply_fixups_and_parse_header(&mut buf).unwrap_err();
        assert_eq!(err, RecordError::FixupMismatch(0));
    }

    #[test]
    fn parses_standard_information_mtime() {
        let mut attrs = Vec::new();
        attrs.extend(resident_attr_header(
            ATTR_STANDARD_INFORMATION,
            &standard_information_value(133_800_000_000_000),
        ));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        assert_eq!(parsed.mtime_100ns, Some(133_800_000_000_000));
    }

    #[test]
    fn parses_file_name_and_prefers_win32() {
        let mut attrs = Vec::new();
        attrs.extend(resident_attr_header(
            ATTR_FILE_NAME,
            &file_name_value(5, "LONGFILENAME~1", 2), // DOS
        ));
        attrs.extend(resident_attr_header(
            ATTR_FILE_NAME,
            &file_name_value(5, "LongFileName.txt", 1), // Win32
        ));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        assert_eq!(parsed.file_names.len(), 2);
        let canonical = pick_canonical_name(&parsed.file_names).unwrap();
        assert_eq!(canonical.name, "LongFileName.txt");
        assert_eq!(canonical.namespace, Namespace::Win32);
    }

    #[test]
    fn win32_and_dos_combined_namespace_is_preferred_form() {
        let mut attrs = Vec::new();
        attrs.extend(resident_attr_header(
            ATTR_FILE_NAME,
            &file_name_value(5, "short.txt", 3), // Win32+DOS combined (short name fits both)
        ));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        let canonical = pick_canonical_name(&parsed.file_names).unwrap();
        assert_eq!(canonical.name, "short.txt");
    }

    #[test]
    fn posix_only_name_used_when_no_win32_alternative() {
        let mut attrs = Vec::new();
        attrs.extend(resident_attr_header(
            ATTR_FILE_NAME,
            &file_name_value(5, "posixname", 0),
        ));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        let canonical = pick_canonical_name(&parsed.file_names).unwrap();
        assert_eq!(canonical.name, "posixname");
    }

    #[test]
    fn parses_resident_data_attribute_sizes() {
        let mut attrs = Vec::new();
        let data = vec![0xAAu8; 20];
        attrs.extend(resident_attr_header(ATTR_DATA, &data));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        assert!(parsed.has_data_attr);
        assert_eq!(parsed.logical_size, 20);
        assert_eq!(parsed.allocated_size, 20);
    }

    #[test]
    fn detects_attribute_list_presence() {
        let mut attrs = Vec::new();
        attrs.extend(resident_attr_header(ATTR_ATTRIBUTE_LIST, &[0u8; 8]));
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        let parsed = parse_attributes(&buf, header.first_attr_offset as usize).unwrap();
        assert!(parsed.unresolved_attribute_list);
    }

    #[test]
    fn hard_link_dedup_keeps_first_win32_and_counts_once() {
        let dos = FileNameAttr {
            parent_frn: 10,
            name: "OLDNAME~1".into(),
            namespace: Namespace::Dos,
        };
        let win32_a = FileNameAttr {
            parent_frn: 10,
            name: "First Link.txt".into(),
            namespace: Namespace::Win32,
        };
        let win32_b = FileNameAttr {
            parent_frn: 20,
            name: "Second Link.txt".into(),
            namespace: Namespace::Win32,
        };
        let frn = 99u64;
        let entries = vec![(frn, &dos), (frn, &win32_a), (frn, &win32_b)];
        let result = dedup_hardlinks(entries);
        assert_eq!(result.len(), 1, "hard links collapse to a single entry");
        let (parent, name) = result.get(&frn).unwrap();
        assert_eq!(name, "First Link.txt");
        assert_eq!(*parent, 10);
    }

    #[test]
    fn directory_flag_detected() {
        let mut attrs = Vec::new();
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE | RECORD_FLAG_DIRECTORY, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        assert!(header.is_directory());
    }

    #[test]
    fn extension_record_detected_via_base_record_ref() {
        let mut attrs = Vec::new();
        attrs.extend(end_marker());
        // base_record_ref low 48 bits = 42 => this record's attributes belong to FRN 42.
        let mut buf = build_record(RECORD_FLAG_IN_USE, 42, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        assert!(header.is_extension_record());
        assert_eq!(header.base_record_ref & 0x0000_FFFF_FFFF_FFFF, 42);
    }

    #[test]
    fn too_short_buffer_rejected() {
        let mut buf = vec![0u8; 10];
        assert_eq!(
            apply_fixups_and_parse_header(&mut buf),
            Err(RecordError::TooShort)
        );
    }

    #[test]
    fn bad_magic_rejected() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"BAAD");
        assert_eq!(
            apply_fixups_and_parse_header(&mut buf),
            Err(RecordError::BadMagic)
        );
    }

    #[test]
    fn data_runs_cluster_total_sums_lengths() {
        // header 0x21: length_size=1, offset_size=2 -> length=5, delta=10.
        let mut mp = vec![0x21u8, 0x05];
        mp.extend_from_slice(&10i16.to_le_bytes());
        mp.push(0x00);
        assert_eq!(data_runs_cluster_total(&mp), Some(5));
        assert_eq!(data_runs_cluster_total(&[0xFF]), None);
    }

    #[test]
    fn header_bytes_in_use_is_parsed() {
        let mut attrs = Vec::new();
        attrs.extend(end_marker());
        let mut buf = build_record(RECORD_FLAG_IN_USE, 0, &attrs);
        let header = apply_fixups_and_parse_header(&mut buf).unwrap();
        // first_attr_offset(56) + attrs.len()(4, just the end marker)
        assert_eq!(header.bytes_in_use, 60);
        assert!(header.bytes_in_use as usize <= buf.len());
    }
}
