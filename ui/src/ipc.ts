/**
 * Typed Tauri IPC wrappers for Treelens.
 *
 * The Rust side owns the tree; this module hides the invoke names and shapes the
 * results into the data types the UI components consume.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type SizeMode = "allocated" | "logical";
export type SortKey =
  | "size_desc"
  | "size_asc"
  | "name_asc"
  | "name_desc"
  | "mtime_desc"
  | "mtime_asc"
  | "count_desc";

export interface DirRow {
  idx: number;
  name: string;
  size: number;
  pct_parent: number;
  pct_root: number;
  file_count: number;
  mtime: number;
  is_dir: boolean;
  is_reparse: boolean;
}

export interface Rect {
  idx: number;
  x: number;
  y: number;
  w: number;
  h: number;
  size: number;
  newest_mtime: number;
  oldest_mtime: number;
  is_dir: boolean;
  depth: number;
  name: string;
}

export interface BreadcrumbEntry {
  idx: number;
  name: string;
}

export interface NodeSummary {
  idx: number;
  name: string;
  full_path: string;
  is_dir: boolean;
  is_reparse: boolean;
  allocated: number;
  logical: number;
  file_count: number;
  dir_count: number;
  mtime: number;
  newest_mtime: number;
  oldest_mtime: number;
}

export interface DriveEntry {
  letter: string;
  label: string | null;
  total: number;
  free: number;
  fs: string;
}

export interface ElevationStatus {
  elevated: boolean;
}

export interface ScanProgress {
  files: number;
  bytes: number;
  dirs: number;
  errors: number;
  elapsed_ms: number;
}

export interface ScanComplete {
  root_idx: number;
  nodes: number;
  bytes: number;
  files: number;
  dirs: number;
  duration_ms: number;
  root_path: string;
}

export interface TopN {
  files: DirRow[];
  dirs: DirRow[];
}

export interface OldFile {
  path: string;
  size: number;
  mtime: number;
}

// We use a `bigint`-tolerant number coercion since Tauri serializes u64 as number
// in JSON and may overflow at >2^53. v0.1 accepts the JS-number cap because no
// individual file or directory should be >9 PB.

export const ipc = {
  scanStart(path: string) {
    return invoke<void>("scan_start", { path });
  },
  scanCancel() {
    return invoke<void>("scan_cancel");
  },
  listDir(parent: number, sort: SortKey, offset: number, limit: number, sizeMode: SizeMode) {
    return invoke<DirRow[]>("list_dir", { parent, sort, offset, limit, sizeMode });
  },
  childCount(parent: number) {
    return invoke<number>("child_count", { parent });
  },
  treemapLayout(
    root: number,
    width: number,
    height: number,
    minPx: number,
    maxDepth: number,
    sizeMode: SizeMode,
  ) {
    return invoke<Rect[]>("treemap_layout", {
      root,
      width,
      height,
      minPx,
      maxDepth,
      sizeMode,
    });
  },
  topN(root: number, n: number, sizeMode: SizeMode) {
    return invoke<TopN>("top_n", { root, n, sizeMode });
  },
  breadcrumb(idx: number) {
    return invoke<BreadcrumbEntry[]>("breadcrumb", { idx });
  },
  nodeSummary(idx: number) {
    return invoke<NodeSummary>("node_summary", { idx });
  },
  openInExplorer(idx: number) {
    return invoke<void>("open_in_explorer", { idx });
  },
  openInTerminal(idx: number) {
    return invoke<void>("open_in_terminal", { idx });
  },
  copyPath(idx: number) {
    return invoke<string>("copy_path", { idx });
  },
  recycleNode(idx: number) {
    return invoke<{ ok: boolean; affected_idx: number; path: string }>("recycle_node", {
      payload: { idx },
    });
  },
  listDrives() {
    return invoke<DriveEntry[]>("list_drives");
  },
  isElevated() {
    return invoke<ElevationStatus>("is_elevated");
  },
  relaunchAsAdmin() {
    return invoke<void>("relaunch_as_admin");
  },
  findOldFiles(idx: number, cutoffUnixSecs: number, minSize: number, limit: number) {
    return invoke<OldFile[]>("find_old_files", {
      idx,
      cutoffUnixSecs,
      minSize,
      limit,
    });
  },
  findEmptyDirs(idx: number, limit: number) {
    return invoke<string[]>("find_empty_dirs", { idx, limit });
  },
};

export function onScanProgress(cb: (p: ScanProgress) => void): Promise<UnlistenFn> {
  return listen<ScanProgress>("scan:progress", (e) => cb(e.payload));
}
export function onScanComplete(cb: (p: ScanComplete) => void): Promise<UnlistenFn> {
  return listen<ScanComplete>("scan:complete", (e) => cb(e.payload));
}
export function onScanCancelled(cb: () => void): Promise<UnlistenFn> {
  return listen<void>("scan:cancelled", () => cb());
}
export function onScanAuto(cb: (path: string) => void): Promise<UnlistenFn> {
  return listen<string>("scan:auto", (e) => cb(e.payload));
}
