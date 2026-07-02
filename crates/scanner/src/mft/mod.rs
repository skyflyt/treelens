//! NTFS `$MFT` fast path: orchestration + eligibility check.
//!
//! Strategy (see `../../../../../../obsidian` design doc `mft-fast-path.md`
//! for the full write-up): open the raw volume, use
//! `FSCTL_GET_NTFS_VOLUME_DATA` + `$MFT`'s own data runs (from FILE record 0)
//! to find every byte range the MFT occupies on disk, then stream those bytes
//! sequentially in large chunks, parsing each fixed-size FILE record. The
//! result is stitched into the same flat `Record` stream the directory walk
//! produces, so `crates/tree` and the UI need no changes to consume it.
//!
//! Eligibility (all must hold, checked by `try_mft_scan`'s caller in `lib.rs`):
//! target is a volume root, filesystem is NTFS, process is elevated, and the
//! `useMftFastPath` setting is on. Any failure at any stage of this module
//! causes the caller to fall back to the directory walk -- this module never
//! panics and always returns a `Result`.

mod boot;
mod record;
mod runs;

use crate::{
    Cancel, Excluder, Progress, Record as ScanRecord, ScanEvent, ScanOptions, FLAG_DIR,
    FLAG_HIDDEN, FLAG_READONLY, FLAG_SYSTEM, MAX_ERROR_SAMPLES,
};
use boot::{bootstrap, VolumeLayout};
use crossbeam_channel::Sender;
use record::{
    apply_fixups_and_parse_header, dedup_hardlinks, parse_attributes, pick_canonical_name,
    FileNameAttr, ParsedRecord, RecordHeader,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeInformationW, ReadFile, SetFilePointerEx, FILE_ATTRIBUTE_NORMAL,
    FILE_BEGIN, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Root FRN of any NTFS volume (well-known: 5 = `.` / the volume root directory).
pub const ROOT_FRN: u64 = 5;
/// Chunk size for sequential MFT reads (see spec: "8-32 MiB chunks").
const READ_CHUNK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum MftError {
    #[error("not a volume root")]
    NotVolumeRoot,
    #[error("not an NTFS volume")]
    NotNtfs,
    #[error("not elevated")]
    NotElevated,
    #[error("fast path disabled by settings")]
    Disabled,
    #[error("bootstrap failed: {0}")]
    Boot(#[from] boot::BootError),
    #[error("io error: {0}")]
    Io(String),
}

/// Check the first three eligibility conditions from the spec (volume root,
/// NTFS, elevated). The fourth (the `useMftFastPath` setting) is a UI-layer
/// concern threaded in via `ScanOptions` by the caller (see `use_mft` below),
/// since the scanner crate has no knowledge of settings persistence.
pub fn is_eligible(root: &Path, use_mft_setting: bool) -> Result<(), MftError> {
    if !use_mft_setting {
        return Err(MftError::Disabled);
    }
    if !is_volume_root(root) {
        return Err(MftError::NotVolumeRoot);
    }
    if !is_ntfs(root) {
        return Err(MftError::NotNtfs);
    }
    if !fileops::is_elevated() {
        return Err(MftError::NotElevated);
    }
    Ok(())
}

/// True if `root` is exactly a drive root like `C:\` (not a subdirectory).
fn is_volume_root(root: &Path) -> bool {
    let s = root.to_string_lossy();
    let bytes: Vec<char> = s.chars().collect();
    bytes.len() == 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == ':'
        && (bytes[2] == '\\' || bytes[2] == '/')
}

fn is_ntfs(root: &Path) -> bool {
    let s = root.to_string_lossy();
    let wroot: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut fs_buf = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wroot.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_buf),
        )
        .is_ok()
    };
    if !ok {
        return false;
    }
    let len = fs_buf.iter().position(|&c| c == 0).unwrap_or(0);
    let fs = String::from_utf16_lossy(&fs_buf[..len]);
    fs.eq_ignore_ascii_case("NTFS")
}

/// Run the MFT fast path. On any internal failure, returns `Err` and emits
/// NOTHING on the channels -- the caller (`scan()` in `lib.rs`) is responsible
/// for falling back to the walk and surfacing the failure note. On success,
/// this function drives the full scan to completion (or cancellation) itself,
/// emitting the same `Progress`/`Errors`/`Done`/`Cancelled` events the walk
/// emits, so from the caller's perspective it's a drop-in alternative body
/// for `scan()`.
pub fn run(
    opts: &ScanOptions,
    cancel: &Cancel,
    record_tx: &Sender<ScanRecord>,
    event_tx: &Sender<ScanEvent>,
) -> Result<(), MftError> {
    let start = Instant::now();
    let layout = bootstrap(&opts.root)?;

    debug_assert!(
        layout.runs_are_cluster_aligned(),
        "MFT data runs should always be cluster-aligned by construction"
    );
    debug_assert!(
        layout.total_record_slots() as u128 * layout.bytes_per_file_record as u128
            <= layout.total_capacity_bytes() as u128,
        "MFT record slots should fit within the volume's reported capacity"
    );
    let letter = boot::drive_letter_of(&opts.root).ok_or(MftError::NotVolumeRoot)?;
    let handle = open_volume(letter)?;
    let result = run_with_handle(opts, cancel, record_tx, event_tx, &layout, handle, start);
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

fn open_volume(letter: char) -> Result<HANDLE, MftError> {
    let path = format!(r"\\.\{letter}:");
    let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
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
        .map_err(|e| MftError::Io(format!("open volume: {e}")))
    }
}

/// One raw FILE record's extracted essentials, keyed by its own FRN (record
/// index within the MFT), before tree stitching / exclusion / idx assignment.
struct RawEntry {
    is_dir: bool,
    parsed: ParsedRecord,
    /// Names collected from THIS base record's own $FILE_NAME attributes.
    /// Names that live in extension records (via $ATTRIBUTE_LIST) are merged
    /// in a second pass over the extension-record name list below.
    file_names: Vec<FileNameAttr>,
}

#[allow(clippy::too_many_arguments)]
fn run_with_handle(
    opts: &ScanOptions,
    cancel: &Cancel,
    record_tx: &Sender<ScanRecord>,
    event_tx: &Sender<ScanEvent>,
    layout: &VolumeLayout,
    handle: HANDLE,
    start: Instant,
) -> Result<(), MftError> {
    let bytes_per_record = layout.bytes_per_file_record as usize;
    if bytes_per_record == 0 {
        return Err(MftError::Io("bytes_per_file_record is 0".into()));
    }
    let total_slots = layout.total_record_slots();

    // Pass 1: stream every FILE record, parse it, collect base records.
    // Extension records (base_record_ref != 0) are stashed separately so we
    // can merge their attributes into the owning base record afterward (the
    // "$ATTRIBUTE_LIST common case" from the spec).
    let mut base_entries: HashMap<u64, RawEntry> = HashMap::new();
    let mut extension_names: Vec<(u64, FileNameAttr)> = Vec::new(); // (owning base FRN, name)
    let mut stat_fallback_frns: Vec<u64> = Vec::new();

    let mut files_seen: u64 = 0;
    let mut dirs_seen: u64 = 0;
    let mut errors: u64 = 0;
    let mut err_samples: Vec<String> = Vec::new();
    let mut last_progress_emit = Instant::now();

    let mut frn: u64 = 0;
    'outer: for &(run_offset, run_len) in &layout.mft_byte_runs {
        let mut pos_in_run: u64 = 0;
        while pos_in_run < run_len {
            if cancel.is_cancelled() {
                let _ = event_tx.send(ScanEvent::Cancelled);
                return Ok(());
            }
            let remaining_in_run = run_len - pos_in_run;
            let chunk_len = (READ_CHUNK_BYTES as u64).min(remaining_in_run) as usize;
            let mut buf = vec![0u8; chunk_len];
            read_at(handle, run_offset + pos_in_run, &mut buf)
                .map_err(|e| MftError::Io(format!("read chunk: {e}")))?;

            let records_in_chunk = chunk_len / bytes_per_record;
            for i in 0..records_in_chunk {
                if frn >= total_slots.max(1) && total_slots > 0 {
                    // We've covered the MFT's valid data length; remaining
                    // preallocated-but-unused slots are skipped.
                    break 'outer;
                }
                if cancel.is_cancelled() {
                    let _ = event_tx.send(ScanEvent::Cancelled);
                    return Ok(());
                }
                let rec_start = i * bytes_per_record;
                let rec_buf = &mut buf[rec_start..rec_start + bytes_per_record];
                let this_frn = frn;
                frn += 1;

                let header = match apply_fixups_and_parse_header(rec_buf) {
                    Ok(h) => h,
                    Err(_) => {
                        // Corrupt/unallocated record; not necessarily an error worth
                        // surfacing (many slots are legitimately empty/unused).
                        continue;
                    }
                };
                if !header.is_in_use() {
                    continue;
                }
                if header.is_extension_record() {
                    handle_extension_record(rec_buf, &header, &mut extension_names);
                    continue;
                }

                let parsed = match parse_attributes(rec_buf, header.first_attr_offset as usize) {
                    Ok(p) => p,
                    Err(_) => {
                        errors += 1;
                        if err_samples.len() < MAX_ERROR_SAMPLES {
                            err_samples.push(format!("<mft record {this_frn}>"));
                        }
                        continue;
                    }
                };

                if parsed.unresolved_attribute_list {
                    stat_fallback_frns.push(this_frn);
                }

                let is_dir = header.is_directory();
                if is_dir {
                    dirs_seen += 1;
                } else {
                    files_seen += 1;
                }

                let file_names = parsed.file_names.clone();
                base_entries.insert(
                    this_frn,
                    RawEntry {
                        is_dir,
                        parsed,
                        file_names,
                    },
                );
            }

            pos_in_run += chunk_len as u64;

            if last_progress_emit.elapsed() >= Duration::from_millis(100) {
                last_progress_emit = Instant::now();
                let _ = event_tx.try_send(ScanEvent::Progress(Progress {
                    files_seen,
                    bytes_seen: 0, // bytes total is only known once sizes are aggregated by `tree`
                    dirs_seen,
                    errors,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                }));
            }
        }
    }

    // Pass 2: merge $ATTRIBUTE_LIST extension-record names into their base entries.
    for (base_frn, name) in extension_names {
        if let Some(entry) = base_entries.get_mut(&base_frn) {
            entry.file_names.push(name);
        }
    }

    // Per-file stat fallback for records whose $ATTRIBUTE_LIST we didn't fully
    // resolve (rare: heavily-fragmented attribute sets). We already have a
    // name/parent from the base record's own $FILE_NAME (if any); the spec
    // only requires degrading gracefully, not perfect fidelity here.
    for frn in &stat_fallback_frns {
        if let Some(entry) = base_entries.get(frn) {
            if entry.file_names.is_empty() {
                errors += 1;
                if err_samples.len() < MAX_ERROR_SAMPLES {
                    err_samples.push(format!("<mft record {frn}: unresolved attribute list>"));
                }
            }
        }
    }

    // Hard-link dedup: pick one canonical (parent, name) per FRN.
    let mut canonical: HashMap<u64, (u64, String)> = HashMap::new();
    for (frn, entry) in &base_entries {
        if let Some(fna) = pick_canonical_name(&entry.file_names) {
            canonical.insert(*frn, (fna.parent_frn, fna.name.clone()));
        }
    }
    // Also fold through the shared dedup helper for FRNs with multiple raw
    // $FILE_NAME attributes on the SAME base record (true hard links share one
    // base record only when... actually each hard link has its own $FILE_NAME
    // attribute on the SAME base record in NTFS). Re-run dedup using all names
    // collected per FRN to guarantee "first Win32 link wins" semantics exactly
    // as documented in `record::dedup_hardlinks`.
    let all_named: Vec<(u64, &FileNameAttr)> = base_entries
        .iter()
        .flat_map(|(frn, e)| e.file_names.iter().map(move |n| (*frn, n)))
        .collect();
    let deduped = dedup_hardlinks(all_named);
    for (frn, (parent, name)) in deduped {
        canonical.insert(frn, (parent, name));
    }

    if cancel.is_cancelled() {
        let _ = event_tx.send(ScanEvent::Cancelled);
        return Ok(());
    }

    // Tree stitching: assign scanner-local idx values (root = 0, parent u32::MAX),
    // orphans go under a synthetic "(orphaned)" node. Apply excludes by
    // reconstructed full path, same semantics as the walk.
    emit_records(
        opts,
        cancel,
        record_tx,
        &base_entries,
        &canonical,
        &mut files_seen,
        &mut dirs_seen,
    )?;

    let duration_ms = start.elapsed().as_millis() as u64;
    if cancel.is_cancelled() {
        let _ = event_tx.send(ScanEvent::Cancelled);
        return Ok(());
    }
    let _ = event_tx.send(ScanEvent::Progress(Progress {
        files_seen,
        bytes_seen: 0,
        dirs_seen,
        errors,
        elapsed_ms: duration_ms,
    }));
    if !err_samples.is_empty() {
        let _ = event_tx.send(ScanEvent::Errors(err_samples));
    }
    let _ = event_tx.send(ScanEvent::Done { duration_ms });
    Ok(())
}

/// Parse an extension record's own $FILE_NAME attributes (if any -- most
/// extension records only carry spilled $DATA or $ATTRIBUTE_LIST-referenced
/// attributes, but a $FILE_NAME CAN legally live in one) and stash them
/// against the base FRN they belong to.
fn handle_extension_record(buf: &[u8], header: &RecordHeader, out: &mut Vec<(u64, FileNameAttr)>) {
    let base_frn = header.base_record_ref & 0x0000_FFFF_FFFF_FFFF;
    if let Ok(parsed) = parse_attributes(buf, header.first_attr_offset as usize) {
        for name in parsed.file_names {
            out.push((base_frn, name));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_records(
    opts: &ScanOptions,
    cancel: &Cancel,
    record_tx: &Sender<ScanRecord>,
    base_entries: &HashMap<u64, RawEntry>,
    canonical: &HashMap<u64, (u64, String)>,
    files_seen: &mut u64,
    dirs_seen: &mut u64,
) -> Result<(), MftError> {
    let excluder = Excluder::new(&opts.excludes);
    let has_excludes = !excluder.is_empty();

    // Build full paths bottom-up via parent-FRN chase, memoized, so exclusion
    // globs (which can match full paths) see the same semantics as the walk.
    let mut path_cache: HashMap<u64, String> = HashMap::new();
    path_cache.insert(ROOT_FRN, String::new()); // root's "relative path" is empty

    fn resolve_path(
        frn: u64,
        canonical: &HashMap<u64, (u64, String)>,
        cache: &mut HashMap<u64, String>,
        depth: u32,
    ) -> Option<String> {
        if let Some(p) = cache.get(&frn) {
            return Some(p.clone());
        }
        if depth > 4096 {
            return None; // cycle guard
        }
        let (parent, name) = canonical.get(&frn)?;
        let parent_path = resolve_path(*parent, canonical, cache, depth + 1)?;
        let full = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}\\{name}")
        };
        cache.insert(frn, full.clone());
        Some(full)
    }

    // idx assignment: root = 0. Everything else gets a stable idx via a
    // FRN -> idx map, assigned in a first pass so parent references always
    // resolve regardless of iteration order (matches the walk's contract:
    // "every record has a scanner-local idx and parent pointing at an
    // already-or-will-be-valid idx in the same scan's numbering").
    let root_name = opts
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| opts.root.to_string_lossy().to_string());

    let orphan_frn_marker = u64::MAX; // synthetic "(orphaned)" bucket, not a real FRN

    let mut frn_to_idx: HashMap<u64, u32> = HashMap::new();
    frn_to_idx.insert(ROOT_FRN, 0);
    let mut next_idx: u32 = 1;

    // Determine, per FRN, its effective parent FRN for tree purposes: the
    // real parent if resolvable back to the root, else the synthetic orphan
    // bucket (which itself hangs off root).
    let mut effective_parent: HashMap<u64, u64> = HashMap::new();
    let mut any_orphans = false;
    for &frn in base_entries.keys() {
        if frn == ROOT_FRN {
            continue;
        }
        match resolve_path(frn, canonical, &mut path_cache, 0) {
            Some(_) => {
                let parent = canonical.get(&frn).map(|(p, _)| *p).unwrap_or(ROOT_FRN);
                effective_parent.insert(frn, parent);
            }
            None => {
                effective_parent.insert(frn, orphan_frn_marker);
                any_orphans = true;
            }
        }
    }

    if any_orphans {
        frn_to_idx.insert(orphan_frn_marker, next_idx);
        next_idx += 1;
    }

    // Assign idx to every remaining base entry (order doesn't matter for
    // correctness since we resolve parent idx lazily via the map, but we
    // iterate deterministically by FRN for reproducible test output).
    let mut frns_sorted: Vec<u64> = base_entries
        .keys()
        .copied()
        .filter(|f| *f != ROOT_FRN)
        .collect();
    frns_sorted.sort_unstable();
    for frn in &frns_sorted {
        frn_to_idx.entry(*frn).or_insert_with(|| {
            let idx = next_idx;
            next_idx += 1;
            idx
        });
    }

    // Emit root record first.
    if record_tx
        .send(ScanRecord {
            idx: 0,
            name: root_name,
            parent: u32::MAX,
            logical: 0,
            allocated: 0,
            mtime: 0,
            flags: FLAG_DIR,
        })
        .is_err()
    {
        cancel.cancel();
        return Ok(());
    }

    if any_orphans
        && record_tx
            .send(ScanRecord {
                idx: frn_to_idx[&orphan_frn_marker],
                name: "(orphaned)".to_string(),
                parent: 0,
                logical: 0,
                allocated: 0,
                mtime: 0,
                flags: FLAG_DIR,
            })
            .is_err()
    {
        cancel.cancel();
        return Ok(());
    }

    *files_seen = 0;
    *dirs_seen = 0;
    for frn in &frns_sorted {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let entry = &base_entries[frn];
        let Some((_, name)) = canonical.get(frn) else {
            continue; // no resolvable name at all (e.g. purely DOS-deleted); skip
        };

        if has_excludes {
            let full_rel = path_cache.get(frn).cloned().unwrap_or_else(|| name.clone());
            let full_abs = format!("{}{}", opts.root.to_string_lossy(), full_rel);
            if excluder.excluded(name, &full_abs) {
                continue;
            }
        }

        let parent_frn = effective_parent.get(frn).copied().unwrap_or(ROOT_FRN);
        let parent_idx = frn_to_idx
            .get(&parent_frn)
            .copied()
            .unwrap_or_else(|| frn_to_idx.get(&orphan_frn_marker).copied().unwrap_or(0));
        let idx = frn_to_idx[frn];

        let mut flags: u16 = 0;
        if entry.is_dir {
            flags |= FLAG_DIR;
            *dirs_seen += 1;
        } else {
            *files_seen += 1;
        }
        // Hidden/system/readonly flags aren't available from the FILE record
        // fields we parse (they live in $FILE_NAME's own "flags" field, which
        // we don't currently extract) -- left as 0 pending a follow-up; this
        // does not affect size/count totals, only badge display.
        let _ = (FLAG_HIDDEN, FLAG_READONLY, FLAG_SYSTEM);

        let mtime = filetime_to_unix(entry.parsed.mtime_100ns.unwrap_or(0));

        if record_tx
            .send(ScanRecord {
                idx,
                name: name.clone(),
                parent: parent_idx,
                logical: entry.parsed.logical_size,
                allocated: entry.parsed.allocated_size,
                mtime,
                flags,
            })
            .is_err()
        {
            cancel.cancel();
            return Ok(());
        }
    }

    Ok(())
}

/// Convert a Windows FILETIME (100ns ticks since 1601-01-01) to Unix seconds.
fn filetime_to_unix(ticks_100ns: i64) -> i64 {
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000; // 1601 -> 1970 in 100ns units
    (ticks_100ns - EPOCH_DIFF_100NS) / 10_000_000
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
    fn is_volume_root_accepts_only_bare_drive_roots() {
        assert!(is_volume_root(Path::new(r"C:\")));
        assert!(!is_volume_root(Path::new(r"C:\Users")));
        assert!(!is_volume_root(Path::new(r"C:\Users\foo")));
    }

    #[test]
    fn filetime_epoch_conversion() {
        // 1970-01-01 00:00:00 UTC in FILETIME 100ns ticks.
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        // One second later.
        assert_eq!(filetime_to_unix(116_444_736_000_000_000 + 10_000_000), 1);
    }
}
