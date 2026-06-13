//! Tauri shell for Treelens: IPC commands, scan state machine, event plumbing.
//!
//! Architectural rule (PLAN.md §3.1): the tree never crosses the IPC boundary.
//! Commands answer narrow questions in bounded payloads; the frontend re-queries
//! after a drill-in or sort change.

mod state;

use crossbeam_channel::Receiver;
use parking_lot::{Mutex, RwLock};
use scanner::{Cancel, ScanEvent, ScanOptions};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tauri::{AppHandle, Emitter, State};

use state::ScanState;
use tree::{DirRow, LayoutOpts, Rect, SizeMode, SortKey, Tree};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ScanProgressPayload {
    files: u64,
    bytes: u64,
    dirs: u64,
    errors: u64,
    elapsed_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ScanCompletePayload {
    root_idx: u32,
    nodes: u32,
    bytes: u64,
    files: u64,
    dirs: u64,
    duration_ms: u64,
    root_path: String,
}

#[derive(Debug, Serialize)]
struct CommandError {
    message: String,
}

impl<E: std::fmt::Display> From<E> for CommandError {
    fn from(e: E) -> Self {
        CommandError {
            message: e.to_string(),
        }
    }
}

#[tauri::command]
async fn scan_start(
    path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), CommandError> {
    // Cancel any in-flight scan first.
    {
        let mut guard = state.scan.lock();
        if let Some(prev) = guard.take() {
            prev.cancel.cancel();
        }
    }
    let root = PathBuf::from(&path);
    if !root.exists() {
        return Err(CommandError {
            message: format!("path does not exist: {path}"),
        });
    }
    let cancel = Cancel::new();
    let opts = ScanOptions::new(root.clone());

    // Channel sizes: records can be high-volume; events are sparse.
    let (rec_rx, evt_rx, handle) = scanner::spawn(opts, cancel.clone(), 8192, 32);

    *state.scan.lock() = Some(ScanState {
        cancel: cancel.clone(),
        path: root.clone(),
    });

    let app_for_collector = app.clone();
    let state_arc = state.inner().clone_handle();
    thread::spawn(move || {
        collect_scan(app_for_collector, state_arc, root, rec_rx, evt_rx, handle);
    });

    Ok(())
}

#[tauri::command]
fn scan_cancel(state: State<'_, AppState>) -> Result<(), CommandError> {
    let mut guard = state.scan.lock();
    if let Some(s) = guard.take() {
        s.cancel.cancel();
    }
    Ok(())
}

fn collect_scan(
    app: AppHandle,
    state: Arc<AppStateInner>,
    root: PathBuf,
    rec_rx: Receiver<scanner::Record>,
    evt_rx: Receiver<ScanEvent>,
    handle: thread::JoinHandle<()>,
) {
    let mut records: Vec<scanner::Record> = Vec::with_capacity(16_384);
    let mut last_progress: Option<scanner::Progress> = None;
    let mut cancelled = false;
    let mut duration_ms: u64 = 0;

    // Drain records and events concurrently.
    loop {
        crossbeam_channel::select! {
            recv(rec_rx) -> r => match r {
                Ok(rec) => records.push(rec),
                Err(_) => break, // record sender dropped
            },
            recv(evt_rx) -> e => if let Ok(ev) = e {
                match ev {
                    ScanEvent::Progress(p) => {
                        let payload = ScanProgressPayload {
                            files: p.files_seen,
                            bytes: p.bytes_seen,
                            dirs: p.dirs_seen,
                            errors: p.errors,
                            elapsed_ms: p.elapsed_ms,
                        };
                        let _ = app.emit("scan:progress", payload.clone());
                        last_progress = Some(p);
                    }
                    ScanEvent::Cancelled => { cancelled = true; }
                    ScanEvent::Done { duration_ms: d } => { duration_ms = d; }
                }
            }
        }
    }
    let _ = handle.join();

    if cancelled {
        *state.scan_path.lock() = None;
        *state.tree.write() = None;
        let _ = app.emit("scan:cancelled", ());
        return;
    }

    let tree = Tree::build(records);
    let root_idx = tree.root;
    let bytes = tree.nodes[root_idx as usize].allocated;
    let files = tree.nodes[root_idx as usize].file_count;
    let dirs = tree.nodes[root_idx as usize].dir_count;
    let nodes = tree.nodes.len() as u32;
    *state.scan_path.lock() = Some(root.clone());
    *state.tree.write() = Some(tree);
    *state.last_progress.lock() = last_progress;
    let payload = ScanCompletePayload {
        root_idx,
        nodes,
        bytes,
        files,
        dirs,
        duration_ms,
        root_path: root.to_string_lossy().to_string(),
    };
    let _ = app.emit("scan:complete", payload);
}

#[tauri::command]
fn list_dir(
    parent: u32,
    sort: String,
    offset: usize,
    limit: usize,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<DirRow>, CommandError> {
    let _timer = TimedSpan::new("list_dir");
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    if parent as usize >= tree.nodes.len() {
        return Err(CommandError {
            message: format!("invalid parent {parent}"),
        });
    }
    Ok(tree::list_dir(
        tree,
        parent,
        parse_sort(&sort),
        offset,
        limit,
        parse_mode(&size_mode),
    ))
}

#[tauri::command]
fn child_count(parent: u32, state: State<'_, AppState>) -> Result<usize, CommandError> {
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    Ok(tree::dir_count(tree, parent))
}

#[tauri::command]
fn treemap_layout(
    root: u32,
    width: f32,
    height: f32,
    min_px: f32,
    max_depth: u16,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<Rect>, CommandError> {
    let _timer = TimedSpan::new("treemap_layout");
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    if root as usize >= tree.nodes.len() {
        return Err(CommandError {
            message: format!("invalid root {root}"),
        });
    }
    let opts = LayoutOpts {
        width,
        height,
        min_px,
        max_depth,
        padding: 1.0,
    };
    Ok(tree::treemap_layout(
        tree,
        root,
        opts,
        parse_mode(&size_mode),
    ))
}

#[derive(Debug, Serialize)]
struct TopNResponse {
    files: Vec<DirRow>,
    dirs: Vec<DirRow>,
}

#[tauri::command]
fn top_n(
    root: u32,
    n: usize,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<TopNResponse, CommandError> {
    let _timer = TimedSpan::new("top_n");
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    let mode = parse_mode(&size_mode);
    Ok(TopNResponse {
        files: tree::top_files(tree, root, n, mode),
        dirs: tree::top_dirs(tree, root, n, mode),
    })
}

#[derive(Debug, Serialize)]
struct BreadcrumbEntry {
    idx: u32,
    name: String,
}

#[tauri::command]
fn breadcrumb(idx: u32, state: State<'_, AppState>) -> Result<Vec<BreadcrumbEntry>, CommandError> {
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    Ok(tree
        .path(idx)
        .into_iter()
        .map(|(i, n)| BreadcrumbEntry { idx: i, name: n })
        .collect())
}

#[derive(Debug, Serialize)]
struct NodeSummary {
    idx: u32,
    name: String,
    full_path: String,
    is_dir: bool,
    is_reparse: bool,
    allocated: u64,
    logical: u64,
    file_count: u64,
    dir_count: u64,
    mtime: i64,
    newest_mtime: i64,
    oldest_mtime: i64,
}

#[tauri::command]
fn node_summary(idx: u32, state: State<'_, AppState>) -> Result<NodeSummary, CommandError> {
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    let path = node_path(state.inner(), idx)?;
    let n = &tree.nodes[idx as usize];
    Ok(NodeSummary {
        idx,
        name: n.name.clone(),
        full_path: path.to_string_lossy().to_string(),
        is_dir: n.is_dir(),
        is_reparse: n.is_reparse(),
        allocated: n.allocated,
        logical: n.logical,
        file_count: n.file_count,
        dir_count: n.dir_count,
        mtime: n.mtime,
        newest_mtime: n.newest_mtime,
        oldest_mtime: n.oldest_mtime,
    })
}

fn node_path(state: &AppStateInner, idx: u32) -> Result<PathBuf, CommandError> {
    let tree_guard = state.tree.read();
    let tree = tree_guard.as_ref().ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    let scan_path = state.scan_path.lock().clone().ok_or_else(|| CommandError {
        message: "no scan path".into(),
    })?;
    let mut segments: Vec<String> = tree.path(idx).into_iter().map(|(_, n)| n).collect();
    // The first segment is the root node's recorded name; we drop it and join the rest
    // onto the actual scan root (handles things like "C:\" vs name="C:").
    if !segments.is_empty() {
        segments.remove(0);
    }
    let mut p = scan_path;
    for seg in segments {
        p.push(seg);
    }
    Ok(p)
}

#[tauri::command]
fn open_in_explorer(idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), idx)?;
    fileops::open_in_explorer(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(())
}

#[tauri::command]
fn open_in_terminal(idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), idx)?;
    fileops::open_in_terminal(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(())
}

#[tauri::command]
fn copy_path(idx: u32, state: State<'_, AppState>) -> Result<String, CommandError> {
    let path = node_path(state.inner(), idx)?;
    Ok(path.to_string_lossy().to_string())
}

/// Open a file in its default application (the "edit" action).
#[tauri::command]
fn open_file(idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), idx)?;
    fileops::open_file(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct MutationResult {
    ok: bool,
    /// Full path of the created/renamed item, so the UI can message it.
    path: String,
    /// The scan root path, so the frontend can re-scan to reflect the change.
    rescan_path: String,
}

fn rescan_path_of(state: &AppStateInner) -> Result<String, CommandError> {
    state
        .scan_path
        .lock()
        .clone()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| CommandError {
            message: "no active scan".into(),
        })
}

/// Create a new folder inside the directory identified by `idx`.
#[tauri::command]
fn create_folder(
    idx: u32,
    name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let dir = node_path(state.inner(), idx)?;
    let created = fileops::create_folder(&dir, &name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: created.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner())?,
    })
}

/// Create a new empty file inside the directory identified by `idx`.
#[tauri::command]
fn create_file(
    idx: u32,
    name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let dir = node_path(state.inner(), idx)?;
    let created = fileops::create_file(&dir, &name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: created.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner())?,
    })
}

/// Rename the file or folder identified by `idx` to `new_name` (bare segment).
#[tauri::command]
fn rename_node(
    idx: u32,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let path = node_path(state.inner(), idx)?;
    let renamed = fileops::rename_path(&path, &new_name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: renamed.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner())?,
    })
}

#[derive(Debug, Deserialize)]
struct RecyclePayload {
    idx: u32,
}

#[derive(Debug, Serialize)]
struct RecycleResult {
    ok: bool,
    affected_idx: u32,
    path: String,
}

#[tauri::command]
fn recycle_node(
    payload: RecyclePayload,
    state: State<'_, AppState>,
) -> Result<RecycleResult, CommandError> {
    let path = node_path(state.inner(), payload.idx)?;
    fileops::recycle(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(RecycleResult {
        ok: true,
        affected_idx: payload.idx,
        path: path.to_string_lossy().to_string(),
    })
}

#[derive(Debug, Serialize)]
struct DriveEntry {
    letter: String,
    label: Option<String>,
    total: u64,
    free: u64,
    fs: String,
}

#[tauri::command]
fn list_drives() -> Result<Vec<DriveEntry>, CommandError> {
    list_windows_drives().map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

#[cfg(windows)]
fn list_windows_drives() -> anyhow::Result<Vec<DriveEntry>> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    };

    let mask = unsafe { GetLogicalDrives() };
    let mut out: Vec<DriveEntry> = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i as u8);
        let root = format!("{letter}:\\");
        let wroot: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let mut total = 0u64;
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wroot.as_ptr()),
                None,
                Some(&mut total),
                Some(&mut free),
            )
            .is_ok()
        };
        if !ok {
            continue;
        }
        // Volume label and filesystem.
        let mut label_buf = [0u16; 256];
        let mut fs_buf = [0u16; 64];
        let mut serial = 0u32;
        let mut max_comp = 0u32;
        let mut fs_flags = 0u32;
        let label_ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wroot.as_ptr()),
                Some(&mut label_buf),
                Some(&mut serial),
                Some(&mut max_comp),
                Some(&mut fs_flags),
                Some(&mut fs_buf),
            )
            .is_ok()
        };
        let label = if label_ok {
            let len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
            let s = String::from_utf16_lossy(&label_buf[..len]);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        };
        let fs = {
            let len = fs_buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&fs_buf[..len])
        };
        out.push(DriveEntry {
            letter: format!("{letter}:\\"),
            label,
            total,
            free,
            fs,
        });
    }
    Ok(out)
}

#[cfg(not(windows))]
fn list_windows_drives() -> anyhow::Result<Vec<DriveEntry>> {
    Ok(vec![])
}

#[derive(Debug, Serialize)]
struct ElevationStatus {
    elevated: bool,
}

#[tauri::command]
fn is_elevated() -> ElevationStatus {
    #[cfg(windows)]
    {
        ElevationStatus {
            elevated: fileops::is_elevated(),
        }
    }
    #[cfg(not(windows))]
    {
        ElevationStatus { elevated: false }
    }
}

#[tauri::command]
fn relaunch_as_admin(app: AppHandle) -> Result<(), CommandError> {
    #[cfg(windows)]
    {
        fileops::relaunch_as_admin().map_err(|e| CommandError {
            message: format!("{e}"),
        })?;
        app.exit(0);
    }
    #[cfg(not(windows))]
    {
        let _ = app;
    }
    Ok(())
}

#[tauri::command]
fn find_old_files(
    idx: u32,
    cutoff_unix_secs: i64,
    min_size: u64,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<fileops::OldFile>, CommandError> {
    let path = node_path(state.inner(), idx)?;
    fileops::find_old_files(&path, cutoff_unix_secs, min_size, limit).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

#[tauri::command]
fn find_empty_dirs(
    idx: u32,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<String>, CommandError> {
    let path = node_path(state.inner(), idx)?;
    let dirs = fileops::find_empty_dirs(&path, limit).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(dirs
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

/// Drop-on-scope timer that logs the elapsed milliseconds of an IPC command
/// in debug builds. Release builds elide both the print and the `Instant::now()`.
struct TimedSpan {
    label: &'static str,
    start: std::time::Instant,
}
impl TimedSpan {
    #[inline]
    fn new(label: &'static str) -> Self {
        Self {
            label,
            start: std::time::Instant::now(),
        }
    }
}
impl Drop for TimedSpan {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if cfg!(debug_assertions) || ms > 200.0 {
            eprintln!("[ipc] {} {:.1} ms", self.label, ms);
        }
    }
}

fn parse_mode(s: &str) -> SizeMode {
    match s {
        "logical" => SizeMode::Logical,
        _ => SizeMode::Allocated,
    }
}

fn parse_sort(s: &str) -> SortKey {
    match s {
        "size_asc" => SortKey::SizeAsc,
        "name_asc" => SortKey::NameAsc,
        "name_desc" => SortKey::NameDesc,
        "mtime_desc" => SortKey::MtimeDesc,
        "mtime_asc" => SortKey::MtimeAsc,
        "count_desc" => SortKey::CountDesc,
        _ => SortKey::SizeDesc,
    }
}

// ---------- App state ----------

pub struct AppStateInner {
    pub scan: Mutex<Option<ScanState>>,
    pub scan_path: Mutex<Option<PathBuf>>,
    /// The arena tree is mostly READ — every IPC query is a read. A single
    /// `Mutex` makes the 4 parallel queries on every drill (treemap_layout +
    /// list_dir + top_n + breadcrumb) serialize behind each other; an
    /// `RwLock` lets them genuinely run in parallel. The only writer is the
    /// scan collector thread, which replaces the tree atomically at end of
    /// scan and otherwise doesn't touch it.
    pub tree: RwLock<Option<Tree>>,
    pub last_progress: Mutex<Option<scanner::Progress>>,
}

impl AppStateInner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            scan: Mutex::new(None),
            scan_path: Mutex::new(None),
            tree: RwLock::new(None),
            last_progress: Mutex::new(None),
        })
    }
}

pub use AppStateInner as AppStateInnerExport;

// State<'_, AppState> in Tauri stores by managed value; we wrap an Arc so the collector
// thread can hold a strong reference.
pub struct AppState {
    inner: Arc<AppStateInner>,
}

impl AppState {
    fn new() -> Self {
        Self {
            inner: AppStateInner::new(),
        }
    }
    fn clone_handle(&self) -> Arc<AppStateInner> {
        self.inner.clone()
    }
}

impl std::ops::Deref for AppState {
    type Target = AppStateInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// ---------- Self-test ----------

/// Exercise the destructive file-op pipeline end-to-end against a temp scratch
/// dir: create folder → create file → rename → recycle. Returns a process exit
/// code (0 = all passed). Prints a line per step to stderr.
// The explicit `return` in the cfg(windows) arm is needed because a
// cfg(not(windows)) block follows it in source; clippy only sees the Windows
// target and flags it as needless.
#[allow(clippy::needless_return)]
pub fn selftest() -> i32 {
    #[cfg(windows)]
    {
        let scratch = std::env::temp_dir().join("treelens-selftest");
        let _ = std::fs::remove_dir_all(&scratch);
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            eprintln!("FAIL setup: {e}");
            return 1;
        }
        let mut ok = true;

        match fileops::create_folder(&scratch, "newfolder") {
            Ok(p) if p.is_dir() => eprintln!("PASS create_folder -> {}", p.display()),
            Ok(p) => {
                eprintln!("FAIL create_folder: not a dir {}", p.display());
                ok = false;
            }
            Err(e) => {
                eprintln!("FAIL create_folder: {e}");
                ok = false;
            }
        }

        let file = match fileops::create_file(&scratch, "note.txt") {
            Ok(p) if p.is_file() => {
                eprintln!("PASS create_file -> {}", p.display());
                Some(p)
            }
            Ok(p) => {
                eprintln!("FAIL create_file: not a file {}", p.display());
                ok = false;
                None
            }
            Err(e) => {
                eprintln!("FAIL create_file: {e}");
                ok = false;
                None
            }
        };

        if let Some(f) = file {
            match fileops::rename_path(&f, "renamed.txt") {
                Ok(p) if p.is_file() && !f.exists() => {
                    eprintln!("PASS rename_path -> {}", p.display());
                    // Recycle the renamed file.
                    match fileops::recycle(&p) {
                        Ok(()) if !p.exists() => eprintln!("PASS recycle -> gone from disk"),
                        Ok(()) => {
                            eprintln!("FAIL recycle: still on disk {}", p.display());
                            ok = false;
                        }
                        Err(e) => {
                            eprintln!("FAIL recycle: {e}");
                            ok = false;
                        }
                    }
                }
                Ok(p) => {
                    eprintln!("FAIL rename_path: unexpected state {}", p.display());
                    ok = false;
                }
                Err(e) => {
                    eprintln!("FAIL rename_path: {e}");
                    ok = false;
                }
            }
        }

        // Clobber guard: create_file on an existing name must error.
        std::fs::write(scratch.join("dup.txt"), b"x").ok();
        if fileops::create_file(&scratch, "dup.txt").is_err() {
            eprintln!("PASS create_file clobber-guard");
        } else {
            eprintln!("FAIL create_file clobber-guard: overwrote existing file");
            ok = false;
        }

        let _ = std::fs::remove_dir_all(&scratch);
        eprintln!("selftest: {}", if ok { "ALL PASS" } else { "FAILURES" });
        return if ok { 0 } else { 1 };
    }
    #[cfg(not(windows))]
    {
        eprintln!("selftest only runs on Windows");
        1
    }
}

// ---------- Entry ----------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            scan_start,
            scan_cancel,
            list_dir,
            child_count,
            treemap_layout,
            top_n,
            breadcrumb,
            node_summary,
            open_in_explorer,
            open_in_terminal,
            copy_path,
            open_file,
            create_folder,
            create_file,
            rename_node,
            recycle_node,
            list_drives,
            is_elevated,
            relaunch_as_admin,
            find_old_files,
            find_empty_dirs,
        ])
        .setup(|app| {
            // Optional CLI: `treelens --scan <path>` auto-starts a scan after launch.
            // Useful for screenshots, demos, and scheduled scans.
            let args: Vec<String> = std::env::args().collect();
            if let Some(i) = args.iter().position(|a| a == "--scan") {
                if let Some(path) = args.get(i + 1).cloned() {
                    let handle = app.handle().clone();
                    std::thread::spawn(move || {
                        // Brief settle so the WebView is alive before we emit IPC.
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let _ = handle.emit("scan:auto", path);
                    });
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running treelens");
}
