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

export type SearchKind = "all" | "files" | "dirs";

export interface SearchHit {
  idx: number;
  name: string;
  path: string;
  size: number;
  pct_root: number;
  file_count: number;
  mtime: number;
  is_dir: boolean;
  is_reparse: boolean;
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
  tab: number;
  files: number;
  bytes: number;
  dirs: number;
  errors: number;
  elapsed_ms: number;
}

export interface ScanComplete {
  tab: number;
  root_idx: number;
  nodes: number;
  bytes: number;
  files: number;
  dirs: number;
  errors: number;
  duration_ms: number;
  root_path: string;
}

export interface ScanCancelled {
  tab: number;
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

export interface JunkFile {
  path: string;
  size: number;
  mtime: number;
  category: string;
}

export interface JunkReport {
  files: JunkFile[];
  total_files: number;
  total_bytes: number;
  truncated: boolean;
}

export interface DupeGroup {
  size: number;
  sha256: string;
  paths: string[];
  redundant_bytes: number;
}

export interface DupeReport {
  groups: DupeGroup[];
  total_groups: number;
  total_redundant_bytes: number;
  truncated: boolean;
}

export interface MutationResult {
  ok: boolean;
  /** Full path of the created/renamed item. */
  path: string;
  /** The scan root path; re-scan this to reflect the change. */
  rescan_path: string;
}

export interface DeleteResult {
  requested: number;
  deleted: number;
  failed: number;
}

export interface ChecksumSet {
  size: number;
  crc32: string;
  md5: string;
  sha1: string;
  sha256: string;
}

export interface CompareResult {
  identical: boolean;
  size_a: number;
  size_b: number;
  first_diff_offset: number | null;
  length_only_diff: boolean;
  sha256_a: string;
  sha256_b: string;
}

export type StegoMethod = "lsb" | "whitespace" | "format_append";

export interface StegoFinding {
  method: StegoMethod;
  suspicious: boolean;
  confidence: number;
  statistical_anomaly: boolean;
  detail: string;
  recoverable_bytes: number | null;
}

export interface StegoReport {
  path: string;
  findings: StegoFinding[];
}

export interface StegoExtract {
  text: string | null;
  bytes: number[];
  len: number;
}

// We use a `bigint`-tolerant number coercion since Tauri serializes u64 as number
// in JSON and may overflow at >2^53. v0.1 accepts the JS-number cap because no
// individual file or directory should be >9 PB.

export const ipc = {
  /** Scan into a specific tab id (defaults to the active tab). */
  scanStart(path: string, tab: number = activeTab) {
    return invoke<void>("scan_start", { path, tab });
  },
  scanCancel() {
    return invoke<void>("scan_cancel");
  },
  closeTab(tab: number) {
    return invoke<void>("close_tab", { tab });
  },
  listDir(parent: number, sort: SortKey, offset: number, limit: number, sizeMode: SizeMode) {
    return invoke<DirRow[]>("list_dir", { tab: activeTab, parent, sort, offset, limit, sizeMode });
  },
  childCount(parent: number) {
    return invoke<number>("child_count", { tab: activeTab, parent });
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
      tab: activeTab,
      root,
      width,
      height,
      minPx,
      maxDepth,
      sizeMode,
    });
  },
  topN(root: number, n: number, sizeMode: SizeMode) {
    return invoke<TopN>("top_n", { tab: activeTab, root, n, sizeMode });
  },
  breadcrumb(idx: number) {
    return invoke<BreadcrumbEntry[]>("breadcrumb", { tab: activeTab, idx });
  },
  nodeSummary(idx: number) {
    return invoke<NodeSummary>("node_summary", { tab: activeTab, idx });
  },
  search(
    root: number,
    query: string,
    minSize: number,
    kind: SearchKind,
    limit: number,
    sizeMode: SizeMode,
  ) {
    return invoke<SearchHit[]>("search", {
      tab: activeTab,
      root,
      query,
      minSize,
      kind,
      limit,
      sizeMode,
    });
  },
  openInExplorer(idx: number) {
    return invoke<void>("open_in_explorer", { tab: activeTab, idx });
  },
  openInTerminal(idx: number) {
    return invoke<void>("open_in_terminal", { tab: activeTab, idx });
  },
  copyPath(idx: number) {
    return invoke<string>("copy_path", { tab: activeTab, idx });
  },
  recycleNode(idx: number) {
    return invoke<{ ok: boolean; affected_idx: number; path: string }>("recycle_node", {
      payload: { tab: activeTab, idx },
    });
  },
  recycleNodes(idxs: number[]) {
    return invoke<number>("recycle_nodes", { tab: activeTab, idxs });
  },
  deletePermanentNodes(idxs: number[]) {
    return invoke<DeleteResult>("delete_permanent_nodes", { tab: activeTab, idxs });
  },
  openFile(idx: number) {
    return invoke<void>("open_file", { tab: activeTab, idx });
  },
  createFolder(idx: number, name: string) {
    return invoke<MutationResult>("create_folder", { tab: activeTab, idx, name });
  },
  createFile(idx: number, name: string) {
    return invoke<MutationResult>("create_file", { tab: activeTab, idx, name });
  },
  renameNode(idx: number, newName: string) {
    return invoke<MutationResult>("rename_node", { tab: activeTab, idx, newName });
  },
  checksumNode(idx: number) {
    return invoke<ChecksumSet>("checksum_node", { tab: activeTab, idx });
  },
  compareNodes(idxA: number, idxB: number) {
    return invoke<CompareResult>("compare_nodes", { tab: activeTab, idxA, idxB });
  },
  stegoScan(idx: number) {
    return invoke<StegoReport>("stego_scan", { tab: activeTab, idx });
  },
  stegoExtract(idx: number, method: StegoMethod) {
    return invoke<StegoExtract>("stego_extract", { tab: activeTab, idx, method });
  },
  stegoEmbed(idx: number, method: StegoMethod, payload: string) {
    return invoke<MutationResult>("stego_embed", { tab: activeTab, idx, method, payload });
  },
  saveBytes(path: string, bytes: number[]) {
    return invoke<void>("save_bytes", { path, bytes });
  },
  exportTree(root: number, format: "csv" | "json", dest: string) {
    return invoke<number>("export_tree", { tab: activeTab, root, format, dest });
  },
  findDuplicates(root: number, minSize: number) {
    return invoke<DupeReport>("find_duplicates", { tab: activeTab, root, minSize });
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
      tab: activeTab,
      idx,
      cutoffUnixSecs,
      minSize,
      limit,
    });
  },
  findEmptyDirs(idx: number, limit: number) {
    return invoke<string[]>("find_empty_dirs", { tab: activeTab, idx, limit });
  },
  findJunk(idx: number, limit: number) {
    return invoke<JunkReport>("find_junk", { tab: activeTab, idx, limit });
  },
  recyclePaths(paths: string[]) {
    return invoke<number>("recycle_paths", { paths });
  },
  deletePermanentPaths(paths: string[]) {
    return invoke<number>("delete_permanent_paths", { paths });
  },
};

/** Active tab id, injected into every per-tab command. main.ts updates this
 *  when the user switches tabs. */
let activeTab = 0;
export function setActiveTab(id: number) {
  activeTab = id;
}

export function onScanProgress(cb: (p: ScanProgress) => void): Promise<UnlistenFn> {
  return listen<ScanProgress>("scan:progress", (e) => cb(e.payload));
}
export function onScanComplete(cb: (p: ScanComplete) => void): Promise<UnlistenFn> {
  return listen<ScanComplete>("scan:complete", (e) => cb(e.payload));
}
export function onScanCancelled(cb: (p: ScanCancelled) => void): Promise<UnlistenFn> {
  return listen<ScanCancelled>("scan:cancelled", (e) => cb(e.payload));
}
export function onScanAuto(cb: (path: string) => void): Promise<UnlistenFn> {
  return listen<string>("scan:auto", (e) => cb(e.payload));
}
