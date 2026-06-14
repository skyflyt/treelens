//! Treelens scanner — parallel directory walk that produces a flat record stream.
//!
//! v0.1 strategy: `std::fs::read_dir` per directory, fanned out across a rayon
//! thread pool via a work-stealing queue. Works on any filesystem and at any
//! privilege level. The MFT fast path (FSCTL_ENUM_USN_DATA) is intentionally
//! deferred to v0.2 — see `PLAN.md` §3.2 path 1.
//!
//! Output is a stream of [`Record`]s emitted through a [`crossbeam_channel`].
//! The receiver builds the arena tree (see the `tree` crate).
//!
//! Cancellation is cooperative via [`Cancel`]; checked once per directory.

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_REPARSE: u16 = 1 << 1;
pub const FLAG_UNSCANNABLE: u16 = 1 << 2;
pub const FLAG_HIDDEN: u16 = 1 << 3;
pub const FLAG_SYSTEM: u16 = 1 << 4;
pub const FLAG_READONLY: u16 = 1 << 5;
/// File is a cloud placeholder (OneDrive Files-On-Demand, etc.) — present in
/// metadata but the bytes are not on local disk. We report allocated=0 for these
/// so "size on disk" totals don't double-count the user's cloud storage.
pub const FLAG_CLOUD_PLACEHOLDER: u16 = 1 << 6;

/// One filesystem entry surfaced by the scanner.
///
/// The scanner emits records as a flat stream over a channel; because workers
/// run concurrently, **the emission order does not match the assigned idx**.
/// Every record carries its own `idx` (atomically allocated at emit time) and
/// references its parent by `parent` (also a scanner-local idx, `u32::MAX` for
/// the root). The receiver MUST sort by `idx` before treating Vec-position as
/// canonical — otherwise `parent` pointers point to the wrong record and the
/// tree topology gets scrambled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub idx: u32,
    pub name: String,
    pub parent: u32,
    pub logical: u64,
    pub allocated: u64,
    pub mtime: i64, // unix seconds, i64 for pre-1970 sanity
    pub flags: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Progress {
    pub files_seen: u64,
    pub bytes_seen: u64,
    pub dirs_seen: u64,
    pub errors: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ScanEvent {
    Progress(Progress),
    Done { duration_ms: u64 },
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub cluster_size: u64,
    pub follow_reparse: bool,
    pub threads: usize,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cluster_size: 4096,
            follow_reparse: false,
            threads: num_cpus().clamp(2, 32),
        }
    }
}

fn num_cpus() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Cooperative cancellation handle, cloneable.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct WorkQueue {
    inner: Mutex<Vec<(PathBuf, u32)>>, // (dir path, parent record index)
    active: AtomicU64,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(1024)),
            active: AtomicU64::new(0),
        }
    }
    fn push(&self, item: (PathBuf, u32)) {
        self.inner.lock().push(item);
    }
    fn pop(&self) -> Option<(PathBuf, u32)> {
        self.inner.lock().pop()
    }
}

/// Run a scan synchronously on a pool of worker threads.
///
/// Records flow into `record_tx`; progress and lifecycle events flow into `event_tx`.
/// The function returns when the queue is empty and no workers are active, or when
/// cancellation is signaled.
pub fn scan(
    opts: ScanOptions,
    cancel: Cancel,
    record_tx: Sender<Record>,
    event_tx: Sender<ScanEvent>,
) {
    let start = Instant::now();
    let stats = Arc::new(ScanStats::default());

    // Root record: index 0, parent u32::MAX, sizes filled in by aggregation.
    let root_meta = fs::metadata(&opts.root).ok();
    let root_name = opts
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| opts.root.to_string_lossy().to_string());
    let root_flags = FLAG_DIR
        | root_meta
            .as_ref()
            .map(|m| {
                if m.file_type().is_symlink() {
                    FLAG_REPARSE
                } else {
                    0
                }
            })
            .unwrap_or(0);
    let _ = record_tx.send(Record {
        idx: 0,
        name: root_name,
        parent: u32::MAX,
        logical: 0,
        allocated: 0,
        mtime: root_meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        flags: root_flags,
    });

    let queue = Arc::new(WorkQueue::new());
    // Root reparse guard: if the scan root is itself a junction/symlink, do NOT
    // traverse it (matches the "never descend reparse points" rule). We still
    // emitted the root record above; just don't enqueue it for a walk.
    if root_flags & FLAG_REPARSE == 0 {
        queue.push((opts.root.clone(), 0));
        queue.active.fetch_add(1, Ordering::AcqRel);
    }

    let next_index = Arc::new(AtomicU64::new(1));
    let opts = Arc::new(opts);

    let last_progress = Arc::new(parking_lot::Mutex::new(Instant::now()));

    let mut handles = Vec::with_capacity(opts.threads);
    for _ in 0..opts.threads {
        let queue = queue.clone();
        let opts = opts.clone();
        let stats = stats.clone();
        let next_index = next_index.clone();
        let record_tx = record_tx.clone();
        let event_tx = event_tx.clone();
        let cancel = cancel.clone();
        let last_progress = last_progress.clone();
        let start_clone = start;
        handles.push(thread::spawn(move || {
            worker(
                queue,
                opts,
                stats,
                next_index,
                record_tx,
                event_tx,
                cancel,
                last_progress,
                start_clone,
            )
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    if cancel.is_cancelled() {
        let _ = event_tx.send(ScanEvent::Cancelled);
    } else {
        // Final progress snapshot so the UI never shows a stale total.
        let _ = event_tx.send(ScanEvent::Progress(Progress {
            files_seen: stats.files.load(Ordering::Relaxed),
            bytes_seen: stats.bytes.load(Ordering::Relaxed),
            dirs_seen: stats.dirs.load(Ordering::Relaxed),
            errors: stats.errors.load(Ordering::Relaxed),
            elapsed_ms: duration_ms,
        }));
        let _ = event_tx.send(ScanEvent::Done { duration_ms });
    }
}

#[derive(Default)]
struct ScanStats {
    files: AtomicU64,
    bytes: AtomicU64,
    dirs: AtomicU64,
    errors: AtomicU64,
}

#[allow(clippy::too_many_arguments)]
fn worker(
    queue: Arc<WorkQueue>,
    opts: Arc<ScanOptions>,
    stats: Arc<ScanStats>,
    next_index: Arc<AtomicU64>,
    record_tx: Sender<Record>,
    event_tx: Sender<ScanEvent>,
    cancel: Cancel,
    last_progress: Arc<parking_lot::Mutex<Instant>>,
    start: Instant,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let item = queue.pop();
        let Some((dir_path, parent_idx)) = item else {
            // No work right now. If anyone is still active, spin briefly. If not, exit.
            let active = queue.active.load(Ordering::Acquire);
            if active == 0 {
                return;
            }
            thread::sleep(Duration::from_micros(500));
            continue;
        };

        // Hold "active" for the duration of this directory's processing.
        let entries = fs::read_dir(&dir_path);
        let mut dir_logical: u64 = 0;
        let mut dir_allocated: u64 = 0;
        match entries {
            Ok(iter) => {
                for ent in iter {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let Ok(ent) = ent else {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let name = ent.file_name().to_string_lossy().to_string();
                    let meta = match ent.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    let ft = meta.file_type();
                    let mut flags: u16 = 0;
                    if ft.is_symlink() {
                        flags |= FLAG_REPARSE;
                    }
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::MetadataExt;
                        let a = meta.file_attributes();
                        // FILE_ATTRIBUTE_REPARSE_POINT — junctions, symlinks, app-exec
                        // aliases. We never traverse these (cycle / double-count safety).
                        if a & 0x0400 != 0 {
                            flags |= FLAG_REPARSE;
                        }
                        // FILE_ATTRIBUTE_OFFLINE (0x1000) / RECALL_ON_DATA_ACCESS (0x400000) /
                        // RECALL_ON_OPEN (0x40000) — OneDrive Files-On-Demand & equivalents.
                        // The file's logical size is real but it occupies ~0 bytes locally
                        // until the user opens it. We surface "on disk" as 0 for these so
                        // a OneDrive-heavy machine doesn't report 2× its actual disk use.
                        if a & 0x001000 != 0 || a & 0x400000 != 0 || a & 0x040000 != 0 {
                            flags |= FLAG_CLOUD_PLACEHOLDER;
                        }
                        if a & 0x0002 != 0 {
                            flags |= FLAG_HIDDEN;
                        }
                        if a & 0x0004 != 0 {
                            flags |= FLAG_SYSTEM;
                        }
                        if a & 0x0001 != 0 {
                            flags |= FLAG_READONLY;
                        }
                    }
                    let is_dir = ft.is_dir() && (flags & FLAG_REPARSE == 0);
                    let logical = if ft.is_file() { meta.len() } else { 0 };
                    let allocated = if ft.is_file() {
                        if flags & FLAG_CLOUD_PLACEHOLDER != 0 {
                            0
                        } else {
                            align_up(logical, opts.cluster_size)
                        }
                    } else {
                        0
                    };
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);

                    let idx64 = next_index.fetch_add(1, Ordering::Relaxed);
                    // Record idx/parent are u32. Past u32::MAX a cast would wrap
                    // and collide with live idxs → tree corruption. Cancel
                    // gracefully instead (no real volume has 4.29B entries).
                    if idx64 > u32::MAX as u64 {
                        cancel.cancel();
                        break;
                    }
                    let idx = idx64 as u32;
                    if is_dir {
                        flags |= FLAG_DIR;
                        // Children will follow; queue this directory for traversal.
                        queue.active.fetch_add(1, Ordering::AcqRel);
                        queue.push((dir_path.join(&name), idx));
                        stats.dirs.fetch_add(1, Ordering::Relaxed);
                    } else if ft.is_file() {
                        dir_logical = dir_logical.saturating_add(logical);
                        dir_allocated = dir_allocated.saturating_add(allocated);
                        stats.files.fetch_add(1, Ordering::Relaxed);
                        stats.bytes.fetch_add(allocated, Ordering::Relaxed);
                    }
                    // Symlinks/reparse points: zero-size leaf, badge in UI.

                    // If the receiver is gone (UI closed), stop walking the disk.
                    if record_tx
                        .send(Record {
                            idx,
                            name,
                            parent: parent_idx,
                            logical,
                            allocated,
                            mtime,
                            flags,
                        })
                        .is_err()
                    {
                        cancel.cancel();
                        break;
                    }
                }
            }
            Err(_) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                // Mark the directory itself as unscannable by emitting a flag-update record?
                // Simpler v0.1: just count the error. The directory still appears in the tree
                // with whatever size aggregation children contribute (zero here).
            }
        }

        // Throttled progress emit.
        let mut lp = last_progress.lock();
        if lp.elapsed() >= Duration::from_millis(100) {
            *lp = Instant::now();
            drop(lp);
            let _ = event_tx.try_send(ScanEvent::Progress(Progress {
                files_seen: stats.files.load(Ordering::Relaxed),
                bytes_seen: stats.bytes.load(Ordering::Relaxed),
                dirs_seen: stats.dirs.load(Ordering::Relaxed),
                errors: stats.errors.load(Ordering::Relaxed),
                elapsed_ms: start.elapsed().as_millis() as u64,
            }));
        }

        queue.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[inline]
fn align_up(n: u64, align: u64) -> u64 {
    if align == 0 {
        n
    } else {
        // Saturating: a sparse/corrupt file reporting a logical size within one
        // cluster of u64::MAX must not overflow-panic (debug) or wrap (release).
        match n.checked_add(align - 1) {
            Some(v) => v & !(align - 1),
            None => n,
        }
    }
}

/// Convenience: spawn a scan on a background thread and return channels.
///
/// `record_buf` and `event_buf` are the channel capacities. The record channel
/// should be large (tens of thousands) because the rate is high; the event
/// channel can be small (a handful is fine).
pub fn spawn(
    opts: ScanOptions,
    cancel: Cancel,
    record_buf: usize,
    event_buf: usize,
) -> (
    Receiver<Record>,
    Receiver<ScanEvent>,
    thread::JoinHandle<()>,
) {
    let (rec_tx, rec_rx) = bounded::<Record>(record_buf);
    let (evt_tx, evt_rx) = bounded::<ScanEvent>(event_buf);
    let handle = thread::spawn(move || {
        scan(opts, cancel, rec_tx, evt_tx);
    });
    (rec_rx, evt_rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn empty_dir_yields_only_root() {
        let dir = tempdir().unwrap();
        let opts = ScanOptions {
            threads: 2,
            ..ScanOptions::new(dir.path())
        };
        let (rec_rx, evt_rx, h) = spawn(opts, Cancel::new(), 1024, 16);
        let recs: Vec<Record> = rec_rx.iter().collect();
        let _ = h.join();
        let _events: Vec<_> = evt_rx.try_iter().collect();
        assert_eq!(recs.len(), 1, "only the root record");
        assert_eq!(recs[0].parent, u32::MAX);
    }

    #[test]
    fn scans_files_and_subdirs() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), vec![0u8; 100]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("b.txt"), vec![0u8; 200]).unwrap();

        let opts = ScanOptions {
            threads: 2,
            ..ScanOptions::new(dir.path())
        };
        let (rec_rx, _evt_rx, h) = spawn(opts, Cancel::new(), 1024, 16);
        let recs: Vec<Record> = rec_rx.iter().collect();
        let _ = h.join();

        // root + a.txt + sub + b.txt
        assert_eq!(recs.len(), 4);
        let files: u64 = recs.iter().filter(|r| r.flags & FLAG_DIR == 0).count() as u64;
        let dirs: u64 = recs.iter().filter(|r| r.flags & FLAG_DIR != 0).count() as u64;
        assert_eq!(files, 2);
        assert_eq!(dirs, 2); // root + sub
        let total_logical: u64 = recs.iter().map(|r| r.logical).sum();
        assert_eq!(total_logical, 300);
    }

    #[cfg(windows)]
    #[test]
    fn junction_is_not_traversed() {
        // Set up:
        //   root/
        //     real/
        //       a.bin (1024 bytes)
        //     link  -> junction to real/
        // Expect: scanner emits one record for `link` (with reparse flag, zero size)
        //         and does NOT recurse into it. Total a.bin counted ONCE.
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("a.bin"), vec![0u8; 1024]).unwrap();

        let link = dir.path().join("link");
        // Use cmd /c mklink /J — works for non-admin (junctions don't require privilege).
        let status = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                link.to_str().unwrap(),
                real.to_str().unwrap(),
            ])
            .status()
            .expect("mklink");
        if !status.success() {
            eprintln!("mklink failed; skipping test (likely sandbox restriction)");
            return;
        }

        let opts = ScanOptions {
            threads: 2,
            ..ScanOptions::new(dir.path())
        };
        let (rec_rx, _evt_rx, h) = spawn(opts, Cancel::new(), 1024, 16);
        let recs: Vec<Record> = rec_rx.iter().collect();
        let _ = h.join();

        // Total logical = 1024 if dedupe works; would be 2048 if junction was traversed.
        let total_logical: u64 = recs.iter().map(|r| r.logical).sum();
        let file_count = recs.iter().filter(|r| r.flags & FLAG_DIR == 0).count();
        let reparse_count = recs.iter().filter(|r| r.flags & FLAG_REPARSE != 0).count();
        eprintln!(
            "records={} total_logical={} files={} reparse_marked={}",
            recs.len(),
            total_logical,
            file_count,
            reparse_count
        );
        assert!(reparse_count >= 1, "junction should be marked as reparse");
        assert_eq!(
            total_logical, 1024,
            "junction was traversed; size double-counted"
        );
    }

    #[test]
    fn cancellation_stops_quickly() {
        let dir = tempdir().unwrap();
        for i in 0..50 {
            fs::create_dir(dir.path().join(format!("d{i}"))).unwrap();
            for j in 0..20 {
                fs::write(
                    dir.path().join(format!("d{i}")).join(format!("f{j}.bin")),
                    vec![0u8; 64],
                )
                .unwrap();
            }
        }
        let cancel = Cancel::new();
        let opts = ScanOptions {
            threads: 2,
            ..ScanOptions::new(dir.path())
        };
        let (rec_rx, evt_rx, h) = spawn(opts, cancel.clone(), 1024, 16);
        cancel.cancel();
        // Drain in case the channel back-pressures.
        let _: Vec<_> = rec_rx.iter().collect();
        let _ = h.join();
        let events: Vec<_> = evt_rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, ScanEvent::Cancelled)));
    }
}
