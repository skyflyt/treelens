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
use tree::{DirRow, LayoutOpts, Rect, SearchKind, SearchOpts, SizeMode, SortKey, Tree};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ScanProgressPayload {
    tab: u32,
    files: u64,
    bytes: u64,
    dirs: u64,
    errors: u64,
    elapsed_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ScanCompletePayload {
    tab: u32,
    root_idx: u32,
    nodes: u32,
    bytes: u64,
    files: u64,
    dirs: u64,
    errors: u64,
    duration_ms: u64,
    root_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ScanCancelledPayload {
    tab: u32,
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
    tab: u32,
    excludes: Option<Vec<String>>,
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
    let mut opts = ScanOptions::new(root.clone());
    opts.excludes = excludes.unwrap_or_default();

    // Channel sizes: records can be high-volume; events are sparse.
    let (rec_rx, evt_rx, handle) = scanner::spawn(opts, cancel.clone(), 8192, 32);

    *state.scan.lock() = Some(ScanState {
        cancel: cancel.clone(),
        path: root.clone(),
    });

    let app_for_collector = app.clone();
    let state_arc = state.inner().clone_handle();
    thread::spawn(move || {
        collect_scan(
            app_for_collector,
            state_arc,
            tab,
            root,
            rec_rx,
            evt_rx,
            handle,
        );
    });

    Ok(())
}

/// Close a tab, freeing its scanned tree.
#[tauri::command]
fn close_tab(tab: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    state.tabs.write().remove(&tab);
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

#[allow(clippy::too_many_arguments)]
fn collect_scan(
    app: AppHandle,
    state: Arc<AppStateInner>,
    tab: u32,
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
                            tab,
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
        let _ = app.emit("scan:cancelled", ScanCancelledPayload { tab });
        return;
    }

    let errors = last_progress.as_ref().map(|p| p.errors).unwrap_or(0);
    let tree = Tree::build(records);
    let root_idx = tree.root;
    let bytes = tree.nodes[root_idx as usize].allocated;
    let files = tree.nodes[root_idx as usize].file_count;
    let dirs = tree.nodes[root_idx as usize].dir_count;
    let nodes = tree.nodes.len() as u32;
    state.tabs.write().insert(
        tab,
        TabData {
            tree,
            scan_path: root.clone(),
        },
    );
    *state.last_progress.lock() = last_progress;
    let payload = ScanCompletePayload {
        tab,
        root_idx,
        nodes,
        bytes,
        files,
        dirs,
        errors,
        duration_ms,
        root_path: root.to_string_lossy().to_string(),
    };
    let _ = app.emit("scan:complete", payload);
}

/// Run a read-only closure against the tree of tab `tab`. Central place that
/// resolves a tab id to its tree (or a "no scan loaded" error).
fn with_tree<T>(
    state: &AppStateInner,
    tab: u32,
    f: impl FnOnce(&Tree) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    let tabs = state.tabs.read();
    let td = tabs.get(&tab).ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    f(&td.tree)
}

#[tauri::command]
fn list_dir(
    tab: u32,
    parent: u32,
    sort: String,
    offset: usize,
    limit: usize,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<DirRow>, CommandError> {
    let _timer = TimedSpan::new("list_dir");
    with_tree(state.inner(), tab, |tree| {
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
    })
}

#[tauri::command]
fn child_count(tab: u32, parent: u32, state: State<'_, AppState>) -> Result<usize, CommandError> {
    with_tree(state.inner(), tab, |tree| Ok(tree::dir_count(tree, parent)))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn treemap_layout(
    tab: u32,
    root: u32,
    width: f32,
    height: f32,
    min_px: f32,
    max_depth: u16,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<Rect>, CommandError> {
    let _timer = TimedSpan::new("treemap_layout");
    with_tree(state.inner(), tab, |tree| {
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
    })
}

#[derive(Debug, Serialize)]
struct TopNResponse {
    files: Vec<DirRow>,
    dirs: Vec<DirRow>,
}

#[tauri::command]
fn top_n(
    tab: u32,
    root: u32,
    n: usize,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<TopNResponse, CommandError> {
    let _timer = TimedSpan::new("top_n");
    with_tree(state.inner(), tab, |tree| {
        let mode = parse_mode(&size_mode);
        Ok(TopNResponse {
            files: tree::top_files(tree, root, n, mode),
            dirs: tree::top_dirs(tree, root, n, mode),
        })
    })
}

#[derive(Debug, Serialize)]
struct BreadcrumbEntry {
    idx: u32,
    name: String,
}

#[tauri::command]
fn breadcrumb(
    tab: u32,
    idx: u32,
    state: State<'_, AppState>,
) -> Result<Vec<BreadcrumbEntry>, CommandError> {
    with_tree(state.inner(), tab, |tree| {
        Ok(tree
            .path(idx)
            .into_iter()
            .map(|(i, n)| BreadcrumbEntry { idx: i, name: n })
            .collect())
    })
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
fn node_summary(
    tab: u32,
    idx: u32,
    state: State<'_, AppState>,
) -> Result<NodeSummary, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    with_tree(state.inner(), tab, |tree| {
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
    })
}

#[derive(Debug, Serialize)]
struct SearchHit {
    idx: u32,
    name: String,
    /// Absolute path, so a result anywhere in the subtree is unambiguous.
    path: String,
    size: u64,
    pct_root: f32,
    file_count: u64,
    mtime: i64,
    is_dir: bool,
    is_reparse: bool,
}

fn parse_search_kind(s: &str) -> SearchKind {
    match s {
        "files" => SearchKind::FilesOnly,
        "dirs" => SearchKind::DirsOnly,
        _ => SearchKind::All,
    }
}

/// Search the subtree under `root` for nodes matching a name substring and the
/// size/kind filters, returning the largest `limit` matches with full paths.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn search(
    tab: u32,
    root: u32,
    query: String,
    min_size: u64,
    kind: String,
    limit: usize,
    size_mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<SearchHit>, CommandError> {
    let _timer = TimedSpan::new("search");
    let tabs = state.tabs.read();
    let td = tabs.get(&tab).ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    if root as usize >= td.tree.nodes.len() {
        return Err(CommandError {
            message: format!("invalid root {root}"),
        });
    }
    let mode = parse_mode(&size_mode);
    let opts = SearchOpts {
        query,
        min_size,
        kind: parse_search_kind(&kind),
        limit: limit.clamp(1, 5000),
    };
    let rows = tree::search(&td.tree, root, &opts, mode);
    let hits = rows
        .into_iter()
        .map(|r| {
            // Build the absolute path the same way node_path does: drop the
            // root node's recorded name and join the rest onto the scan root.
            let mut segments: Vec<String> =
                td.tree.path(r.idx).into_iter().map(|(_, n)| n).collect();
            if !segments.is_empty() {
                segments.remove(0);
            }
            let mut p = td.scan_path.clone();
            for seg in segments {
                p.push(seg);
            }
            SearchHit {
                idx: r.idx,
                name: r.name,
                path: p.to_string_lossy().to_string(),
                size: r.size,
                pct_root: r.pct_root,
                file_count: r.file_count,
                mtime: r.mtime,
                is_dir: r.is_dir,
                is_reparse: r.is_reparse,
            }
        })
        .collect();
    Ok(hits)
}

/// Aggregate file sizes/counts by extension across the subtree under `root`.
#[tauri::command]
fn extension_breakdown(
    tab: u32,
    root: u32,
    size_mode: String,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<tree::ExtStat>, CommandError> {
    let _timer = TimedSpan::new("extension_breakdown");
    with_tree(state.inner(), tab, |tree| {
        if root as usize >= tree.nodes.len() {
            return Err(CommandError {
                message: format!("invalid root {root}"),
            });
        }
        Ok(tree::extension_breakdown(
            tree,
            root,
            parse_mode(&size_mode),
            limit.clamp(1, 1000),
        ))
    })
}

/// Export the subtree under `root` to `dest` as CSV or JSON. Walks iteratively
/// and streams rows to a buffered writer so even a whole-drive tree exports
/// without building a giant in-memory string. Returns the number of rows written.
#[tauri::command]
fn export_tree(
    tab: u32,
    root: u32,
    format: String,
    dest: String,
    state: State<'_, AppState>,
) -> Result<usize, CommandError> {
    use std::io::{BufWriter, Write};
    let is_json = format.eq_ignore_ascii_case("json");

    let tabs = state.tabs.read();
    let td = tabs.get(&tab).ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    if root as usize >= td.tree.nodes.len() {
        return Err(CommandError {
            message: format!("invalid root {root}"),
        });
    }

    let file = std::fs::File::create(&dest).map_err(|e| CommandError {
        message: format!("cannot create {dest}: {e}"),
    })?;
    let mut w = BufWriter::new(file);
    let mut count = 0usize;

    let write_err = |e: std::io::Error| CommandError {
        message: format!("write failed: {e}"),
    };

    // The export path for the root node is the actual scan path; descendants are
    // built incrementally (parent path + name) to avoid an O(depth) re-walk per
    // node. Stack holds (idx, full_path).
    let root_path = td.scan_path.to_string_lossy().to_string();

    if is_json {
        w.write_all(b"[").map_err(write_err)?;
    } else {
        w.write_all(b"path,name,type,allocated,logical,mtime,file_count,dir_count\r\n")
            .map_err(write_err)?;
    }

    let mut stack: Vec<(u32, String)> = vec![(root, root_path)];
    while let Some((idx, path)) = stack.pop() {
        let n = &td.tree.nodes[idx as usize];
        let kind = if n.is_reparse() {
            "reparse"
        } else if n.is_dir() {
            "dir"
        } else {
            "file"
        };
        if is_json {
            if count > 0 {
                w.write_all(b",").map_err(write_err)?;
            }
            // serde_json handles all string escaping correctly.
            let obj = serde_json::json!({
                "path": path,
                "name": n.name,
                "type": kind,
                "allocated": n.allocated,
                "logical": n.logical,
                "mtime": n.mtime,
                "file_count": n.file_count,
                "dir_count": n.dir_count,
            });
            serde_json::to_writer(&mut w, &obj).map_err(|e| CommandError {
                message: format!("json encode failed: {e}"),
            })?;
        } else {
            let row = format!(
                "{},{},{},{},{},{},{},{}\r\n",
                csv_field(&path),
                csv_field(&n.name),
                kind,
                n.allocated,
                n.logical,
                n.mtime,
                n.file_count,
                n.dir_count,
            );
            w.write_all(row.as_bytes()).map_err(write_err)?;
        }
        count += 1;

        // Push children with their precomputed paths.
        for c in td.tree.child_indexes(idx) {
            let cname = &td.tree.nodes[c as usize].name;
            let mut cpath = path.clone();
            if !cpath.ends_with(['\\', '/']) {
                cpath.push('\\');
            }
            cpath.push_str(cname);
            stack.push((c, cpath));
        }
    }

    if is_json {
        w.write_all(b"]").map_err(write_err)?;
    }
    w.flush().map_err(write_err)?;
    Ok(count)
}

/// Find duplicate files (byte-identical) in the subtree under `root`. Gathers
/// candidate files (≥ `min_size`) with their absolute paths, then runs the
/// size → prefix-hash → full-hash duplicate funnel.
#[tauri::command]
fn find_duplicates(
    tab: u32,
    root: u32,
    min_size: u64,
    state: State<'_, AppState>,
) -> Result<analysis::DupeReport, CommandError> {
    let _timer = TimedSpan::new("find_duplicates");
    let tabs = state.tabs.read();
    let td = tabs.get(&tab).ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    if root as usize >= td.tree.nodes.len() {
        return Err(CommandError {
            message: format!("invalid root {root}"),
        });
    }

    // Gather (path, size) for regular files at/under root, building paths
    // incrementally (parent path + name) to avoid an O(depth) re-walk per file.
    let root_path = td.scan_path.to_string_lossy().to_string();
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<(u32, String)> = vec![(root, root_path)];
    while let Some((idx, path)) = stack.pop() {
        let n = &td.tree.nodes[idx as usize];
        if !n.is_dir() && !n.is_reparse() && n.logical >= min_size {
            files.push((path.clone(), n.logical));
        }
        for c in td.tree.child_indexes(idx) {
            let cname = &td.tree.nodes[c as usize].name;
            let mut cpath = path.clone();
            if !cpath.ends_with(['\\', '/']) {
                cpath.push('\\');
            }
            cpath.push_str(cname);
            stack.push((c, cpath));
        }
    }

    Ok(analysis::find_duplicates(&files, 2000))
}

/// Quote a CSV field if it contains a comma, quote, CR, or LF (RFC 4180).
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Resolve a node idx within tab `tab` to its absolute filesystem path.
fn node_path(state: &AppStateInner, tab: u32, idx: u32) -> Result<PathBuf, CommandError> {
    let tabs = state.tabs.read();
    let td = tabs.get(&tab).ok_or_else(|| CommandError {
        message: "no scan loaded".into(),
    })?;
    let mut segments: Vec<String> = td.tree.path(idx).into_iter().map(|(_, n)| n).collect();
    // The first segment is the root node's recorded name; drop it and join the
    // rest onto the actual scan root (handles "C:\" vs name="C:").
    if !segments.is_empty() {
        segments.remove(0);
    }
    let mut p = td.scan_path.clone();
    for seg in segments {
        p.push(seg);
    }
    Ok(p)
}

fn rescan_path_of(state: &AppStateInner, tab: u32) -> Result<String, CommandError> {
    let tabs = state.tabs.read();
    tabs.get(&tab)
        .map(|td| td.scan_path.to_string_lossy().to_string())
        .ok_or_else(|| CommandError {
            message: "no active scan".into(),
        })
}

#[tauri::command]
fn open_in_explorer(tab: u32, idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    fileops::open_in_explorer(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(())
}

#[tauri::command]
fn open_in_terminal(tab: u32, idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    fileops::open_in_terminal(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(())
}

#[tauri::command]
fn copy_path(tab: u32, idx: u32, state: State<'_, AppState>) -> Result<String, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    Ok(path.to_string_lossy().to_string())
}

/// Open a file in its default application (the "edit" action).
#[tauri::command]
fn open_file(tab: u32, idx: u32, state: State<'_, AppState>) -> Result<(), CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
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

/// Create a new folder inside the directory identified by `idx`.
#[tauri::command]
fn create_folder(
    tab: u32,
    idx: u32,
    name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let dir = node_path(state.inner(), tab, idx)?;
    let created = fileops::create_folder(&dir, &name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: created.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner(), tab)?,
    })
}

/// Create a new empty file inside the directory identified by `idx`.
#[tauri::command]
fn create_file(
    tab: u32,
    idx: u32,
    name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let dir = node_path(state.inner(), tab, idx)?;
    let created = fileops::create_file(&dir, &name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: created.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner(), tab)?,
    })
}

/// Rename the file or folder identified by `idx` to `new_name` (bare segment).
#[tauri::command]
fn rename_node(
    tab: u32,
    idx: u32,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    let renamed = fileops::rename_path(&path, &new_name).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: renamed.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner(), tab)?,
    })
}

// ---------- Analysis: checksums, compare, steganography ----------

/// Compute CRC32 / MD5 / SHA-1 / SHA-256 of a file.
#[tauri::command]
fn checksum_node(
    tab: u32,
    idx: u32,
    state: State<'_, AppState>,
) -> Result<analysis::ChecksumSet, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    analysis::checksum_file(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

/// Byte-compare two nodes (by idx).
#[tauri::command]
fn compare_nodes(
    tab: u32,
    idx_a: u32,
    idx_b: u32,
    state: State<'_, AppState>,
) -> Result<analysis::CompareResult, CommandError> {
    let a = node_path(state.inner(), tab, idx_a)?;
    let b = node_path(state.inner(), tab, idx_b)?;
    analysis::compare_files(&a, &b).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

/// Run all steganography detectors on a file.
#[tauri::command]
fn stego_scan(
    tab: u32,
    idx: u32,
    state: State<'_, AppState>,
) -> Result<analysis::stego::ScanReport, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    Ok(analysis::stego::scan(&path))
}

fn parse_method(s: &str) -> std::result::Result<analysis::stego::Method, CommandError> {
    use analysis::stego::Method;
    match s {
        "lsb" => Ok(Method::Lsb),
        "whitespace" => Ok(Method::Whitespace),
        "format_append" => Ok(Method::FormatAppend),
        other => Err(CommandError {
            message: format!("unknown stego method: {other}"),
        }),
    }
}

#[derive(Debug, Serialize)]
struct ExtractResult {
    /// The recovered payload as UTF-8 if it's valid text, else null.
    text: Option<String>,
    /// Recovered payload as raw bytes (always present on success).
    bytes: Vec<u8>,
    len: usize,
}

/// Extract a hidden payload from a file using the given method.
#[tauri::command]
fn stego_extract(
    tab: u32,
    idx: u32,
    method: String,
    state: State<'_, AppState>,
) -> Result<ExtractResult, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    let m = parse_method(&method)?;
    let bytes = analysis::stego::extract(m, &path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    let text = String::from_utf8(bytes.clone()).ok();
    Ok(ExtractResult {
        len: bytes.len(),
        text,
        bytes,
    })
}

/// Embed a UTF-8 payload into `idx` using `method`, writing a new file beside
/// the source (so the original is never modified). Returns the output path and
/// triggers a re-scan.
#[tauri::command]
fn stego_embed(
    tab: u32,
    idx: u32,
    method: String,
    payload: String,
    state: State<'_, AppState>,
) -> Result<MutationResult, CommandError> {
    let src = node_path(state.inner(), tab, idx)?;
    let m = parse_method(&method)?;
    // Output: <stem>.stego.<ext> next to the source — never overwrite the input.
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".into());
    let ext = src
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let out = src.with_file_name(format!("{stem}.stego{ext}"));
    analysis::stego::embed(m, &src, &out, payload.as_bytes()).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(MutationResult {
        ok: true,
        path: out.to_string_lossy().to_string(),
        rescan_path: rescan_path_of(state.inner(), tab)?,
    })
}

/// Save an extracted payload to a chosen path.
#[tauri::command]
fn save_bytes(path: String, bytes: Vec<u8>) -> Result<(), CommandError> {
    std::fs::write(&path, &bytes).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

/// Candidate locations for the portable settings file, most-preferred first:
/// next to the executable (true portable mode), then %APPDATA%\Treelens
/// (fallback for per-machine installs under Program Files where the exe dir
/// isn't writable).
fn config_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("treelens.config.json"));
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        v.push(PathBuf::from(appdata).join("Treelens").join("config.json"));
    }
    v
}

/// Load the persisted UI settings JSON (empty string if none exists yet).
#[tauri::command]
fn load_config() -> Result<String, CommandError> {
    for p in config_candidates() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Ok(s);
        }
    }
    Ok(String::new())
}

/// Persist the UI settings JSON to the first writable candidate location.
#[tauri::command]
fn save_config(json: String) -> Result<(), CommandError> {
    let mut last_err = String::from("no writable config location");
    for p in config_candidates() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&p, &json) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = format!("{}: {e}", p.display()),
        }
    }
    Err(CommandError { message: last_err })
}

#[derive(Debug, Deserialize)]
struct RecyclePayload {
    tab: u32,
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
    let path = node_path(state.inner(), payload.tab, payload.idx)?;
    fileops::recycle(&path).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(RecycleResult {
        ok: true,
        affected_idx: payload.idx,
        path: path.to_string_lossy().to_string(),
    })
}

/// Recycle several nodes at once (multi-select bulk delete). Resolves each idx
/// to a path and sends them to the Recycle Bin in one shell operation.
#[tauri::command]
fn recycle_nodes(
    tab: u32,
    idxs: Vec<u32>,
    state: State<'_, AppState>,
) -> Result<u32, CommandError> {
    let mut paths = Vec::with_capacity(idxs.len());
    for idx in &idxs {
        paths.push(node_path(state.inner(), tab, *idx)?);
    }
    fileops::recycle_many(&paths).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(paths.len() as u32)
}

#[derive(Debug, Serialize)]
struct DeleteResult {
    requested: u32,
    deleted: u32,
    failed: u32,
}

/// PERMANENTLY delete one or more nodes (bypasses the Recycle Bin —
/// unrecoverable). The frontend gates this behind an explicit confirmation.
/// Returns how many were actually removed vs. how many survived (e.g. locked
/// files held open by another program), so the UI can report honestly.
#[tauri::command]
fn delete_permanent_nodes(
    tab: u32,
    idxs: Vec<u32>,
    state: State<'_, AppState>,
) -> Result<DeleteResult, CommandError> {
    let mut paths = Vec::with_capacity(idxs.len());
    for idx in &idxs {
        paths.push(node_path(state.inner(), tab, *idx)?);
    }
    let requested = paths.len() as u32;
    let deleted = fileops::delete_permanent_many(&paths).map_err(|e| CommandError {
        message: format!("{e}"),
    })? as u32;
    Ok(DeleteResult {
        requested,
        deleted,
        failed: requested - deleted,
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
    tab: u32,
    idx: u32,
    cutoff_unix_secs: i64,
    min_size: u64,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<fileops::OldFile>, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    fileops::find_old_files(&path, cutoff_unix_secs, min_size, limit).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

/// Find reclaimable junk (logs / temp / dumps / backups / empty files) under a
/// node's subtree.
#[tauri::command]
fn find_junk(
    tab: u32,
    idx: u32,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<fileops::JunkReport, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
    fileops::find_junk(&path, limit).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

/// Recycle a list of explicit paths (used by the junk finder, whose results are
/// raw paths rather than tree node idxs). Returns how many are gone afterward.
#[tauri::command]
fn recycle_paths(paths: Vec<String>) -> Result<usize, CommandError> {
    let pbufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    fileops::recycle_many(&pbufs).map_err(|e| CommandError {
        message: format!("{e}"),
    })?;
    Ok(pbufs.iter().filter(|p| !p.exists()).count())
}

/// Permanently delete a list of explicit paths. Returns how many are gone.
#[tauri::command]
fn delete_permanent_paths(paths: Vec<String>) -> Result<usize, CommandError> {
    let pbufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    fileops::delete_permanent_many(&pbufs).map_err(|e| CommandError {
        message: format!("{e}"),
    })
}

#[tauri::command]
fn find_empty_dirs(
    tab: u32,
    idx: u32,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<String>, CommandError> {
    let path = node_path(state.inner(), tab, idx)?;
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

/// One scanned tree plus the path it was scanned from. One per UI tab.
pub struct TabData {
    pub tree: Tree,
    pub scan_path: PathBuf,
}

pub struct AppStateInner {
    pub scan: Mutex<Option<ScanState>>,
    /// Per-tab scanned trees, keyed by the frontend's tab id. Each tab holds an
    /// independent scan. Reads dominate (every query is a read), and an
    /// `RwLock` lets the 4 parallel queries on a drill run concurrently; the
    /// only writer is the scan collector replacing one tab's tree at scan end.
    pub tabs: RwLock<std::collections::HashMap<u32, TabData>>,
    pub last_progress: Mutex<Option<scanner::Progress>>,
}

impl AppStateInner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            scan: Mutex::new(None),
            tabs: RwLock::new(std::collections::HashMap::new()),
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

        // Permanent delete (bypasses recycle bin).
        let perm = scratch.join("perm.txt");
        std::fs::write(&perm, b"delete me forever").ok();
        match fileops::delete_permanent(&perm) {
            Ok(true) if !perm.exists() => eprintln!("PASS delete_permanent -> gone from disk"),
            Ok(_) => {
                eprintln!("FAIL delete_permanent: still on disk");
                ok = false;
            }
            Err(e) => {
                eprintln!("FAIL delete_permanent: {e}");
                ok = false;
            }
        }

        // --- Analysis: checksums, compare, and stego round-trips ---
        // (scratch dir still exists from the file-op phase; reuse it.)
        ok &= selftest_analysis(&scratch);

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

/// Analysis-engine self-test: checksum vector, byte compare, and an
/// embed→detect→extract→verify round-trip for each stego method.
#[cfg(windows)]
fn selftest_analysis(scratch: &std::path::Path) -> bool {
    use analysis::stego::{self, Method};
    let mut ok = true;

    // Checksum known vector ("abc").
    let abc = scratch.join("abc.txt");
    let _ = std::fs::write(&abc, b"abc");
    match analysis::checksum_file(&abc) {
        Ok(c) if c.sha256 == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad" => {
            eprintln!("PASS checksum sha256(abc)")
        }
        Ok(c) => {
            eprintln!("FAIL checksum: {}", c.sha256);
            ok = false;
        }
        Err(e) => {
            eprintln!("FAIL checksum: {e}");
            ok = false;
        }
    }

    // Compare: identical vs different.
    let f1 = scratch.join("c1.bin");
    let f2 = scratch.join("c2.bin");
    let _ = std::fs::write(&f1, b"hello world");
    let _ = std::fs::write(&f2, b"hello WORLD");
    match analysis::compare_files(&f1, &f2) {
        Ok(r) if !r.identical && r.first_diff_offset == Some(6) => {
            eprintln!("PASS compare first-diff@6")
        }
        Ok(r) => {
            eprintln!(
                "FAIL compare: identical={} off={:?}",
                r.identical, r.first_diff_offset
            );
            ok = false;
        }
        Err(e) => {
            eprintln!("FAIL compare: {e}");
            ok = false;
        }
    }

    // Build a small PNG cover for image-based methods.
    let cover = scratch.join("cover.png");
    {
        let mut img = analysis::image::RgbImage::new(48, 48);
        let mut s: u32 = 0xABCD_1234;
        for px in img.pixels_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *px = analysis::image::Rgb([(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8]);
        }
        let _ = img.save(&cover);
    }
    let txt = scratch.join("doc.txt");
    let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let _ = std::fs::write(&txt, body);

    let cases: &[(Method, &std::path::Path, &str, &str)] = &[
        (Method::Lsb, &cover, "lsb", "png"),
        (Method::Whitespace, &txt, "whitespace", "txt"),
        (Method::FormatAppend, &cover, "format_append", "png"),
    ];
    let secret = b"treelens stego self-test payload";
    for (method, src, name, ext) in cases {
        let out = scratch.join(format!("{name}.stego.{ext}"));
        match stego::embed(*method, src, &out, secret).and_then(|_| stego::extract(*method, &out)) {
            Ok(got) if got == secret => {
                let report = stego::scan(&out);
                let flagged = report
                    .findings
                    .iter()
                    .any(|f| f.method == *method && f.suspicious);
                if flagged {
                    eprintln!("PASS stego {name}: embed → detect → extract round-trip");
                } else {
                    eprintln!("FAIL stego {name}: extracted ok but detector didn't flag");
                    ok = false;
                }
            }
            Ok(_) => {
                eprintln!("FAIL stego {name}: payload mismatch after round-trip");
                ok = false;
            }
            Err(e) => {
                eprintln!("FAIL stego {name}: {e}");
                ok = false;
            }
        }
    }
    ok
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
            close_tab,
            list_dir,
            child_count,
            treemap_layout,
            top_n,
            breadcrumb,
            node_summary,
            search,
            extension_breakdown,
            export_tree,
            find_duplicates,
            load_config,
            save_config,
            open_in_explorer,
            open_in_terminal,
            copy_path,
            open_file,
            create_folder,
            create_file,
            rename_node,
            checksum_node,
            compare_nodes,
            stego_scan,
            stego_extract,
            stego_embed,
            save_bytes,
            recycle_node,
            recycle_nodes,
            delete_permanent_nodes,
            list_drives,
            is_elevated,
            relaunch_as_admin,
            find_old_files,
            find_empty_dirs,
            find_junk,
            recycle_paths,
            delete_permanent_paths,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_field_quotes_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("he said \"hi\""), "\"he said \"\"hi\"\"\"");
        assert_eq!(csv_field("line1\r\nline2"), "\"line1\r\nline2\"");
        // A Windows path with no special chars stays unquoted.
        assert_eq!(csv_field(r"C:\Users\me\file.txt"), r"C:\Users\me\file.txt");
    }

    #[test]
    fn parse_search_kind_maps_known_values() {
        assert_eq!(parse_search_kind("files"), SearchKind::FilesOnly);
        assert_eq!(parse_search_kind("dirs"), SearchKind::DirsOnly);
        assert_eq!(parse_search_kind("all"), SearchKind::All);
        assert_eq!(parse_search_kind("whatever"), SearchKind::All);
    }
}
