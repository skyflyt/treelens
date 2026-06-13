/**
 * Treelens main entry. Wires DOM events to IPC, manages frontend state,
 * coordinates the treemap canvas and the side panels.
 */
import {
  ipc,
  onScanAuto,
  onScanCancelled,
  onScanComplete,
  onScanProgress,
  setActiveTab,
  type DirRow,
  type Rect,
  type SizeMode,
  type SortKey,
  type StegoMethod,
} from "./ipc";
import { Treemap, type TreemapTheme } from "./treemap";
import { fmtBytes, fmtCount, fmtDuration, fmtMtime } from "./format";
import { type ColorMode } from "./colors";
import { pickScanRoot } from "./drives";

declare const __APP_VERSION__: string;

interface UiState {
  scanRoot: number | null;       // arena idx of the scanned root
  currentRoot: number | null;    // currently-displayed (drilled-in) root
  selectedIdx: number | null;
  rectNames: Map<number, string>;
  scanRootPath: string;
  totals: { files: number; dirs: number; bytes: number; duration_ms: number } | null;
  scanning: boolean;
  sizeMode: SizeMode;
  colorMode: ColorMode;
  sort: SortKey;
  theme: "light" | "dark";
  themeFollowsSystem: boolean;
}

const state: UiState = {
  scanRoot: null,
  currentRoot: null,
  selectedIdx: null,
  rectNames: new Map(),
  scanRootPath: "",
  totals: null,
  scanning: false,
  sizeMode: "allocated",
  colorMode: "type",
  sort: "size_desc",
  theme: prefersDark() ? "dark" : "light",
  themeFollowsSystem: true,
};

function prefersDark(): boolean {
  return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
}

// ---------- tabs ----------
// Each tab owns an independent scan (its tree lives in the Rust backend, keyed
// by tab id). The per-tab *view* fields are snapshotted out of / into `state`
// on switch, so all the existing `state.currentRoot` references keep working —
// they always refer to whichever tab is active.
interface TabSnapshot {
  id: number;
  title: string;
  scanRoot: number | null;
  currentRoot: number | null;
  selectedIdx: number | null;
  scanRootPath: string;
  totals: UiState["totals"];
}
let tabs: TabSnapshot[] = [];
let activeTabId = 0;
let nextTabId = 1;

function snapshotActiveTab() {
  const t = tabs.find((x) => x.id === activeTabId);
  if (!t) return;
  t.scanRoot = state.scanRoot;
  t.currentRoot = state.currentRoot;
  t.selectedIdx = state.selectedIdx;
  t.scanRootPath = state.scanRootPath;
  t.totals = state.totals;
}

function loadTabIntoState(t: TabSnapshot) {
  state.scanRoot = t.scanRoot;
  state.currentRoot = t.currentRoot;
  state.selectedIdx = t.selectedIdx;
  state.scanRootPath = t.scanRootPath;
  state.totals = t.totals;
  state.scanning = false;
  state.rectNames.clear();
  expandedIdxs.clear();
}

function renderTabBar() {
  elScanTabs.innerHTML = "";
  for (const t of tabs) {
    const el = document.createElement("div");
    el.className = "scan-tab" + (t.id === activeTabId ? " active" : "");
    el.innerHTML = `<span class="tab-title">${escapeHtml(t.title)}</span>`;
    el.title = t.scanRootPath || t.title;
    el.addEventListener("click", () => switchToTab(t.id));
    if (tabs.length > 1) {
      const close = document.createElement("button");
      close.className = "tab-close";
      close.textContent = "✕";
      close.title = "Close tab";
      close.addEventListener("click", (e) => {
        e.stopPropagation();
        closeTab(t.id);
      });
      el.appendChild(close);
    }
    elScanTabs.appendChild(el);
  }
}

function createTab(title = "New tab"): TabSnapshot {
  const t: TabSnapshot = {
    id: nextTabId++,
    title,
    scanRoot: null,
    currentRoot: null,
    selectedIdx: null,
    scanRootPath: "",
    totals: null,
  };
  tabs.push(t);
  return t;
}

async function switchToTab(id: number) {
  if (id === activeTabId) return;
  const target = tabs.find((t) => t.id === id);
  if (!target) return;
  snapshotActiveTab();
  activeTabId = id;
  setActiveTab(id); // route subsequent IPC at this tab's tree
  loadTabIntoState(target);
  renderTabBar();
  drillSeq++; // invalidate any in-flight renders from the previous tab
  if (state.currentRoot !== null) {
    await refreshAll(drillSeq);
    elStatusSummary.textContent = state.totals
      ? `${fmtCount(state.totals.files)} files · ${fmtCount(state.totals.dirs)} folders · ${fmtBytes(state.totals.bytes)}`
      : "Ready.";
  } else {
    // Empty tab — show the empty state + clear panels.
    elDirList.innerHTML = "";
    elTopFilesList.innerHTML = "";
    elTopDirsList.innerHTML = "";
    elBreadcrumb.innerHTML = `<span class="hint">No scan yet. Pick a drive or folder to begin.</span>`;
    renderEmptyState();
  }
  (elRescanBtn as HTMLButtonElement).disabled = state.scanRootPath === "";
  (elNewFolderBtn as HTMLButtonElement).disabled = state.currentRoot === null;
  (elNewFileBtn as HTMLButtonElement).disabled = state.currentRoot === null;
}

async function newTab() {
  snapshotActiveTab();
  const t = createTab();
  activeTabId = t.id;
  setActiveTab(t.id);
  loadTabIntoState(t);
  renderTabBar();
  renderEmptyState();
  elDirList.innerHTML = "";
  elTopFilesList.innerHTML = "";
  elTopDirsList.innerHTML = "";
  elBreadcrumb.innerHTML = `<span class="hint">No scan yet. Pick a drive or folder to begin.</span>`;
  // Immediately prompt for what to scan in the new tab.
  const root = await pickScanRoot();
  if (root) startScan(root);
}

function closeTab(id: number) {
  if (tabs.length <= 1) return;
  ipc.closeTab(id).catch(() => {});
  const idx = tabs.findIndex((t) => t.id === id);
  if (idx === -1) return;
  tabs.splice(idx, 1);
  if (activeTabId === id) {
    const neighbor = tabs[Math.max(0, idx - 1)];
    activeTabId = -1; // force switchToTab to run
    switchToTab(neighbor.id);
  } else {
    renderTabBar();
  }
}

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

const elTreemap = $("#treemap") as HTMLCanvasElement;
const elTreemapPane = $("#treemap-pane");
const elTreemapEmpty = $("#treemap-empty");
const elTooltip = $("#treemap-tooltip");
const elScanBtn = $("#scan-btn");
const elEmptyScanBtn = $("#empty-scan-btn");
const elRescanBtn = $("#rescan-btn");
const elNewFolderBtn = $("#new-folder-btn");
const elNewFileBtn = $("#new-file-btn");
const elModeAlloc = $("#mode-allocated");
const elModeLogical = $("#mode-logical");
const elHeatBtn = $("#heat-btn");
const elThemeBtn = $("#theme-btn");
const elBreadcrumb = $("#breadcrumb");
const elStatusSummary = $("#status-summary");
const elScanningOverlay = $("#scanning-overlay");
const elScanFiles = $("#scan-files");
const elScanBytes = $("#scan-bytes");
const elScanElapsed = $("#scan-elapsed");
const elScanCancel = $("#scan-cancel");
const elDirList = $("#dir-list");
const elTopFilesList = $("#topfiles-list");
const elTopDirsList = $("#topdirs-list");
const elAdminBanner = $("#admin-banner");
const elElevateBtn = $("#elevate-btn");
const elDismissBanner = $("#dismiss-banner");
const elCtxMenu = $("#ctx-menu");
const elStatusLoading = $("#status-loading");
const elStatusLoadingText = $("#status-loading-text");
const elStatusVersion = $("#status-version");
const elScanTabs = $("#scan-tabs");
const elNewTabBtn = $("#new-tab-btn");
const elInspector = $("#inspector");

const treemap = new Treemap(
  elTreemap,
  themeForCanvas(),
  {
    onHover: (rect, x, y) => updateTooltip(rect, x, y),
    onClick: (rect) => selectNode(rect.idx),
    onDoubleClick: (rect) => drillInto(rect.idx),
    onContextMenu: (rect, x, y) => openCtxMenu(rect.idx, x, y),
  },
);

// ---------- bootstrap ----------

(async function init() {
  loadConfig();
  applyTheme();

  // Admin banner: show only if not elevated and not dismissed this session.
  ipc.isElevated().then(({ elevated }) => {
    if (!elevated && !sessionStorage.getItem("admin-banner-dismissed")) {
      elAdminBanner.hidden = false;
    }
  }).catch(() => {});

  await onScanProgress(handleScanProgress);
  await onScanComplete(handleScanComplete);
  await onScanCancelled(handleScanCancelled);

  // CLI auto-scan: backend emits `scan:auto` with a path if `--scan <p>` was given.
  onScanAuto((path) => {
    if (typeof path === "string" && path.length > 0) startScan(path);
  });

  // Scan triggers.
  const triggerScan = async () => {
    const root = await pickScanRoot();
    if (root) startScan(root);
  };
  elScanBtn.addEventListener("click", triggerScan);
  elEmptyScanBtn?.addEventListener("click", triggerScan);
  elRescanBtn.addEventListener("click", () => {
    if (state.scanRootPath) startScan(state.scanRootPath);
  });
  // New folder / file act on the current drilled-in directory.
  elNewFolderBtn.addEventListener("click", () => {
    if (state.currentRoot !== null) promptCreate(state.currentRoot, "folder");
  });
  elNewFileBtn.addEventListener("click", () => {
    if (state.currentRoot !== null) promptCreate(state.currentRoot, "file");
  });
  elScanCancel.addEventListener("click", () => {
    ipc.scanCancel().catch(() => {});
  });

  // Size mode toggle.
  elModeAlloc.addEventListener("click", () => setSizeMode("allocated"));
  elModeLogical.addEventListener("click", () => setSizeMode("logical"));

  // Heat-map toggle.
  elHeatBtn.addEventListener("click", () => {
    state.colorMode = state.colorMode === "heat" ? "type" : "heat";
    elHeatBtn.setAttribute("aria-pressed", state.colorMode === "heat" ? "true" : "false");
    treemap.setMode(state.colorMode);
    saveConfig();
  });

  // Theme toggle.
  elThemeBtn.addEventListener("click", () => {
    state.theme = state.theme === "dark" ? "light" : "dark";
    state.themeFollowsSystem = false;
    applyTheme();
    saveConfig();
  });
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", (e) => {
    if (state.themeFollowsSystem) {
      state.theme = e.matches ? "dark" : "light";
      applyTheme();
    }
  });

  // Admin banner actions.
  elElevateBtn.addEventListener("click", () => {
    ipc.relaunchAsAdmin().catch(() => {});
  });
  elDismissBanner.addEventListener("click", () => {
    elAdminBanner.hidden = true;
    sessionStorage.setItem("admin-banner-dismissed", "1");
  });

  // Side-panel view switching (Contents / Top files / Top folders / Inspect).
  document.querySelectorAll(".side-tabs .tab").forEach((t) => {
    t.addEventListener("click", () => {
      const tab = (t as HTMLElement).dataset.tab!;
      document.querySelectorAll(".side-tabs .tab").forEach((x) => x.classList.toggle("active", x === t));
      document.querySelectorAll(".tab-pane").forEach((x) => x.classList.toggle("active", x.id === `tab-${tab}`));
      if (tab === "inspect") refreshInspector();
    });
  });

  // New scan tab.
  elNewTabBtn.addEventListener("click", () => newTab());

  // Initial tab (id 0, matching the backend's default active tab).
  tabs = [];
  nextTabId = 0;
  const first = createTab("Treelens");
  activeTabId = first.id;
  setActiveTab(first.id);
  renderTabBar();

  // Keyboard shortcuts + world-class tree navigation.
  document.addEventListener("keydown", (e) => {
    if (e.target instanceof HTMLInputElement) return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        moveSelection(1);
        return;
      case "ArrowUp":
        e.preventDefault();
        moveSelection(-1);
        return;
      case "ArrowRight":
        e.preventDefault();
        activateSelected(false); // expand, or step into
        return;
      case "ArrowLeft":
        e.preventDefault();
        collapseOrParent();
        return;
      case "PageDown":
        e.preventDefault();
        moveSelection(Math.floor(elDirList.clientHeight / ROW_HEIGHT) || 10);
        return;
      case "PageUp":
        e.preventDefault();
        moveSelection(-(Math.floor(elDirList.clientHeight / ROW_HEIGHT) || 10));
        return;
      case "Home":
        e.preventDefault();
        selectFlatIndex(0);
        return;
      case "End":
        e.preventDefault();
        selectFlatIndex(flatRows.length - 1);
        return;
      case "Backspace":
        e.preventDefault();
        drillUp();
        return;
      case "F5":
        e.preventDefault();
        if (state.scanRootPath) startScan(state.scanRootPath);
        return;
      case "F2":
        e.preventDefault();
        if (state.selectedIdx !== null) promptRename(state.selectedIdx);
        return;
      case "Delete":
        e.preventDefault();
        if (state.selectedIdx !== null) confirmRecycle(state.selectedIdx);
        return;
      case "Enter":
        e.preventDefault();
        activateSelected(true); // drill into dir / open file
        return;
      case "Escape":
        closeCtxMenu();
        return;
    }
    // Type-ahead: printable single characters jump to a matching row.
    if (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey && /\S/.test(e.key)) {
      typeAhead(e.key, performance.now());
    }
  });

  // Click anywhere closes context menu.
  document.addEventListener("click", () => closeCtxMenu(), true);

  // Initial canvas size.
  const ro = new ResizeObserver(() => sizeTreemap());
  ro.observe(elTreemapPane);
  sizeTreemap();
  renderEmptyState();

  // Stamp the version into the status bar from the build-time constant
  // injected by Vite (vite.config.ts reads package.json). No more drift.
  elStatusVersion.textContent = "v" + __APP_VERSION__;

  // Virtual scroller: re-render the visible window on scroll, coalesced to one
  // render per animation frame so fast scrolling stays smooth.
  let scrollPending = false;
  elDirList.addEventListener("scroll", () => {
    if (scrollPending || flatRows.length === 0) return;
    scrollPending = true;
    requestAnimationFrame(() => {
      scrollPending = false;
      renderDirWindow(true);
    });
  });
  // Re-window when the side panel resizes (different number of rows fit).
  const dirRo = new ResizeObserver(() => {
    if (flatRows.length > 0) renderDirWindow(true);
  });
  dirRo.observe(elDirList);
})();

// ---------- scan flow ----------

function startScan(rootPath: string) {
  state.scanning = true;
  state.scanRoot = null;
  state.currentRoot = null;
  state.selectedIdx = null;
  state.rectNames.clear();
  state.totals = null;
  state.scanRootPath = rootPath;
  elScanningOverlay.hidden = false;
  elTreemapEmpty.hidden = true;
  elDirList.innerHTML = "";
  elTopFilesList.innerHTML = "";
  elTopDirsList.innerHTML = "";
  elScanFiles.textContent = "0";
  elScanBytes.textContent = "0 B";
  elScanElapsed.textContent = "0.0 s";
  elBreadcrumb.innerHTML = `<span class="hint">Scanning ${escapeHtml(rootPath)}…</span>`;
  elStatusSummary.textContent = `Scanning ${rootPath}…`;
  saveConfig();
  ipc.scanStart(rootPath).catch((e) => {
    elScanningOverlay.hidden = true;
    state.scanning = false;
    elStatusSummary.textContent = `Scan failed: ${e?.message || e}`;
  });
}

function handleScanProgress(p: { files: number; bytes: number; elapsed_ms: number }) {
  elScanFiles.textContent = fmtCount(p.files);
  elScanBytes.textContent = fmtBytes(p.bytes);
  elScanElapsed.textContent = (p.elapsed_ms / 1000).toFixed(1) + " s";
}

async function handleScanComplete(p: {
  root_idx: number;
  files: number;
  dirs: number;
  bytes: number;
  duration_ms: number;
  root_path: string;
}) {
  state.scanning = false;
  state.scanRoot = p.root_idx;
  state.currentRoot = p.root_idx;
  state.scanRootPath = p.root_path;
  state.totals = { files: p.files, dirs: p.dirs, bytes: p.bytes, duration_ms: p.duration_ms };
  elScanningOverlay.hidden = true;
  elTreemapEmpty.hidden = true;
  (elRescanBtn as HTMLButtonElement).disabled = false;
  (elNewFolderBtn as HTMLButtonElement).disabled = false;
  (elNewFileBtn as HTMLButtonElement).disabled = false;
  expandedIdxs.clear();
  // Name the active tab after the scanned folder's last path segment.
  const t = tabs.find((x) => x.id === activeTabId);
  if (t) {
    const seg = p.root_path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || p.root_path;
    t.title = seg;
    renderTabBar();
  }
  const seq = ++drillSeq;
  pushLoading("Rendering…");
  try {
    await refreshAll(seq);
  } finally {
    popLoading();
  }
  elStatusSummary.textContent =
    `${fmtCount(p.files)} files · ${fmtCount(p.dirs)} folders · ${fmtBytes(p.bytes)} · scanned in ${fmtDuration(p.duration_ms)}`;
}

function handleScanCancelled() {
  state.scanning = false;
  elScanningOverlay.hidden = true;
  elStatusSummary.textContent = "Scan cancelled.";
  renderEmptyState();
}

// ---------- navigation ----------

function selectNode(idx: number) {
  state.selectedIdx = idx;
  treemap.setSelected(idx);
  // Highlight in dir list if visible.
  document.querySelectorAll(".list-row").forEach((r) => {
    r.classList.toggle("selected", Number((r as HTMLElement).dataset.idx) === idx);
  });
  // Keep the inspector in sync if it's the active side view.
  if (document.querySelector("#tab-inspect.active")) {
    refreshInspector();
  }
}

/** Switch the side panel to the Inspect view and load it for `idx`. */
function inspectNode(idx: number) {
  selectNode(idx);
  document.querySelectorAll(".side-tabs .tab").forEach((x) =>
    x.classList.toggle("active", (x as HTMLElement).dataset.tab === "inspect"),
  );
  document.querySelectorAll(".tab-pane").forEach((x) =>
    x.classList.toggle("active", x.id === "tab-inspect"),
  );
  refreshInspector();
}

// ---------- keyboard tree navigation ----------

/** Index of the currently-selected row within the flattened list, or -1. */
function selectedFlatIndex(): number {
  if (state.selectedIdx === null) return -1;
  return flatRows.findIndex((f) => f.row.idx === state.selectedIdx);
}

/** Select the row at flat index `i`, scroll it into view, repaint the window. */
function selectFlatIndex(i: number) {
  if (flatRows.length === 0) return;
  const clamped = Math.max(0, Math.min(flatRows.length - 1, i));
  const row = flatRows[clamped].row;
  state.selectedIdx = row.idx;
  treemap.setSelected(row.idx);
  scrollFlatIndexIntoView(clamped);
  renderDirWindow(true); // re-render applies .selected from state
}

function scrollFlatIndexIntoView(i: number) {
  const top = i * ROW_HEIGHT;
  const bottom = top + ROW_HEIGHT;
  const viewTop = elDirList.scrollTop;
  const viewBottom = viewTop + elDirList.clientHeight;
  if (top < viewTop) {
    elDirList.scrollTop = top;
  } else if (bottom > viewBottom) {
    elDirList.scrollTop = bottom - elDirList.clientHeight;
  }
}

/** Move the selection by `delta` rows (e.g. ±1 for arrows). */
function moveSelection(delta: number) {
  const cur = selectedFlatIndex();
  if (cur === -1) {
    selectFlatIndex(delta > 0 ? 0 : flatRows.length - 1);
  } else {
    selectFlatIndex(cur + delta);
  }
}

/** Expand a collapsed dir, or drill into it / open a file (the → / Enter key). */
async function activateSelected(drillOnDir: boolean) {
  const idx = state.selectedIdx;
  if (idx === null) return;
  const isDir = nameIsDir(idx) && !reparseIdxs.has(idx);
  if (isDir) {
    if (drillOnDir) {
      await drillInto(idx);
    } else if (!expandedIdxs.has(idx)) {
      await toggleExpand(idx);
    } else {
      // Already expanded → move into first child.
      moveSelection(1);
    }
  } else {
    ipc.openFile(idx).catch(() => {});
  }
}

/** Collapse an expanded dir, or jump to its parent row (the ← key). */
function collapseOrParent() {
  const idx = state.selectedIdx;
  if (idx === null) return;
  if (nameIsDir(idx) && expandedIdxs.has(idx)) {
    toggleExpand(idx);
    return;
  }
  // Jump to the parent row: the nearest preceding row at depth-1.
  const cur = selectedFlatIndex();
  if (cur <= 0) return;
  const myDepth = flatRows[cur].depth;
  for (let i = cur - 1; i >= 0; i--) {
    if (flatRows[i].depth < myDepth) {
      selectFlatIndex(i);
      return;
    }
  }
}

// Type-ahead: typing letters jumps to the next row whose name starts with the
// accumulated prefix (resets after a short idle).
let typeAheadBuffer = "";
let typeAheadAt = 0;
function typeAhead(ch: string, nowMs: number) {
  if (nowMs - typeAheadAt > 800) typeAheadBuffer = "";
  typeAheadAt = nowMs;
  typeAheadBuffer += ch.toLowerCase();
  const start = Math.max(0, selectedFlatIndex());
  const n = flatRows.length;
  for (let off = 0; off < n; off++) {
    const i = (start + off + (typeAheadBuffer.length === 1 ? 1 : 0)) % n;
    const name = flatRows[i].row.name.toLowerCase();
    if (name.startsWith(typeAheadBuffer)) {
      selectFlatIndex(i);
      return;
    }
  }
}

/** Bumped on every drill/refresh. Promises captured before a drill check the
 *  current seq before applying their results so a slow in-flight render from
 *  the previous drill doesn't overwrite the new view. Also kills the perceived
 *  "freeze" from racing renders piling up. */
let drillSeq = 0;
let inFlightDrills = 0;

function pushLoading(text: string) {
  inFlightDrills++;
  elStatusLoadingText.textContent = text;
  elStatusLoading.hidden = false;
}
function popLoading() {
  inFlightDrills = Math.max(0, inFlightDrills - 1);
  if (inFlightDrills === 0) {
    elStatusLoading.hidden = true;
  }
}

async function drillInto(idx: number) {
  if (!nameIsDir(idx)) return;
  const name = state.rectNames.get(idx) || "(node)";
  const seq = ++drillSeq;
  state.currentRoot = idx;
  state.selectedIdx = null;
  expandedIdxs.clear();
  pushLoading(`Loading ${name}…`);
  try {
    await refreshAll(seq);
    if (seq !== drillSeq) return;
    const summary = await ipc.nodeSummary(idx).catch(() => null);
    if (seq !== drillSeq) return;
    elStatusSummary.textContent = summary
      ? `${escapeText(summary.full_path)} — ${fmtBytes(state.sizeMode === "allocated" ? summary.allocated : summary.logical)} · ${fmtCount(summary.file_count)} files`
      : name;
  } catch (e) {
    if (seq === drillSeq) {
      elStatusSummary.textContent = `Drill failed: ${(e as Error)?.message ?? e}`;
    }
  } finally {
    popLoading();
  }
}

async function drillUp() {
  if (state.currentRoot === null || state.scanRoot === null) return;
  if (state.currentRoot === state.scanRoot) return;
  const crumbs = await ipc.breadcrumb(state.currentRoot).catch(() => []);
  if (crumbs.length < 2) return;
  const seq = ++drillSeq;
  state.currentRoot = crumbs[crumbs.length - 2].idx;
  state.selectedIdx = null;
  expandedIdxs.clear();
  pushLoading("Loading…");
  try {
    await refreshAll(seq);
  } catch (e) {
    if (seq === drillSeq) {
      elStatusSummary.textContent = `Drill failed: ${(e as Error)?.message ?? e}`;
    }
  } finally {
    popLoading();
  }
}

// We stash is_dir per visible idx because list rows and breadcrumb come from
// different IPC calls; nameIsDir checks the most recent dir list and treemap rects.
const dirIdxs = new Set<number>();
// Reparse points (junctions/symlinks) — leaves we never descend or treat as
// real folders for create-inside operations.
const reparseIdxs = new Set<number>();
function nameIsDir(idx: number): boolean {
  return dirIdxs.has(idx);
}

// Idxs the user has expanded inline (chevron-click). Reset on drill-in/-out
// because the new view is rooted somewhere else and these idxs would refer to
// nodes that aren't currently visible.
const expandedIdxs = new Set<number>();

async function toggleExpand(idx: number) {
  if (expandedIdxs.has(idx)) expandedIdxs.delete(idx);
  else expandedIdxs.add(idx);
  // Re-flatten + re-render, keeping the user's scroll position (expand-in-place).
  await refreshDirList(drillSeq, true);
}

// ---------- refresh ----------

/** Refresh everything for the current root. Callers pass the drillSeq they
 *  captured before mutating state; if a newer drill supersedes us, we discard
 *  our results instead of writing stale data to the DOM. */
async function refreshAll(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  // Issue all four IPC calls in parallel. The Rust side now uses RwLock so
  // these genuinely run concurrently on the worker pool instead of serializing.
  const breadcrumbP = refreshBreadcrumb(seq).catch((e) => console.error("breadcrumb", e));
  const treemapP = refreshTreemap(seq).catch((e) => console.error("treemap", e));
  const dirListP = refreshDirList(seq).catch((e) => console.error("dirList", e));
  const topNP = refreshTopN(seq).catch((e) => console.error("topN", e));
  await Promise.all([breadcrumbP, treemapP, dirListP, topNP]);
}

async function refreshBreadcrumb(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  const crumbs = await ipc.breadcrumb(state.currentRoot).catch(() => []);
  if (seq !== drillSeq) return;
  elBreadcrumb.innerHTML = "";
  crumbs.forEach((c, i) => {
    const b = document.createElement("button");
    b.className = "crumb";
    b.type = "button";
    b.textContent = c.name || "(root)";
    b.title = c.name;
    b.addEventListener("click", () => drillInto(c.idx));
    elBreadcrumb.appendChild(b);
    if (i < crumbs.length - 1) {
      const sep = document.createElement("span");
      sep.className = "sep";
      sep.textContent = "›";
      elBreadcrumb.appendChild(sep);
    }
  });
}

async function refreshTreemap(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  const rect = elTreemapPane.getBoundingClientRect();
  const minPx = 3;
  const maxDepth = 4;
  const rects = await ipc.treemapLayout(
    state.currentRoot,
    rect.width,
    rect.height,
    minPx,
    maxDepth,
    state.sizeMode,
  );
  if (seq !== drillSeq) return;
  // The Rust Rect now carries its `name`, so we don't need a separate name-
  // lookup IPC call — the treemap renderer reads name straight off each rect.
  state.rectNames.clear();
  dirIdxs.clear();
  for (const r of rects) {
    state.rectNames.set(r.idx, r.name);
    if (r.is_dir) dirIdxs.add(r.idx);
  }
  treemap.setData(rects, state.currentRoot, (idx) => state.rectNames.get(idx) || "");
}

function noteRow(row: DirRow) {
  if (row.is_dir) dirIdxs.add(row.idx);
  if (row.is_reparse) reparseIdxs.add(row.idx);
  state.rectNames.set(row.idx, row.name);
}

/** Flat tree-with-expansion: each row carries the depth it should indent at. */
interface FlatRow { row: DirRow; depth: number; }

const ROW_HEIGHT = 26;       // px; must match .list-row height in CSS
const WINDOW_BUFFER = 8;     // extra rows above/below the viewport

/** The full flattened row list for the current view (root's children + any
 *  inline-expanded subtrees). The virtual scroller renders only the slice that
 *  is on screen, so this can hold tens of thousands of rows cheaply. */
let flatRows: FlatRow[] = [];

async function refreshDirList(seq: number = drillSeq, preserveScroll = false) {
  if (state.currentRoot === null) return;
  const PER_LEVEL_LIMIT = 8192;
  const next: FlatRow[] = [];

  const visit = async (parent: number, depth: number): Promise<void> => {
    const rows = await ipc.listDir(parent, state.sort, 0, PER_LEVEL_LIMIT, state.sizeMode);
    if (seq !== drillSeq) return;
    for (const row of rows) {
      noteRow(row);
      next.push({ row, depth });
      if (expandedIdxs.has(row.idx) && row.is_dir && !row.is_reparse) {
        await visit(row.idx, depth + 1);
        if (seq !== drillSeq) return;
      }
    }
  };
  await visit(state.currentRoot, 0);
  if (seq !== drillSeq) return;

  flatRows = next;
  // Fresh drill resets scroll to top; chevron-expands keep the user in place.
  renderDirWindow(preserveScroll);
}

/** Render only the rows visible in the viewport (windowed virtualization).
 *
 *  Layout: a single content div whose total height is `rows * ROW_HEIGHT`
 *  (via a spacer at the bottom), with the visible slice rendered after a
 *  `padding-top` equal to `firstVisible * ROW_HEIGHT`. Using padding-top
 *  rather than absolute/transform positioning keeps the rows in normal flow,
 *  which is what made an earlier transform-based attempt render blank. */
function renderDirWindow(preserveScroll: boolean) {
  const total = flatRows.length;
  const scrollTop = preserveScroll ? elDirList.scrollTop : 0;
  if (!preserveScroll) elDirList.scrollTop = 0;

  const viewportH = elDirList.clientHeight || 600;
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - WINDOW_BUFFER);
  const visibleCount = Math.ceil(viewportH / ROW_HEIGHT) + WINDOW_BUFFER * 2;
  const last = Math.min(total, first + visibleCount);

  const frag = document.createDocumentFragment();
  // Top spacer pushes the visible slice down to its real scroll offset.
  const topPad = document.createElement("div");
  topPad.style.height = `${first * ROW_HEIGHT}px`;
  frag.appendChild(topPad);

  for (let i = first; i < last; i++) {
    frag.appendChild(renderRow(flatRows[i].row, flatRows[i].depth));
  }

  // Bottom spacer makes the scrollbar represent the full list height.
  const bottomPad = document.createElement("div");
  bottomPad.style.height = `${Math.max(0, (total - last) * ROW_HEIGHT)}px`;
  frag.appendChild(bottomPad);

  elDirList.innerHTML = "";
  elDirList.appendChild(frag);
}

async function refreshTopN(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  const t = await ipc.topN(state.currentRoot, 50, state.sizeMode);
  if (seq !== drillSeq) return;
  elTopFilesList.innerHTML = "";
  elTopDirsList.innerHTML = "";
  const ff = document.createDocumentFragment();
  for (const r of t.files) {
    noteRow(r);
    ff.appendChild(renderRow(r));
  }
  elTopFilesList.appendChild(ff);
  const fd = document.createDocumentFragment();
  for (const r of t.dirs) {
    noteRow(r);
    fd.appendChild(renderRow(r));
  }
  elTopDirsList.appendChild(fd);
}

function renderRow(row: DirRow, depth: number = 0): HTMLElement {
  const el = document.createElement("div");
  el.className =
    "list-row" +
    (row.is_dir ? " dir" : "") +
    (row.idx === state.selectedIdx ? " selected" : "");
  el.dataset.idx = String(row.idx);
  if (depth > 0) {
    el.style.paddingLeft = `${10 + 16 * depth}px`;
  }

  // Real folders get an expand/collapse chevron (separate click target from the
  // row body). Reparse points (junctions, app-exec links) are leaves to us —
  // they get a different glyph and no chevron handler. Files get a plain dot.
  const expandable = row.is_dir && !row.is_reparse;
  const isExpanded = expandedIdxs.has(row.idx);
  let iconHtml: string;
  if (expandable) {
    const chev = isExpanded ? "▼" : "▶";
    iconHtml = `<span class="chev" title="${isExpanded ? "Collapse" : "Expand"}">${chev}</span>`;
  } else if (row.is_reparse) {
    iconHtml = `<span class="icon">↪</span>`;
  } else {
    iconHtml = `<span class="icon">·</span>`;
  }

  el.innerHTML = `
    <span class="name">${iconHtml}<span class="filename" title="${escapeHtml(row.name)}">${escapeHtml(row.name)}</span>${row.is_reparse ? '<span class="badge">link</span>' : ""}</span>
    <span class="size">${fmtBytes(row.size)}</span>
    <span class="pct"><span class="pct-bar"><span class="pct-fill" style="width:${(row.pct_parent * 100).toFixed(2)}%"></span></span></span>
    <span class="mtime">${fmtMtime(row.mtime)}</span>
  `;

  // Chevron: toggle inline expansion without drilling.
  const chev = el.querySelector(".chev") as HTMLElement | null;
  if (chev) {
    chev.addEventListener("click", (e) => {
      e.stopPropagation();
      toggleExpand(row.idx);
    });
  }

  // Row body: single-click drills into the folder (replaces the view). Files
  // just select. This matches Skylar's stated expectation that one click on
  // the row navigates rather than just highlights.
  el.addEventListener("click", (e) => {
    e.stopPropagation();
    if (row.is_dir && !row.is_reparse) {
      drillInto(row.idx);
    } else {
      selectNode(row.idx);
    }
  });
  // Double-click: same as single for dirs (kept for muscle memory); for files,
  // hand off to Explorer.
  el.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    if (row.is_dir && !row.is_reparse) drillInto(row.idx);
    else ipc.openInExplorer(row.idx).catch(() => {});
  });
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openCtxMenu(row.idx, e.clientX, e.clientY);
  });
  return el;
}

// ---------- size mode + theme ----------

function setSizeMode(mode: SizeMode) {
  if (state.sizeMode === mode) return;
  state.sizeMode = mode;
  elModeAlloc.classList.toggle("active", mode === "allocated");
  elModeLogical.classList.toggle("active", mode === "logical");
  saveConfig();
  if (state.currentRoot !== null) refreshAll();
}

function themeForCanvas(): TreemapTheme {
  const dark = state.theme === "dark";
  const styles = getComputedStyle(document.documentElement);
  return {
    dark,
    borderColor: styles.getPropertyValue("--treemap-border").trim() || (dark ? "rgba(0,0,0,0.45)" : "rgba(255,255,255,0.55)"),
    textColor: styles.getPropertyValue("--treemap-text").trim() || (dark ? "#f9fafb" : "#ffffff"),
    bgColor: styles.getPropertyValue("--treemap-bg").trim() || (dark ? "#0b0d12" : "#f9fafb"),
  };
}

function applyTheme() {
  document.documentElement.dataset.theme = state.theme;
  treemap.setTheme(themeForCanvas());
}

function sizeTreemap() {
  const { width, height } = elTreemapPane.getBoundingClientRect();
  treemap.resize(width, height);
  if (state.currentRoot !== null && !state.scanning) {
    refreshTreemap().catch(() => {});
  }
}

function renderEmptyState() {
  if (state.scanRoot === null && !state.scanning) {
    elTreemapEmpty.hidden = false;
  } else {
    elTreemapEmpty.hidden = true;
  }
}

// ---------- tooltip ----------

function updateTooltip(rect: Rect | null, x: number, y: number) {
  if (!rect) {
    elTooltip.hidden = true;
    return;
  }
  const name = state.rectNames.get(rect.idx) || "";
  elTooltip.innerHTML = `
    <div class="t-name">${escapeHtml(name || "(unknown)")}</div>
    <div class="t-row">${fmtBytes(rect.size)}</div>
    <div class="t-row">${rect.is_dir ? "folder" : "file"} · depth ${rect.depth}</div>
  `;
  elTooltip.hidden = false;
  const pad = 10;
  const tx = Math.min(window.innerWidth - elTooltip.offsetWidth - pad, x + pad);
  const ty = Math.min(window.innerHeight - elTooltip.offsetHeight - pad, y + pad);
  elTooltip.style.left = tx + "px";
  elTooltip.style.top = ty + "px";
}

// ---------- context menu ----------

interface CtxItem { label: string; danger?: boolean; shortcut?: string; action: () => void; }

function openCtxMenu(idx: number, x: number, y: number) {
  closeCtxMenu();
  const isDir = nameIsDir(idx);
  const isReparse = reparseIdxs.has(idx);
  const items: CtxItem[] = [
    isDir
      ? { label: "Drill into folder", shortcut: "Enter", action: () => drillInto(idx) }
      : { label: "Open (edit)", shortcut: "Enter", action: () => ipc.openFile(idx).catch((e) => alert(`Open failed: ${(e as Error)?.message ?? e}`)) },
    ...(!isDir ? [{ label: "Inspect (checksums, hidden data)", action: () => inspectNode(idx) }] : []),
    { label: "Reveal in Explorer", action: () => ipc.openInExplorer(idx).catch(() => {}) },
    ...(isDir && !isReparse ? [{ label: "Open in Terminal", action: () => ipc.openInTerminal(idx).catch(() => {}) }] : []),
    { label: "Copy full path", action: async () => {
      try {
        const p = await ipc.copyPath(idx);
        await navigator.clipboard.writeText(p);
      } catch {}
    } },
    { label: "—", action: () => {} },
    ...(isDir && !isReparse ? [
      { label: "New folder…", action: () => promptCreate(idx, "folder") },
      { label: "New file…", action: () => promptCreate(idx, "file") },
    ] : []),
    { label: "Rename…", shortcut: "F2", action: () => promptRename(idx) },
    ...(isDir && !isReparse ? [
      { label: "—", action: () => {} },
      { label: "Find files older than 1 year (≥10 MB)", action: () => runSuperSkillOldFiles(idx) },
      { label: "Find empty folders", action: () => runSuperSkillEmpty(idx) },
    ] : []),
    { label: "—", action: () => {} },
    { label: "Move to Recycle Bin…", danger: true, shortcut: "Del", action: () => confirmRecycle(idx) },
  ];
  const menu = elCtxMenu;
  menu.innerHTML = "";
  for (const item of items) {
    if (item.label === "—") {
      const sep = document.createElement("div");
      sep.className = "ctx-sep";
      menu.appendChild(sep);
      continue;
    }
    const it = document.createElement("div");
    it.className = "ctx-item" + (item.danger ? " danger" : "");
    it.innerHTML = `<span>${escapeHtml(item.label)}</span>${item.shortcut ? `<span class="ctx-shortcut">${item.shortcut}</span>` : ""}`;
    it.addEventListener("click", (e) => {
      e.stopPropagation();
      closeCtxMenu();
      try { item.action(); } catch {}
    });
    menu.appendChild(it);
  }
  menu.style.left = Math.min(window.innerWidth - 220, x) + "px";
  menu.style.top = Math.min(window.innerHeight - menu.offsetHeight - 10, y) + "px";
  menu.hidden = false;
}

function closeCtxMenu() {
  elCtxMenu.hidden = true;
}

async function promptCreate(parentIdx: number, kind: "folder" | "file") {
  const label = kind === "folder" ? "New folder name:" : "New file name:";
  const placeholder = kind === "folder" ? "New folder" : "untitled.txt";
  const name = prompt(label, placeholder);
  if (name === null) return; // cancelled
  const trimmed = name.trim();
  if (!trimmed) return;
  try {
    const res =
      kind === "folder"
        ? await ipc.createFolder(parentIdx, trimmed)
        : await ipc.createFile(parentIdx, trimmed);
    elStatusSummary.textContent = `Created ${res.path}`;
    if (res.rescan_path) startScan(res.rescan_path);
  } catch (e) {
    alert(`Create failed: ${(e as Error)?.message ?? e}`);
  }
}

async function promptRename(idx: number) {
  const current = state.rectNames.get(idx) || "";
  const name = prompt("Rename to:", current);
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed || trimmed === current) return;
  try {
    const res = await ipc.renameNode(idx, trimmed);
    elStatusSummary.textContent = `Renamed to ${res.path}`;
    if (res.rescan_path) startScan(res.rescan_path);
  } catch (e) {
    alert(`Rename failed: ${(e as Error)?.message ?? e}`);
  }
}

async function confirmRecycle(idx: number) {
  const path = await ipc.copyPath(idx).catch(() => "");
  const summary = await ipc.nodeSummary(idx).catch(() => null);
  const size = summary ? fmtBytes(state.sizeMode === "allocated" ? summary.allocated : summary.logical) : "";
  const msg = `Move to Recycle Bin?\n\n${path}\n${size ? `(${size})` : ""}\n\nYou can restore from Explorer's Recycle Bin.`;
  if (!confirm(msg)) return;
  try {
    await ipc.recycleNode(idx);
    elStatusSummary.textContent = `Recycled ${path}`;
    // Rescan the current root to pick up the change. Simpler than diffing the tree.
    if (state.scanRootPath) startScan(state.scanRootPath);
  } catch (e) {
    alert(`Recycle failed: ${(e as Error).message ?? e}`);
  }
}

async function runSuperSkillOldFiles(idx: number) {
  const cutoff = Math.floor(Date.now() / 1000) - 365 * 86400;
  try {
    const found = await ipc.findOldFiles(idx, cutoff, 10 * 1024 * 1024, 200);
    showResultsModal(
      "Files older than 1 year, ≥ 10 MB",
      found.map((f) => ({ label: f.path, sub: `${fmtBytes(f.size)} · ${fmtMtime(f.mtime)}` })),
    );
  } catch (e) {
    alert(`Search failed: ${(e as Error).message ?? e}`);
  }
}

async function runSuperSkillEmpty(idx: number) {
  try {
    const dirs = await ipc.findEmptyDirs(idx, 500);
    showResultsModal(
      "Empty folders",
      dirs.map((d) => ({ label: d, sub: "" })),
    );
  } catch (e) {
    alert(`Search failed: ${(e as Error).message ?? e}`);
  }
}

function showResultsModal(title: string, rows: { label: string; sub: string }[]) {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true">
      <div class="modal-head">
        <div class="modal-title">${escapeHtml(title)} — ${rows.length} results</div>
        <button class="btn ghost small" id="close-modal">✕</button>
      </div>
      <div class="modal-body" style="max-height:50vh; min-width: 600px"></div>
      <div class="modal-foot">
        <button class="btn" id="close-modal-2">Close</button>
      </div>
    </div>`;
  const body = backdrop.querySelector(".modal-body")!;
  for (const r of rows) {
    const row = document.createElement("div");
    row.className = "drive-row";
    row.style.gridTemplateColumns = "1fr auto";
    row.innerHTML = `
      <div>
        <div class="drive-label" style="font-family: var(--font-mono); font-size:11.5px; word-break: break-all">${escapeHtml(r.label)}</div>
        <div class="drive-sub">${escapeHtml(r.sub)}</div>
      </div>`;
    body.appendChild(row);
  }
  document.body.appendChild(backdrop);
  const close = () => backdrop.remove();
  backdrop.querySelector("#close-modal")?.addEventListener("click", close);
  backdrop.querySelector("#close-modal-2")?.addEventListener("click", close);
  backdrop.addEventListener("click", (e) => { if (e.target === backdrop) close(); });
}

// ---------- file inspector (checksums + steganography) ----------

let compareMarkIdx: number | null = null;
let compareMarkName = "";

const STEGO_METHODS: { id: StegoMethod; label: string }[] = [
  { id: "lsb", label: "LSB (image)" },
  { id: "whitespace", label: "Whitespace / SNOW (text)" },
  { id: "format_append", label: "Appended-after-EOF" },
];

/** Rebuild the Inspect panel for the currently-selected node. */
async function refreshInspector() {
  const idx = state.selectedIdx;
  if (idx === null || (nameIsDir(idx) && !reparseIdxs.has(idx))) {
    elInspector.innerHTML = `<div class="inspector-empty muted small">Select a file, then open Inspect to compute checksums and scan for hidden data.</div>`;
    return;
  }
  const name = state.rectNames.get(idx) ?? "(file)";
  let path = "";
  try {
    path = await ipc.copyPath(idx);
  } catch {}

  elInspector.innerHTML = "";
  const sec = (title: string) => {
    const s = document.createElement("div");
    s.className = "insp-section";
    s.innerHTML = `<h3>${escapeHtml(title)}</h3>`;
    elInspector.appendChild(s);
    return s;
  };

  // Header: name + path.
  const head = sec("File");
  const p = document.createElement("div");
  p.className = "insp-path";
  p.textContent = path || name;
  head.appendChild(p);

  // --- Checksums ---
  const cs = sec("Checksums");
  const csBtn = btn("Compute CRC32 / MD5 / SHA-1 / SHA-256");
  cs.appendChild(csBtn);
  csBtn.addEventListener("click", async () => {
    csBtn.textContent = "Computing…";
    (csBtn as HTMLButtonElement).disabled = true;
    try {
      const c = await ipc.checksumNode(idx);
      const kv = document.createElement("div");
      kv.className = "insp-kv";
      kv.innerHTML = `
        <span class="k">Size</span><span class="v">${fmtBytes(c.size)} (${c.size.toLocaleString()} bytes)</span>
        <span class="k">CRC32</span><span class="v">${c.crc32}</span>
        <span class="k">MD5</span><span class="v">${c.md5}</span>
        <span class="k">SHA-1</span><span class="v">${c.sha1}</span>
        <span class="k">SHA-256</span><span class="v">${c.sha256}</span>`;
      csBtn.replaceWith(kv);
    } catch (e) {
      csBtn.textContent = "Compute checksums";
      (csBtn as HTMLButtonElement).disabled = false;
      alert(`Checksum failed: ${(e as Error)?.message ?? e}`);
    }
  });

  // --- Compare ---
  const cmp = sec("Compare");
  const cmpActions = document.createElement("div");
  cmpActions.className = "insp-actions";
  const markBtn = btn(compareMarkIdx === idx ? "✓ Marked" : "Mark for compare");
  cmpActions.appendChild(markBtn);
  markBtn.addEventListener("click", () => {
    compareMarkIdx = idx;
    compareMarkName = name;
    refreshInspector();
  });
  if (compareMarkIdx !== null && compareMarkIdx !== idx) {
    const doCmp = btn(`Compare with "${compareMarkName}"`);
    cmpActions.appendChild(doCmp);
    doCmp.addEventListener("click", async () => {
      try {
        const r = await ipc.compareNodes(compareMarkIdx!, idx);
        const note = r.identical
          ? "✓ Identical (same SHA-256)"
          : r.first_diff_offset !== null
            ? `✗ Differ — first difference at byte ${r.first_diff_offset.toLocaleString()}`
            : `✗ Differ — sizes ${fmtBytes(r.size_a)} vs ${fmtBytes(r.size_b)}`;
        showResultsModal("Compare result", [
          { label: note, sub: "" },
          { label: `A: ${compareMarkName}`, sub: `${fmtBytes(r.size_a)} · ${r.sha256_a}` },
          { label: `B: ${name}`, sub: `${fmtBytes(r.size_b)} · ${r.sha256_b}` },
        ]);
      } catch (e) {
        alert(`Compare failed: ${(e as Error)?.message ?? e}`);
      }
    });
  }
  cmp.appendChild(cmpActions);

  // --- Steganography scan ---
  const st = sec("Hidden data (steganography)");
  const scanBtn = btn("Scan for hidden data");
  st.appendChild(scanBtn);
  scanBtn.addEventListener("click", async () => {
    scanBtn.textContent = "Scanning…";
    (scanBtn as HTMLButtonElement).disabled = true;
    try {
      const report = await ipc.stegoScan(idx);
      const container = document.createElement("div");
      for (const f of report.findings) {
        const card = document.createElement("div");
        const cls = f.suspicious ? "hit" : f.statistical_anomaly ? "advisory" : "";
        card.className = "insp-finding" + (cls ? ` ${cls}` : "");
        const badge = f.suspicious
          ? `<span class="insp-badge hit">found</span>`
          : f.statistical_anomaly
            ? `<span class="insp-badge advisory">advisory</span>`
            : `<span class="insp-badge clean">clean</span>`;
        const label = STEGO_METHODS.find((m) => m.id === f.method)?.label ?? f.method;
        card.innerHTML = `<div class="insp-method">${escapeHtml(label)} ${badge}</div><div class="insp-detail">${escapeHtml(f.detail)}</div>`;
        if (f.suspicious) {
          const ex = btn("Extract hidden data");
          ex.style.marginTop = "6px";
          ex.addEventListener("click", () => extractAndShow(idx, f.method));
          card.appendChild(ex);
        }
        container.appendChild(card);
      }
      scanBtn.replaceWith(container);
    } catch (e) {
      scanBtn.textContent = "Scan for hidden data";
      (scanBtn as HTMLButtonElement).disabled = false;
      alert(`Scan failed: ${(e as Error)?.message ?? e}`);
    }
  });

  // --- Embed (round-trip / watermark your own file) ---
  const em = sec("Embed (watermark / test — writes a new .stego copy)");
  const emActions = document.createElement("div");
  emActions.className = "insp-actions";
  for (const m of STEGO_METHODS) {
    const b = btn(m.label);
    b.addEventListener("click", () => embedFlow(idx, m.id));
    emActions.appendChild(b);
  }
  em.appendChild(emActions);
}

function btn(label: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.className = "btn small";
  b.textContent = label;
  return b;
}

async function extractAndShow(idx: number, method: StegoMethod) {
  try {
    const res = await ipc.stegoExtract(idx, method);
    const preview =
      res.text !== null
        ? res.text
        : `(${res.len} binary bytes — not valid UTF-8)`;
    const save = confirm(
      `Recovered ${res.len} bytes:\n\n${preview.slice(0, 500)}\n\nClick OK to save the recovered payload to a file.`,
    );
    if (save) {
      const { save: saveDialog } = await import("@tauri-apps/plugin-dialog");
      const out = await saveDialog({ defaultPath: "recovered.bin" });
      if (out && typeof out === "string") {
        await ipc.saveBytes(out, res.bytes);
        elStatusSummary.textContent = `Saved recovered payload → ${out}`;
      }
    }
  } catch (e) {
    alert(`Extract failed: ${(e as Error)?.message ?? e}`);
  }
}

async function embedFlow(idx: number, method: StegoMethod) {
  const payload = prompt(
    "Text to hide in this file (a new .stego copy is written; the original is untouched):",
    "",
  );
  if (payload === null || payload === "") return;
  try {
    const res = await ipc.stegoEmbed(idx, method, payload);
    elStatusSummary.textContent = `Embedded → ${res.path}`;
    if (res.rescan_path) startScan(res.rescan_path);
  } catch (e) {
    alert(`Embed failed: ${(e as Error)?.message ?? e}`);
  }
}

// ---------- config ----------

const CONFIG_KEY = "treelens.ui.v1";
function loadConfig() {
  try {
    const raw = localStorage.getItem(CONFIG_KEY);
    if (!raw) return;
    const v = JSON.parse(raw) as Partial<UiState>;
    if (v.theme === "light" || v.theme === "dark") state.theme = v.theme;
    if (typeof v.themeFollowsSystem === "boolean") state.themeFollowsSystem = v.themeFollowsSystem;
    if (v.sizeMode === "logical" || v.sizeMode === "allocated") state.sizeMode = v.sizeMode;
    if (v.colorMode === "type" || v.colorMode === "heat") state.colorMode = v.colorMode;
    if (v.sort) state.sort = v.sort;
  } catch {}
  // Apply visible state to controls.
  elModeAlloc.classList.toggle("active", state.sizeMode === "allocated");
  elModeLogical.classList.toggle("active", state.sizeMode === "logical");
  elHeatBtn.setAttribute("aria-pressed", state.colorMode === "heat" ? "true" : "false");
  treemap.setMode(state.colorMode);
}
function saveConfig() {
  const v: Partial<UiState> = {
    theme: state.theme,
    themeFollowsSystem: state.themeFollowsSystem,
    sizeMode: state.sizeMode,
    colorMode: state.colorMode,
    sort: state.sort,
  };
  try { localStorage.setItem(CONFIG_KEY, JSON.stringify(v)); } catch {}
}

// ---------- helpers ----------

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
function escapeText(s: string): string {
  return s.replace(/[\r\n\t]+/g, " ");
}
