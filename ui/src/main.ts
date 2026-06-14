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
  type DriveEntry,
  type ExtStat,
  type Rect,
  type SearchHit,
  type SearchKind,
  type SizeMode,
  type SortKey,
  type StegoMethod,
} from "./ipc";
import { Treemap, type TreemapTheme } from "./treemap";
import { fmtBytes, fmtCount, fmtDuration, fmtMtime } from "./format";
import { type ColorMode, hsl } from "./colors";
import { pickScanRoot } from "./drives";

declare const __APP_VERSION__: string;

interface UiState {
  scanRoot: number | null;       // arena idx of the scanned root
  currentRoot: number | null;    // currently-displayed (drilled-in) root
  selectedIdx: number | null;    // the "active" (last-clicked) selection
  selectedIdxs: Set<number>;     // full multi-selection (ctrl/shift-click)
  selectAnchor: number | null;   // anchor for shift-range selection
  rectNames: Map<number, string>;
  scanRootPath: string;
  totals: { files: number; dirs: number; bytes: number; duration_ms: number } | null;
  scanning: boolean;
  sizeMode: SizeMode;
  colorMode: ColorMode;
  sort: SortKey;
  theme: "light" | "dark";
  themeFollowsSystem: boolean;
  /** Glob patterns excluded from scans (persisted; edited in Settings). */
  excludes: string[];
  /** Max treemap nesting depth to render (2–6). */
  treemapDepth: number;
  /** Recently scanned roots, most-recent-first (persisted). */
  recents: string[];
  /** Minimum file size (bytes) the duplicate finder considers. */
  dupeMinSize: number;
}

const state: UiState = {
  scanRoot: null,
  currentRoot: null,
  selectedIdx: null,
  selectedIdxs: new Set(),
  selectAnchor: null,
  rectNames: new Map(),
  scanRootPath: "",
  totals: null,
  scanning: false,
  sizeMode: "allocated",
  colorMode: "type",
  sort: "size_desc",
  theme: prefersDark() ? "dark" : "light",
  themeFollowsSystem: true,
  excludes: [],
  treemapDepth: 4,
  recents: [],
  dupeMinSize: 4096,
};

function prefersDark(): boolean {
  return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
}

const RECENTS_MAX = 10;

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
  (elJunkBtn as HTMLButtonElement).disabled = state.currentRoot === null;
  (elExportBtn as HTMLButtonElement).disabled = state.currentRoot === null;
  (elDupesBtn as HTMLButtonElement).disabled = state.currentRoot === null;
  (elSaveScanBtn as HTMLButtonElement).disabled = state.scanRoot === null;
  // The errors pill reflects the last scan globally; clear it on tab switch.
  elScanErrorsPill.hidden = true;
  updateTreemapChrome();
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
const elDriveCards = $("#drive-cards");
const elRescanBtn = $("#rescan-btn");
const elNewFolderBtn = $("#new-folder-btn");
const elNewFileBtn = $("#new-file-btn");
const elJunkBtn = $("#junk-btn");
const elExportBtn = $("#export-btn");
const elDupesBtn = $("#dupes-btn");
const elSaveScanBtn = $("#save-scan-btn");
const elOpenScanBtn = $("#open-scan-btn");
const elRecentsBtn = $("#recents-btn");
const elRecentsMenu = $("#recents-menu");
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
const elSearchInput = $<HTMLInputElement>("#search-input");
const elSearchList = $("#search-list");
const elSearchMinSize = $<HTMLSelectElement>("#search-minsize");
const elTypesList = $("#types-list");
const elContentsFilter = $<HTMLInputElement>("#contents-filter");
const elScanErrorsPill = $("#scan-errors-pill");
const elDepthCtl = $("#treemap-depth-ctl");
const elDepthVal = $("#depth-val");
const elTreemapLegend = $("#treemap-legend");

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
  await loadConfig();
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

  // Scan triggers. The toolbar button opens the drive+folder picker modal; the
  // empty-state button goes straight to a folder dialog (drives are already
  // shown as cards right above it).
  const triggerScan = async () => {
    const root = await pickScanRoot();
    if (root) startScan(root);
  };
  elScanBtn.addEventListener("click", triggerScan);
  elEmptyScanBtn?.addEventListener("click", async () => {
    const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
    const picked = await openDialog({ directory: true, multiple: false });
    if (picked && typeof picked === "string") startScan(picked);
  });
  renderDriveCards();
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
  elJunkBtn.addEventListener("click", () => {
    if (state.currentRoot !== null) runJunkFinder(state.currentRoot);
  });
  elExportBtn.addEventListener("click", () => exportCurrentTree());
  elDupesBtn.addEventListener("click", () => {
    if (state.currentRoot !== null) runDuplicateFinder(state.currentRoot);
  });
  elSaveScanBtn.addEventListener("click", () => saveCurrentScan());
  elOpenScanBtn.addEventListener("click", () => openSavedScan());
  elRecentsBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleRecentsMenu();
  });
  document.addEventListener("click", (e) => {
    if (!elRecentsMenu.hidden && !(e.target as HTMLElement).closest(".scan-group")) {
      elRecentsMenu.hidden = true;
    }
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
    renderLegend();
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
      if (tab === "search") elSearchInput.focus();
      if (tab === "types") refreshExtensions();
    });
  });

  setupSearch();
  setupColumnSort();
  setupRowDelegation();
  $("#help-btn").addEventListener("click", () => toggleHelp());
  $("#settings-btn").addEventListener("click", () => openSettings());
  elScanErrorsPill.addEventListener("click", () => showScanErrors());

  // Treemap depth control.
  $("#depth-dec").addEventListener("click", () => setTreemapDepth(state.treemapDepth - 1));
  $("#depth-inc").addEventListener("click", () => setTreemapDepth(state.treemapDepth + 1));
  elDepthVal.textContent = String(state.treemapDepth);

  // Live Contents filter — client-side, instant, no IPC.
  elContentsFilter.addEventListener("input", () => {
    contentsFilter = elContentsFilter.value;
    applyContentsFilter(false);
  });
  elContentsFilter.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      elContentsFilter.value = "";
      contentsFilter = "";
      applyContentsFilter(false);
    }
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
    // Esc closes the help overlay from anywhere (even with a field focused).
    if (e.key === "Escape" && document.getElementById("help-overlay")) {
      closeHelp();
      return;
    }
    // Ctrl/Cmd+F jumps to the Search tab from anywhere.
    if ((e.ctrlKey || e.metaKey) && (e.key === "f" || e.key === "F")) {
      e.preventDefault();
      openSearchTab();
      return;
    }
    if (e.target instanceof HTMLInputElement) return;
    if (e.key === "F1" || e.key === "?") {
      e.preventDefault();
      toggleHelp();
      return;
    }
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
        if (state.selectedIdx !== null) {
          // Shift+Delete = permanent delete (Windows convention); Delete = recycle.
          if (e.shiftKey) confirmDeletePermanent(state.selectedIdx);
          else confirmRecycle(state.selectedIdx);
        }
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

  // Initial canvas size. Coalesce resize bursts (window drag) to one
  // sizeTreemap per animation frame so we don't re-render the treemap base
  // layer on every intermediate size.
  let resizePending = false;
  const ro = new ResizeObserver(() => {
    if (resizePending) return;
    resizePending = true;
    requestAnimationFrame(() => {
      resizePending = false;
      sizeTreemap();
    });
  });
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

// Tabs with an in-flight scan, plus a per-tab watchdog timer. A scan that dies
// in the backend without emitting complete/cancelled (panic, killed worker)
// would otherwise leave the overlay spinning forever; the watchdog recovers the
// UI if no scan event arrives for WATCHDOG_MS.
const scanningTabs = new Set<number>();
const scanWatchdogs = new Map<number, ReturnType<typeof setTimeout>>();
const WATCHDOG_MS = 45_000;

function armScanWatchdog(tab: number) {
  const prev = scanWatchdogs.get(tab);
  if (prev) clearTimeout(prev);
  scanWatchdogs.set(tab, setTimeout(() => handleScanStalled(tab), WATCHDOG_MS));
}

function clearScanWatchdog(tab: number) {
  const prev = scanWatchdogs.get(tab);
  if (prev) clearTimeout(prev);
  scanWatchdogs.delete(tab);
  scanningTabs.delete(tab);
}

function handleScanStalled(tab: number) {
  clearScanWatchdog(tab);
  if (tab !== activeTabId) return; // background tab; nothing on screen to recover
  state.scanning = false;
  elScanningOverlay.hidden = true;
  (elRescanBtn as HTMLButtonElement).disabled = state.scanRootPath === "";
  elStatusSummary.textContent =
    "Scan stalled — no activity for 45s. The folder may be locked or inaccessible. Try again or pick another folder.";
  renderEmptyState();
}

function startScan(rootPath: string) {
  const tab = activeTabId;
  pushRecent(rootPath);
  elScanErrorsPill.hidden = true;
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
  scanningTabs.add(tab);
  armScanWatchdog(tab);
  ipc.scanStart(rootPath, tab, state.excludes).catch((e) => {
    clearScanWatchdog(tab);
    if (tab === activeTabId) {
      elScanningOverlay.hidden = true;
      state.scanning = false;
      elStatusSummary.textContent = `Scan failed: ${e?.message || e}`;
    }
  });
}

function handleScanProgress(p: { tab: number; files: number; bytes: number; elapsed_ms: number }) {
  // Keep the watchdog alive for whichever tab is making progress, even in the
  // background, so a long scan in a non-active tab isn't falsely declared stalled.
  if (scanningTabs.has(p.tab)) armScanWatchdog(p.tab);
  if (p.tab !== activeTabId) return; // progress for a background tab — don't touch the live overlay
  elScanFiles.textContent = fmtCount(p.files);
  elScanBytes.textContent = fmtBytes(p.bytes);
  elScanElapsed.textContent = (p.elapsed_ms / 1000).toFixed(1) + " s";
}

function tabTitleFromPath(path: string): string {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
}

async function handleScanComplete(p: {
  tab: number;
  root_idx: number;
  files: number;
  dirs: number;
  bytes: number;
  errors?: number;
  duration_ms: number;
  root_path: string;
}) {
  clearScanWatchdog(p.tab);

  // A scan that finished for a *background* tab must update that tab's snapshot
  // only — touching the live `state`/DOM here would corrupt whatever tab the
  // user is now looking at (the original cross-tab completion bug).
  if (p.tab !== activeTabId) {
    const bt = tabs.find((x) => x.id === p.tab);
    if (bt) {
      bt.scanRoot = p.root_idx;
      bt.currentRoot = p.root_idx;
      bt.scanRootPath = p.root_path;
      bt.totals = { files: p.files, dirs: p.dirs, bytes: p.bytes, duration_ms: p.duration_ms };
      bt.selectedIdx = null;
      bt.title = tabTitleFromPath(p.root_path);
      renderTabBar();
    }
    return;
  }

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
  (elJunkBtn as HTMLButtonElement).disabled = false;
  (elExportBtn as HTMLButtonElement).disabled = false;
  (elDupesBtn as HTMLButtonElement).disabled = false;
  (elSaveScanBtn as HTMLButtonElement).disabled = false;
  expandedIdxs.clear();
  // Name the active tab after the scanned folder's last path segment.
  const t = tabs.find((x) => x.id === activeTabId);
  if (t) {
    t.title = tabTitleFromPath(p.root_path);
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
  updateTreemapChrome();
  updateScanErrorsPill(p.errors ?? 0);
}

/** Show/refresh the status-bar pill counting inaccessible items, if any. */
function updateScanErrorsPill(errors: number) {
  if (errors > 0) {
    elScanErrorsPill.textContent = `⚠ ${fmtCount(errors)} inaccessible`;
    elScanErrorsPill.hidden = false;
  } else {
    elScanErrorsPill.hidden = true;
  }
}

async function showScanErrors() {
  let report;
  try {
    report = await ipc.scanErrors();
  } catch (e) {
    toastErr("Couldn't load scan errors", e);
    return;
  }
  if (report.count === 0) {
    toast("No inaccessible items in the last scan.", "info");
    return;
  }
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  const note = report.truncated
    ? ` (showing first ${report.sample.length.toLocaleString()})`
    : "";
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true" aria-label="Inaccessible items">
      <div class="modal-head">
        <div class="modal-title">${report.count.toLocaleString()} inaccessible items${note}</div>
        <button class="btn ghost small" id="se-close" aria-label="Close">✕</button>
      </div>
      <div class="modal-body" style="max-height:55vh; min-width:620px"></div>
      <div class="modal-foot">
        <span class="muted small" style="margin-right:auto">Usually permission-protected or locked. Relaunch as admin to see more.</span>
        <button class="btn" id="se-ok">Close</button>
      </div>
    </div>`;
  const body = backdrop.querySelector(".modal-body")!;
  if (report.sample.length === 0) {
    body.innerHTML = '<div class="search-hint muted small">No path samples were captured.</div>';
  } else {
    for (const p of report.sample) {
      const row = document.createElement("div");
      row.className = "se-row";
      row.textContent = p;
      body.appendChild(row);
    }
  }
  document.body.appendChild(backdrop);
  const close = () => backdrop.remove();
  backdrop.querySelector("#se-close")?.addEventListener("click", close);
  backdrop.querySelector("#se-ok")?.addEventListener("click", close);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) close();
  });
}

function handleScanCancelled(p: { tab: number }) {
  clearScanWatchdog(p.tab);
  if (p.tab !== activeTabId) return;
  state.scanning = false;
  elScanningOverlay.hidden = true;
  elStatusSummary.textContent = "Scan cancelled.";
  renderEmptyState();
}

// ---------- navigation ----------

/** Single-select `idx` (clears any multi-selection). Used by the treemap,
 *  keyboard nav, and plain clicks. */
function selectNode(idx: number) {
  state.selectedIdx = idx;
  state.selectAnchor = idx;
  state.selectedIdxs = new Set([idx]);
  applySelectionClasses();
  treemap.setSelected(idx);
  if (document.querySelector("#tab-inspect.active")) refreshInspector();
}

/** Paint the `.selected` class on whichever rows are in the selection set. */
function applySelectionClasses() {
  document.querySelectorAll(".list-row").forEach((r) => {
    const i = Number((r as HTMLElement).dataset.idx);
    r.classList.toggle("selected", state.selectedIdxs.has(i));
  });
}

/** Modifier-aware click selection on a list row.
 *  - plain  : single-select (caller handles drill for directories)
 *  - ctrl   : toggle this row in/out of the selection
 *  - shift  : range-select from the anchor to this row (in flat order) */
function clickSelect(idx: number, e: MouseEvent) {
  if (e.shiftKey && state.selectAnchor !== null) {
    const a = flatRows.findIndex((f) => f.row.idx === state.selectAnchor);
    const b = flatRows.findIndex((f) => f.row.idx === idx);
    if (a !== -1 && b !== -1) {
      const [lo, hi] = a < b ? [a, b] : [b, a];
      state.selectedIdxs = new Set(flatRows.slice(lo, hi + 1).map((f) => f.row.idx));
      state.selectedIdx = idx;
    }
  } else if (e.ctrlKey || e.metaKey) {
    if (state.selectedIdxs.has(idx)) state.selectedIdxs.delete(idx);
    else state.selectedIdxs.add(idx);
    state.selectedIdx = idx;
    state.selectAnchor = idx;
  } else {
    state.selectedIdx = idx;
    state.selectAnchor = idx;
    state.selectedIdxs = new Set([idx]);
  }
  applySelectionClasses();
  treemap.setSelected(state.selectedIdx);
  if (state.selectedIdxs.size > 1) {
    elStatusSummary.textContent = `${state.selectedIdxs.size} items selected — Del to recycle, right-click for actions`;
  }
  if (document.querySelector("#tab-inspect.active")) refreshInspector();
}

/** Navigate the view to a folder by idx, no dir-membership guard. Used by the
 *  breadcrumb, where every segment is by definition an ancestor directory —
 *  drillInto()'s nameIsDir guard would reject these because ancestors aren't in
 *  the current view's dirIdxs set. */
async function navigateToFolder(idx: number) {
  if (state.currentRoot === idx) return;
  const seq = ++drillSeq;
  state.currentRoot = idx;
  state.selectedIdx = null;
  state.selectedIdxs.clear();
  expandedIdxs.clear();
  pushLoading("Loading…");
  try {
    await refreshAll(seq);
  } catch (e) {
    if (seq === drillSeq) elStatusSummary.textContent = `Navigate failed: ${(e as Error)?.message ?? e}`;
  } finally {
    popLoading();
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
  state.selectAnchor = row.idx;
  state.selectedIdxs = new Set([row.idx]);
  treemap.setSelected(row.idx);
  scrollFlatIndexIntoView(clamped);
  renderDirWindow(true); // re-render applies .selected from state
  if (document.querySelector("#tab-inspect.active")) refreshInspector();
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

// ---------- toasts ----------

type ToastKind = "info" | "success" | "error" | "warn";
const elToastStack = $("#toast-stack");

/** Non-blocking notification. Errors stay until dismissed; others auto-expire.
 *  Replaces blocking alert() for status/error surfacing. */
function toast(message: string, kind: ToastKind = "info", ms?: number) {
  const el = document.createElement("div");
  el.className = `toast toast-${kind}`;
  el.setAttribute("role", kind === "error" ? "alert" : "status");
  const icon = kind === "error" ? "✕" : kind === "success" ? "✓" : kind === "warn" ? "⚠" : "ℹ";
  el.innerHTML = `<span class="toast-icon">${icon}</span><span class="toast-msg"></span><button class="toast-x" aria-label="Dismiss">✕</button>`;
  el.querySelector(".toast-msg")!.textContent = message;
  const remove = () => {
    el.classList.add("leaving");
    setTimeout(() => el.remove(), 180);
  };
  el.querySelector(".toast-x")!.addEventListener("click", remove);
  elToastStack.appendChild(el);
  // Errors persist (user must read/dismiss); informational ones auto-expire.
  const life = ms ?? (kind === "error" ? 9000 : kind === "warn" ? 6000 : 3500);
  setTimeout(remove, life);
}

/** Convenience for `catch` blocks: surface a failed action as an error toast. */
function toastErr(prefix: string, e: unknown) {
  toast(`${prefix}: ${(e as Error)?.message ?? e}`, "error");
}

// ---------- help overlay ----------

const HELP_SHORTCUTS: { keys: string; desc: string }[] = [
  { keys: "↑ / ↓", desc: "Move selection" },
  { keys: "→ / ←", desc: "Expand-or-enter / collapse-or-parent" },
  { keys: "Enter", desc: "Drill into folder / open file" },
  { keys: "Backspace", desc: "Go up one level" },
  { keys: "PageUp / PageDown", desc: "Jump a screenful" },
  { keys: "Home / End", desc: "First / last row" },
  { keys: "type letters", desc: "Type-ahead jump to a row" },
  { keys: "F2", desc: "Rename selected" },
  { keys: "Delete", desc: "Recycle selected" },
  { keys: "Shift + Delete", desc: "Permanently delete selected" },
  { keys: "F5", desc: "Rescan current root" },
  { keys: "Ctrl + F", desc: "Jump to Search" },
  { keys: "? or F1", desc: "Show this help" },
  { keys: "Esc", desc: "Close menus / dialogs" },
];

function toggleHelp() {
  if (document.getElementById("help-overlay")) closeHelp();
  else openHelp();
}

function closeHelp() {
  document.getElementById("help-overlay")?.remove();
}

function openHelp() {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.id = "help-overlay";
  const rows = HELP_SHORTCUTS.map(
    (s) =>
      `<div class="help-row"><kbd>${escapeHtml(s.keys)}</kbd><span>${escapeHtml(s.desc)}</span></div>`,
  ).join("");
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
      <div class="modal-head">
        <div class="modal-title">Keyboard shortcuts</div>
        <button class="btn ghost small" id="help-close" aria-label="Close">✕</button>
      </div>
      <div class="modal-body" style="min-width:420px">${rows}</div>
    </div>`;
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeHelp();
  });
  backdrop.querySelector("#help-close")?.addEventListener("click", closeHelp);
  document.body.appendChild(backdrop);
  (backdrop.querySelector("#help-close") as HTMLElement)?.focus();
}

// ---------- settings ----------

function openSettings() {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  const themeSel = (v: string) => (state.themeFollowsSystem ? "system" : state.theme) === v ? "selected" : "";
  const dupOpts = [
    [0, "any size"],
    [4096, "4 KB"],
    [1048576, "1 MB"],
    [10485760, "10 MB"],
    [104857600, "100 MB"],
  ] as const;
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true" aria-label="Settings">
      <div class="modal-head">
        <div class="modal-title">Settings</div>
        <button class="btn ghost small" id="set-close" aria-label="Close">✕</button>
      </div>
      <div class="modal-body settings-body" style="min-width:460px">
        <label class="set-row"><span>Theme</span>
          <select id="set-theme">
            <option value="system" ${themeSel("system")}>Follow system</option>
            <option value="light" ${themeSel("light")}>Light</option>
            <option value="dark" ${themeSel("dark")}>Dark</option>
          </select>
        </label>
        <label class="set-row"><span>Default size mode</span>
          <select id="set-sizemode">
            <option value="allocated" ${state.sizeMode === "allocated" ? "selected" : ""}>On disk</option>
            <option value="logical" ${state.sizeMode === "logical" ? "selected" : ""}>Logical</option>
          </select>
        </label>
        <label class="set-row"><span>Treemap depth</span>
          <input type="number" id="set-depth" min="2" max="6" value="${state.treemapDepth}" />
        </label>
        <label class="set-row"><span>Duplicate finder — minimum size</span>
          <select id="set-dupemin">
            ${dupOpts.map(([v, t]) => `<option value="${v}" ${state.dupeMinSize === v ? "selected" : ""}>${t}</option>`).join("")}
          </select>
        </label>
        <div class="set-row set-col">
          <span>Scan exclusions <span class="muted small">(one glob per line — e.g. <code>node_modules</code>, <code>*.tmp</code>, <code>C:\\Windows\\*</code>). Applies to the next scan.</span></span>
          <textarea id="set-excludes" rows="6" spellcheck="false" placeholder="node_modules&#10;*.tmp"></textarea>
        </div>
      </div>
      <div class="modal-foot">
        <span class="muted small" style="margin-right:auto">Settings save to a portable file next to the app.</span>
        <button class="btn" id="set-done">Done</button>
      </div>
    </div>`;
  const exTa = backdrop.querySelector("#set-excludes") as HTMLTextAreaElement;
  exTa.value = state.excludes.join("\n");

  document.body.appendChild(backdrop);
  const close = () => backdrop.remove();
  backdrop.querySelector("#set-close")?.addEventListener("click", close);
  backdrop.querySelector("#set-done")?.addEventListener("click", () => {
    // Commit exclusions on close (textarea isn't live).
    state.excludes = exTa.value
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    saveConfig();
    close();
  });
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) {
      state.excludes = exTa.value.split("\n").map((s) => s.trim()).filter(Boolean);
      saveConfig();
      close();
    }
  });

  // Live-apply the simple toggles.
  backdrop.querySelector("#set-theme")?.addEventListener("change", (e) => {
    const v = (e.target as HTMLSelectElement).value;
    if (v === "system") {
      state.themeFollowsSystem = true;
      state.theme = prefersDark() ? "dark" : "light";
    } else {
      state.themeFollowsSystem = false;
      state.theme = v === "dark" ? "dark" : "light";
    }
    applyTheme();
    saveConfig();
  });
  backdrop.querySelector("#set-sizemode")?.addEventListener("change", (e) => {
    setSizeMode((e.target as HTMLSelectElement).value as SizeMode);
  });
  backdrop.querySelector("#set-depth")?.addEventListener("change", (e) => {
    setTreemapDepth(Number((e.target as HTMLInputElement).value) || state.treemapDepth);
  });
  backdrop.querySelector("#set-dupemin")?.addEventListener("change", (e) => {
    state.dupeMinSize = Number((e.target as HTMLSelectElement).value) || 0;
    saveConfig();
  });
}

/** Activate the Search side-tab and focus its input (Ctrl+F). */
function openSearchTab() {
  document
    .querySelectorAll(".side-tabs .tab")
    .forEach((x) => x.classList.toggle("active", (x as HTMLElement).dataset.tab === "search"));
  document
    .querySelectorAll(".tab-pane")
    .forEach((x) => x.classList.toggle("active", x.id === "tab-search"));
  elSearchInput.focus();
  elSearchInput.select();
}

async function drillInto(idx: number) {
  if (!nameIsDir(idx)) return;
  const name = state.rectNames.get(idx) || "(node)";
  const seq = ++drillSeq;
  state.currentRoot = idx;
  state.selectedIdx = null;
  state.selectedIdxs.clear();
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
  state.selectedIdxs.clear();
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

// Single- vs. double-click disambiguation for list rows. A plain click is
// deferred this long; a double-click within the window cancels it and runs the
// drill/open instead. Roughly the OS double-click time.
const DOUBLE_CLICK_MS = 230;
let pendingRowClick: number | undefined;
function cancelPendingRowClick() {
  if (pendingRowClick !== undefined) {
    clearTimeout(pendingRowClick);
    pendingRowClick = undefined;
  }
}

async function toggleExpand(idx: number) {
  if (expandedIdxs.has(idx)) expandedIdxs.delete(idx);
  else expandedIdxs.add(idx);
  // Bump drillSeq so any in-flight refresh from a prior drill/tab-switch is
  // invalidated and can't overwrite this expand's result. Re-flatten + re-render,
  // keeping the user's scroll position (expand-in-place).
  const seq = ++drillSeq;
  await refreshDirList(seq, true);
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
  const typesP = typesTabActive()
    ? refreshExtensions(seq).catch((e) => console.error("types", e))
    : Promise.resolve();
  await Promise.all([breadcrumbP, treemapP, dirListP, topNP, typesP]);
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
    b.addEventListener("click", () => navigateToFolder(c.idx));
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
  const maxDepth = state.treemapDepth;
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
  let rootSize = 0;
  for (const r of rects) {
    state.rectNames.set(r.idx, r.name);
    if (r.is_dir) dirIdxs.add(r.idx);
    if (r.idx === state.currentRoot) rootSize = r.size;
    if (r.size > rootSize && r.depth <= 1) rootSize = Math.max(rootSize, r.size);
  }
  // Size of the view's root, for the tooltip's "% of view" readout.
  treemapRootSize = rootSize || rects.reduce((m, r) => Math.max(m, r.size), 0);
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

/** The complete flattened list for the current view (root's children + any
 *  inline-expanded subtrees), before the Contents filter is applied. */
let allRows: FlatRow[] = [];
/** The currently *visible* rows — `allRows` narrowed by the live Contents
 *  filter. Selection, keyboard nav, and the virtual scroller all operate on
 *  this, so they automatically respect the filter. */
let flatRows: FlatRow[] = [];
/** Live substring filter for the Contents list (client-side, no IPC). */
let contentsFilter = "";

/** Narrow `allRows` by the current filter into `flatRows`, then re-render. */
function applyContentsFilter(preserveScroll = false) {
  const q = contentsFilter.trim().toLowerCase();
  flatRows = q ? allRows.filter((f) => f.row.name.toLowerCase().includes(q)) : allRows;
  renderDirWindow(preserveScroll);
}

async function refreshDirList(seq: number = drillSeq, preserveScroll = false) {
  if (state.currentRoot === null) return;
  // A fresh drill/navigate (not an in-place expand) clears the live filter so
  // the new folder shows in full.
  if (!preserveScroll && contentsFilter) {
    contentsFilter = "";
    elContentsFilter.value = "";
  }
  const PER_LEVEL_LIMIT = 8192;
  const next: FlatRow[] = [];

  const visit = async (parent: number, depth: number): Promise<void> => {
    const rows = await ipc.listDir(parent, state.sort, 0, PER_LEVEL_LIMIT, state.sizeMode);
    if (seq !== drillSeq) return;
    for (const row of rows) {
      next.push({ row, depth });
      if (expandedIdxs.has(row.idx) && row.is_dir && !row.is_reparse) {
        await visit(row.idx, depth + 1);
        if (seq !== drillSeq) return;
      }
    }
  };
  await visit(state.currentRoot, 0);
  if (seq !== drillSeq) return;

  // Commit to the global note sets only after the final seq guard, so a
  // superseded in-flight render can never pollute dirIdxs/reparseIdxs/rectNames
  // with rows belonging to a different drill or tab.
  for (const f of next) noteRow(f.row);
  allRows = next;
  // Fresh drill resets scroll to top; chevron-expands keep the user in place.
  applyContentsFilter(preserveScroll);
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

// ---------- export ----------

/** Export the current view's subtree to CSV or JSON. The format is inferred
 *  from the extension the user picks in the save dialog (default CSV). */
async function exportCurrentTree() {
  const root = state.currentRoot;
  if (root === null) return;
  const base =
    (state.scanRootPath.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "treelens")
      .replace(/[^A-Za-z0-9._-]+/g, "_") || "treelens";
  try {
    const { save: saveDialog } = await import("@tauri-apps/plugin-dialog");
    const dest = await saveDialog({
      defaultPath: `${base}.csv`,
      filters: [
        { name: "CSV", extensions: ["csv"] },
        { name: "JSON", extensions: ["json"] },
      ],
    });
    if (!dest) return;
    const format = dest.toLowerCase().endsWith(".json") ? "json" : "csv";
    pushLoading("Exporting…");
    let count: number;
    try {
      count = await ipc.exportTree(root, format, dest);
    } finally {
      popLoading();
    }
    elStatusSummary.textContent = `Exported ${fmtCount(count)} rows to ${dest}`;
  } catch (e) {
    elStatusSummary.textContent = `Export failed: ${(e as Error)?.message ?? e}`;
  }
}

// ---------- recent scans ----------

function pushRecent(path: string) {
  const p = path.replace(/[\\/]+$/, "");
  if (!p) return;
  // Case-insensitive dedupe (Windows paths), most-recent-first, capped.
  state.recents = [p, ...state.recents.filter((x) => x.toLowerCase() !== p.toLowerCase())].slice(
    0,
    RECENTS_MAX,
  );
  saveConfig();
}

function toggleRecentsMenu() {
  if (!elRecentsMenu.hidden) {
    elRecentsMenu.hidden = true;
    return;
  }
  elRecentsMenu.innerHTML = "";
  if (state.recents.length === 0) {
    const empty = document.createElement("div");
    empty.className = "recents-empty muted small";
    empty.textContent = "No recent scans yet.";
    elRecentsMenu.appendChild(empty);
  } else {
    for (const p of state.recents) {
      const item = document.createElement("button");
      item.className = "recents-item";
      item.type = "button";
      item.title = p;
      const seg = p.split(/[\\/]/).pop() || p;
      item.innerHTML = `<span class="recents-name">${escapeHtml(seg)}</span><span class="recents-path">${escapeHtml(p)}</span>`;
      item.addEventListener("click", () => {
        elRecentsMenu.hidden = true;
        startScan(p);
      });
      elRecentsMenu.appendChild(item);
    }
    const clear = document.createElement("button");
    clear.className = "recents-item recents-clear";
    clear.type = "button";
    clear.textContent = "Clear recent scans";
    clear.addEventListener("click", () => {
      state.recents = [];
      saveConfig();
      elRecentsMenu.hidden = true;
    });
    elRecentsMenu.appendChild(clear);
  }
  elRecentsMenu.hidden = false;
}

// ---------- save / open scan ----------

async function saveCurrentScan() {
  if (state.scanRoot === null) return;
  const base =
    (state.scanRootPath.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "scan").replace(
      /[^A-Za-z0-9._-]+/g,
      "_",
    ) || "scan";
  try {
    const { save: saveDialog } = await import("@tauri-apps/plugin-dialog");
    const dest = await saveDialog({
      defaultPath: `${base}.treelens`,
      filters: [{ name: "Treelens scan", extensions: ["treelens"] }],
    });
    if (!dest) return;
    pushLoading("Saving scan…");
    let n: number;
    try {
      n = await ipc.saveScan(dest);
    } finally {
      popLoading();
    }
    toast(`Saved scan (${fmtCount(n)} nodes) to ${dest}`, "success");
  } catch (e) {
    toastErr("Save scan failed", e);
  }
}

async function openSavedScan() {
  try {
    const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
    const picked = await openDialog({
      multiple: false,
      filters: [{ name: "Treelens scan", extensions: ["treelens"] }],
    });
    const path = Array.isArray(picked) ? picked[0] : picked;
    if (!path) return;
    pushLoading("Opening scan…");
    let result;
    try {
      result = await ipc.openScan(path);
    } finally {
      popLoading();
    }
    // Render it exactly like a completed live scan in the active tab.
    await handleScanComplete(result);
    toast("Scan opened.", "success");
  } catch (e) {
    toastErr("Open scan failed", e);
  }
}

// ---------- column sort ----------

// Per-column toggle targets and the direction to use when first switching to a
// column. Name reads best ascending; size and date most-useful biggest/newest
// first. The "% of parent" header shares the size key (it's size-derived).
const SORT_FOR: Record<string, { asc: SortKey; desc: SortKey; first: SortKey }> = {
  name: { asc: "name_asc", desc: "name_desc", first: "name_asc" },
  size: { asc: "size_asc", desc: "size_desc", first: "size_desc" },
  mtime: { asc: "mtime_asc", desc: "mtime_desc", first: "mtime_desc" },
};

function colOfSort(s: SortKey): { col: string; dir: "asc" | "desc" } | null {
  switch (s) {
    case "name_asc": return { col: "name", dir: "asc" };
    case "name_desc": return { col: "name", dir: "desc" };
    case "size_asc": return { col: "size", dir: "asc" };
    case "size_desc": return { col: "size", dir: "desc" };
    case "mtime_asc": return { col: "mtime", dir: "asc" };
    case "mtime_desc": return { col: "mtime", dir: "desc" };
    default: return null; // count_desc has no header
  }
}

function setupColumnSort() {
  document.querySelectorAll<HTMLElement>("#list-header .sortable").forEach((h) => {
    h.addEventListener("click", () => {
      const col = h.dataset.sortcol!;
      const map = SORT_FOR[col];
      if (!map) return;
      const cur = colOfSort(state.sort);
      // Same column → flip; different column → that column's natural default.
      state.sort =
        cur && cur.col === col ? (cur.dir === "asc" ? map.desc : map.asc) : map.first;
      saveConfig();
      updateSortIndicators();
      if (state.currentRoot !== null) refreshDirList(++drillSeq, false);
    });
  });
  updateSortIndicators();
}

function updateSortIndicators() {
  const cur = colOfSort(state.sort);
  document.querySelectorAll<HTMLElement>("#list-header .sortable").forEach((h) => {
    const ind = h.querySelector(".sort-ind");
    const active = cur && cur.col === h.dataset.sortcol;
    h.classList.toggle("sorted", !!active);
    if (ind) ind.textContent = active ? (cur!.dir === "asc" ? " ▲" : " ▼") : "";
  });
}

// ---------- file-type breakdown ----------

function typesTabActive(): boolean {
  return !!document.querySelector("#tab-types.active");
}

async function refreshExtensions(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  let stats: ExtStat[];
  try {
    stats = await ipc.extensionBreakdown(state.currentRoot, state.sizeMode, 200);
  } catch {
    return;
  }
  if (seq !== drillSeq) return;
  if (stats.length === 0) {
    elTypesList.innerHTML = '<div class="search-hint muted small">No files here.</div>';
    return;
  }
  const max = stats[0].size || 1;
  const frag = document.createDocumentFragment();
  for (const s of stats) {
    const row = document.createElement("div");
    row.className = "list-row types-row";
    const pct = Math.max(2, Math.round((s.size / max) * 100));
    row.innerHTML =
      `<span class="col col-name"><span class="ext-bar" style="width:${pct}%"></span>` +
      `<span class="ext-name">${escapeHtml(s.ext)}</span></span>` +
      `<span class="col col-size">${escapeHtml(fmtBytes(s.size))}</span>` +
      `<span class="col col-count">${escapeHtml(fmtCount(s.count))}</span>`;
    frag.appendChild(row);
  }
  elTypesList.innerHTML = "";
  elTypesList.appendChild(frag);
}

// ---------- search ----------

let searchKind: SearchKind = "all";
let searchSeq = 0;
let searchDebounce: ReturnType<typeof setTimeout> | null = null;

function setupSearch() {
  elSearchInput.addEventListener("input", () => {
    if (searchDebounce) clearTimeout(searchDebounce);
    searchDebounce = setTimeout(runSearch, 180);
  });
  elSearchInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      if (searchDebounce) clearTimeout(searchDebounce);
      runSearch();
    } else if (e.key === "Escape") {
      elSearchInput.value = "";
      runSearch();
    }
  });
  elSearchMinSize.addEventListener("change", runSearch);
  document.querySelectorAll<HTMLElement>("[data-skind]").forEach((b) => {
    b.addEventListener("click", () => {
      searchKind = (b.dataset.skind as SearchKind) || "all";
      document
        .querySelectorAll("[data-skind]")
        .forEach((x) => x.classList.toggle("active", x === b));
      runSearch();
    });
  });
}

async function runSearch() {
  const query = elSearchInput.value.trim();
  const minSize = Number(elSearchMinSize.value) || 0;
  const root = state.currentRoot ?? state.scanRoot;
  // Nothing to search against, and no useful query/filter — show the hint.
  if (root === null || (query === "" && minSize === 0 && searchKind === "all")) {
    elSearchList.innerHTML =
      '<div class="search-hint muted small">Type to search the current folder and everything under it.</div>';
    return;
  }
  const seq = ++searchSeq;
  try {
    const hits = await ipc.search(root, query, minSize, searchKind, 500, state.sizeMode);
    if (seq !== searchSeq) return; // a newer search superseded us
    renderSearchResults(hits);
  } catch (e) {
    if (seq !== searchSeq) return;
    elSearchList.innerHTML = `<div class="search-hint muted small">Search failed: ${escapeHtml(
      (e as Error)?.message ?? String(e),
    )}</div>`;
  }
}

function renderSearchResults(hits: SearchHit[]) {
  if (hits.length === 0) {
    elSearchList.innerHTML = '<div class="search-hint muted small">No matches.</div>';
    return;
  }
  const frag = document.createDocumentFragment();
  for (const h of hits) {
    const el = document.createElement("div");
    el.className = "list-row search-row" + (h.is_dir ? " dir" : "");
    el.dataset.idx = String(h.idx);
    const icon = h.is_dir ? "📁" : "📄";
    el.innerHTML =
      `<span class="col col-name"><span class="row-icon">${icon}</span>` +
      `<span class="search-name">${escapeHtml(h.name)}</span>` +
      `<span class="search-path">${escapeHtml(h.path)}</span></span>` +
      `<span class="col col-size">${escapeHtml(fmtBytes(h.size))}</span>` +
      `<span class="col col-mtime">${escapeHtml(fmtMtime(h.mtime))}</span>`;
    el.addEventListener("click", () => revealSearchHit(h));
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      openCtxMenu(h.idx, e.clientX, e.clientY);
    });
    frag.appendChild(el);
  }
  elSearchList.innerHTML = "";
  elSearchList.appendChild(frag);
}

/** Jump from a search result to where it lives: drill into a folder, or open
 *  the file's containing folder and select it. Then switch to Contents. */
async function revealSearchHit(h: SearchHit) {
  if (h.is_dir) {
    await navigateToFolder(h.idx);
  } else {
    const crumbs = await ipc.breadcrumb(h.idx).catch(() => []);
    const parent = crumbs.length >= 2 ? crumbs[crumbs.length - 2].idx : state.scanRoot;
    if (parent !== null && parent !== undefined && parent !== state.currentRoot) {
      await navigateToFolder(parent);
    }
    selectNode(h.idx);
  }
  document
    .querySelectorAll(".side-tabs .tab")
    .forEach((x) => x.classList.toggle("active", (x as HTMLElement).dataset.tab === "contents"));
  document
    .querySelectorAll(".tab-pane")
    .forEach((x) => x.classList.toggle("active", x.id === "tab-contents"));
}

function renderRow(row: DirRow, depth: number = 0): HTMLElement {
  const el = document.createElement("div");
  el.className =
    "list-row" +
    (row.is_dir ? " dir" : "") +
    (state.selectedIdxs.has(row.idx) ? " selected" : "");
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

  // Row interactions are handled by event delegation on the list containers
  // (see setupRowDelegation) rather than per-row listeners, so re-rendering the
  // virtual window doesn't churn thousands of handlers.
  return el;
}

/** Resolve the row idx for a delegated list event, or null if not on a row. */
function rowIdxFromEvent(e: Event): number | null {
  const rowEl = (e.target as HTMLElement).closest<HTMLElement>(".list-row");
  if (!rowEl || rowEl.dataset.idx === undefined) return null;
  const n = Number(rowEl.dataset.idx);
  return Number.isNaN(n) ? null : n;
}

/** One set of delegated handlers per list container. Reads the row idx from the
 *  target's `.list-row` ancestor and looks type up via the dirIdxs/reparseIdxs
 *  sets, matching the old per-row behavior exactly. */
function setupRowDelegation() {
  for (const list of [elDirList, elTopFilesList, elTopDirsList]) {
    list.addEventListener("click", (e) => {
      const idx = rowIdxFromEvent(e);
      if (idx === null) return;
      e.stopPropagation();
      // Chevron → toggle inline expansion without selecting/drilling.
      if ((e.target as HTMLElement).closest(".chev")) {
        toggleExpand(idx);
        return;
      }
      if (e.ctrlKey || e.metaKey || e.shiftKey) {
        cancelPendingRowClick();
        clickSelect(idx, e);
        return;
      }
      // Defer the single-click; a dblclick within the window cancels it.
      cancelPendingRowClick();
      pendingRowClick = window.setTimeout(() => {
        pendingRowClick = undefined;
        selectNode(idx);
        if (nameIsDir(idx) && !reparseIdxs.has(idx)) toggleExpand(idx);
      }, DOUBLE_CLICK_MS);
    });
    list.addEventListener("dblclick", (e) => {
      const idx = rowIdxFromEvent(e);
      if (idx === null) return;
      e.stopPropagation();
      cancelPendingRowClick();
      if (nameIsDir(idx) && !reparseIdxs.has(idx)) drillInto(idx);
      else ipc.openFile(idx).catch(() => ipc.openInExplorer(idx).catch(() => {}));
    });
    list.addEventListener("contextmenu", (e) => {
      const idx = rowIdxFromEvent(e);
      if (idx === null) return;
      e.preventDefault();
      e.stopPropagation();
      openCtxMenu(idx, (e as MouseEvent).clientX, (e as MouseEvent).clientY);
    });
  }
}

// ---------- treemap chrome (depth + legend) ----------

function setTreemapDepth(d: number) {
  const next = Math.min(6, Math.max(2, d));
  if (next === state.treemapDepth) return;
  state.treemapDepth = next;
  elDepthVal.textContent = String(next);
  saveConfig();
  if (state.currentRoot !== null) refreshTreemap(++drillSeq);
}

// Representative categories for the type-mode legend (hue matches colors.ts).
const LEGEND_TYPES: { label: string; hue: number }[] = [
  { label: "Code", hue: 215 },
  { label: "Docs", hue: 200 },
  { label: "Data", hue: 100 },
  { label: "Images", hue: 75 },
  { label: "Audio", hue: 145 },
  { label: "Video", hue: 285 },
  { label: "Archives", hue: 32 },
  { label: "Binaries", hue: 0 },
];

function renderLegend() {
  const dark = state.theme === "dark";
  if (state.colorMode === "heat") {
    const stops = [
      { c: hsl(0, 0.65, dark ? 0.5 : 0.6), t: "new" },
      { c: hsl(60, 0.65, dark ? 0.5 : 0.6), t: "~6mo" },
      { c: hsl(120, 0.65, dark ? 0.5 : 0.6), t: "~1yr" },
      { c: hsl(220, 0.65, dark ? 0.5 : 0.6), t: "old" },
    ];
    elTreemapLegend.innerHTML =
      `<span class="legend-title">Age</span>` +
      stops
        .map(
          (s) =>
            `<span class="legend-item"><span class="legend-swatch" style="background:${s.c}"></span>${s.t}</span>`,
        )
        .join("");
    return;
  }
  const s = dark ? 0.5 : 0.55;
  const l = dark ? 0.48 : 0.62;
  elTreemapLegend.innerHTML =
    `<span class="legend-title">Type</span>` +
    LEGEND_TYPES.map(
      (c) =>
        `<span class="legend-item"><span class="legend-swatch" style="background:${hsl(
          c.hue,
          s,
          l,
        )}"></span>${c.label}</span>`,
    ).join("");
}

/** Show/hide the depth control + legend depending on whether a treemap is up. */
function updateTreemapChrome() {
  const show = state.currentRoot !== null && !state.scanning;
  elDepthCtl.hidden = !show;
  elTreemapLegend.hidden = !show;
  if (show) renderLegend();
}

// ---------- size mode + theme ----------

function setSizeMode(mode: SizeMode) {
  if (state.sizeMode === mode) return;
  state.sizeMode = mode;
  elModeAlloc.classList.toggle("active", mode === "allocated");
  elModeLogical.classList.toggle("active", mode === "logical");
  saveConfig();
  // Bump the seq so an in-flight refresh (from a drill or the prior size mode)
  // can't race its results in over this one. The breadcrumb is independent of
  // size mode, so we refresh only the size-sensitive views, not the path.
  if (state.currentRoot !== null) refreshSizeSensitive(++drillSeq);
}

/** Refresh only the views whose numbers depend on the size mode (treemap, dir
 *  list, top-N). Skips the breadcrumb, which is purely the path. */
async function refreshSizeSensitive(seq: number = drillSeq) {
  if (state.currentRoot === null) return;
  const treemapP = refreshTreemap(seq).catch((e) => console.error("treemap", e));
  const dirListP = refreshDirList(seq).catch((e) => console.error("dirList", e));
  const topNP = refreshTopN(seq).catch((e) => console.error("topN", e));
  const typesP = typesTabActive()
    ? refreshExtensions(seq).catch((e) => console.error("types", e))
    : Promise.resolve();
  await Promise.all([treemapP, dirListP, topNP, typesP]);
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
    renderDriveCards();
  } else {
    elTreemapEmpty.hidden = true;
  }
  updateTreemapChrome();
}

/** Populate the empty-state with a card per drive (usage bar + capacity);
 *  clicking a card scans that drive. Best-effort — silent if drives can't list. */
let driveCardsLoading = false;
async function renderDriveCards() {
  if (driveCardsLoading) return;
  driveCardsLoading = true;
  let drives: DriveEntry[];
  try {
    drives = await ipc.listDrives();
  } catch {
    driveCardsLoading = false;
    return;
  }
  driveCardsLoading = false;
  elDriveCards.innerHTML = "";
  for (const d of drives) {
    const used = d.total > 0 ? 1 - d.free / d.total : 0;
    const pct = Math.round(used * 100);
    const card = document.createElement("button");
    card.className = "drive-card";
    card.type = "button";
    card.title = `Scan ${d.letter}`;
    // Tint the bar red as a drive fills up.
    const hue = used > 0.9 ? 0 : used > 0.75 ? 30 : 150;
    card.innerHTML =
      `<div class="drive-card-top"><span class="drive-card-letter">${escapeHtml(d.letter)}</span>` +
      `<span class="drive-card-label">${escapeHtml(d.label || (d.fs ? d.fs + " drive" : "Local drive"))}</span></div>` +
      `<div class="drive-card-bar"><div class="drive-card-fill" style="width:${pct}%;background:hsl(${hue} 65% 50%)"></div></div>` +
      `<div class="drive-card-sub">${escapeHtml(fmtBytes(d.free))} free of ${escapeHtml(fmtBytes(d.total))} · ${pct}% used</div>`;
    card.addEventListener("click", () => startScan(d.letter));
    elDriveCards.appendChild(card);
  }
}

// ---------- tooltip ----------

let treemapRootSize = 0;

function updateTooltip(rect: Rect | null, x: number, y: number) {
  if (!rect) {
    elTooltip.hidden = true;
    return;
  }
  const name = state.rectNames.get(rect.idx) || "";
  const icon = rect.is_dir ? "📁" : "📄";
  const pct = treemapRootSize > 0 ? (rect.size / treemapRootSize) * 100 : 0;
  const pctStr = pct >= 0.1 ? `${pct.toFixed(1)}% of view` : "<0.1% of view";
  // Age: show the subtree's newest activity, and a range if it spans time.
  let ageRow = "";
  if (rect.newest_mtime > 0) {
    const newest = fmtMtime(rect.newest_mtime);
    if (rect.is_dir && rect.oldest_mtime > 0 && rect.oldest_mtime !== rect.newest_mtime) {
      ageRow = `<div class="t-row t-muted">${escapeHtml(fmtMtime(rect.oldest_mtime))} – ${escapeHtml(newest)}</div>`;
    } else {
      ageRow = `<div class="t-row t-muted">modified ${escapeHtml(newest)}</div>`;
    }
  }
  elTooltip.innerHTML =
    `<div class="t-name">${icon} ${escapeHtml(name || "(unknown)")}</div>` +
    `<div class="t-row t-size">${escapeHtml(fmtBytes(rect.size))} · ${pctStr}</div>` +
    `<div class="t-row t-muted">${rect.is_dir ? "folder" : "file"}${rect.is_dir ? " · double-click to drill in" : ""}</div>` +
    ageRow;
  elTooltip.hidden = false;
  const pad = 12;
  // Flip to the other side of the cursor when near the right/bottom edges.
  const w = elTooltip.offsetWidth;
  const h = elTooltip.offsetHeight;
  let tx = x + pad;
  if (tx + w > window.innerWidth - pad) tx = x - w - pad;
  let ty = y + pad;
  if (ty + h > window.innerHeight - pad) ty = y - h - pad;
  elTooltip.style.left = Math.max(pad, tx) + "px";
  elTooltip.style.top = Math.max(pad, ty) + "px";
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
      : { label: "Open (edit)", shortcut: "Enter", action: () => ipc.openFile(idx).catch((e) => toastErr("Open failed", e)) },
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
      { label: "Find reclaimable junk (logs, temp, dumps)…", action: () => runJunkFinder(idx) },
      { label: "Find files older than 1 year (≥10 MB)", action: () => runSuperSkillOldFiles(idx) },
      { label: "Find empty folders", action: () => runSuperSkillEmpty(idx) },
    ] : []),
    { label: "—", action: () => {} },
    {
      label:
        state.selectedIdxs.size > 1 && state.selectedIdxs.has(idx)
          ? `Move ${state.selectedIdxs.size} items to Recycle Bin…`
          : "Move to Recycle Bin…",
      danger: true,
      shortcut: "Del",
      action: () => confirmRecycle(idx),
    },
    {
      label:
        state.selectedIdxs.size > 1 && state.selectedIdxs.has(idx)
          ? `Delete ${state.selectedIdxs.size} items permanently…`
          : "Delete permanently…",
      danger: true,
      shortcut: "Shift+Del",
      action: () => confirmDeletePermanent(idx),
    },
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
    toastErr("Create failed", e);
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
    toastErr("Rename failed", e);
  }
}

async function confirmRecycle(idx: number) {
  // If the clicked row is part of a multi-selection, recycle the whole set.
  const multi =
    state.selectedIdxs.size > 1 && state.selectedIdxs.has(idx)
      ? [...state.selectedIdxs]
      : null;
  if (multi) {
    if (!confirm(`Move ${multi.length} items to the Recycle Bin?\n\nYou can restore them from Explorer's Recycle Bin.`)) return;
    try {
      const n = await ipc.recycleNodes(multi);
      elStatusSummary.textContent = `Recycled ${n} items`;
      state.selectedIdxs.clear();
      if (state.scanRootPath) startScan(state.scanRootPath);
    } catch (e) {
      toastErr("Recycle failed", e);
    }
    return;
  }
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
    toastErr("Recycle failed", e);
  }
}

/** Permanently delete (bypasses the Recycle Bin — unrecoverable). Gated behind
 *  a deliberately strong confirmation that names what will be destroyed. */
async function confirmDeletePermanent(idx: number) {
  const multi =
    state.selectedIdxs.size > 1 && state.selectedIdxs.has(idx)
      ? [...state.selectedIdxs]
      : null;

  const idxs = multi ?? [idx];
  if (multi) {
    if (
      !confirm(
        `⚠ PERMANENTLY delete ${multi.length} items?\n\n` +
          `This bypasses the Recycle Bin. The data CANNOT be recovered.\n\n` +
          `Click OK only if you are sure.`,
      )
    )
      return;
  } else {
    const path = await ipc.copyPath(idx).catch(() => "");
    const summary = await ipc.nodeSummary(idx).catch(() => null);
    const size = summary ? fmtBytes(state.sizeMode === "allocated" ? summary.allocated : summary.logical) : "";
    if (
      !confirm(
        `⚠ PERMANENTLY delete this ${summary?.is_dir ? "folder and everything in it" : "file"}?\n\n` +
          `${path}\n${size ? `(${size})` : ""}\n\n` +
          `This bypasses the Recycle Bin. The data CANNOT be recovered.`,
      )
    )
      return;
  }

  pushLoading(`Deleting ${idxs.length} item${idxs.length > 1 ? "s" : ""}…`);
  try {
    const r = await ipc.deletePermanentNodes(idxs);
    if (r.failed > 0) {
      // Most common cause: the file is held open by another process (e.g.
      // OneDrive's own .odl logs, an open document, a running program).
      elStatusSummary.textContent = `Deleted ${r.deleted} of ${r.requested}`;
      toast(
        `Deleted ${r.deleted} of ${r.requested}. ${r.failed} could not be deleted — ` +
          `most likely in use by another program (e.g. OneDrive keeps its logs open). ` +
          `Close it and try again.`,
        "warn",
      );
    } else {
      elStatusSummary.textContent = `Permanently deleted ${r.deleted} item${r.deleted > 1 ? "s" : ""}`;
    }
    state.selectedIdxs.clear();
  } catch (e) {
    toastErr("Delete failed", e);
  } finally {
    popLoading();
    // Always rescan so the view matches reality (whatever got deleted is gone;
    // anything that survived still shows).
    if (state.scanRootPath) startScan(state.scanRootPath);
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
    toastErr("Search failed", e);
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
    toastErr("Search failed", e);
  }
}

/** "Bullshit file detector": scan the subtree for reclaimable junk (logs,
 *  temp, dumps, backups, empty files, temp/cache/log folders), show the total
 *  reclaimable space, and offer one-click recycle or permanent delete. */
async function runJunkFinder(idx: number) {
  pushLoading("Scanning for reclaimable junk…");
  let report;
  try {
    report = await ipc.findJunk(idx, 5000);
  } catch (e) {
    popLoading();
    toastErr("Junk scan failed", e);
    return;
  }
  popLoading();
  if (report.total_files === 0) {
    toast("No obvious junk found here — no logs, temp files, dumps, or empty files.", "info");
    return;
  }
  showJunkModal(report);
}

function showJunkModal(report: import("./ipc").JunkReport) {
  const paths = report.files.map((f) => f.path);
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  const truncNote = report.truncated
    ? ` (showing largest ${report.files.length.toLocaleString()})`
    : "";
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true">
      <div class="modal-head">
        <div class="modal-title">Reclaimable junk — ${report.total_files.toLocaleString()} files, ${fmtBytes(report.total_bytes)}${truncNote}</div>
        <button class="btn ghost small" id="jk-close">✕</button>
      </div>
      <div class="modal-body" style="max-height:55vh; min-width: 640px"></div>
      <div class="modal-foot">
        <span class="muted small" style="margin-right:auto">Logs, temp, dumps, backups, empty files, and temp/cache/log folders.</span>
        <button class="btn" id="jk-recycle">Recycle all (safe)</button>
        <button class="btn" id="jk-delete" style="border-color:var(--danger);color:var(--danger)">Delete all permanently</button>
        <button class="btn" id="jk-cancel">Close</button>
      </div>
    </div>`;
  const body = backdrop.querySelector(".modal-body")!;
  for (const f of report.files) {
    const row = document.createElement("div");
    row.className = "drive-row";
    row.style.gridTemplateColumns = "1fr 90px 130px";
    row.innerHTML = `
      <div><div class="drive-label" style="font-family:var(--font-mono);font-size:11px;word-break:break-all">${escapeHtml(f.path)}</div></div>
      <div class="drive-sub" style="text-align:right">${fmtBytes(f.size)}</div>
      <div class="drive-sub" style="text-align:right">${escapeHtml(f.category)}</div>`;
    body.appendChild(row);
  }
  document.body.appendChild(backdrop);
  const close = () => backdrop.remove();
  backdrop.querySelector("#jk-close")?.addEventListener("click", close);
  backdrop.querySelector("#jk-cancel")?.addEventListener("click", close);
  backdrop.addEventListener("click", (e) => { if (e.target === backdrop) close(); });

  backdrop.querySelector("#jk-recycle")?.addEventListener("click", async () => {
    if (!confirm(`Move ${paths.length.toLocaleString()} junk files to the Recycle Bin?\n\nYou can restore them from Explorer if needed.`)) return;
    close();
    pushLoading("Recycling junk…");
    try {
      const n = await ipc.recyclePaths(paths);
      elStatusSummary.textContent = `Recycled ${n} of ${paths.length} junk files (${fmtBytes(report.total_bytes)} flagged)`;
    } catch (e) {
      toastErr("Recycle failed", e);
    } finally {
      popLoading();
      if (state.scanRootPath) startScan(state.scanRootPath);
    }
  });

  backdrop.querySelector("#jk-delete")?.addEventListener("click", async () => {
    if (!confirm(`⚠ PERMANENTLY delete ${paths.length.toLocaleString()} junk files?\n\nThis bypasses the Recycle Bin and CANNOT be undone.`)) return;
    close();
    pushLoading("Deleting junk…");
    try {
      const n = await ipc.deletePermanentPaths(paths);
      const failed = paths.length - n;
      elStatusSummary.textContent = `Deleted ${n} of ${paths.length} junk files`;
      if (failed > 0) {
        toast(`Deleted ${n} of ${paths.length}. ${failed} could not be deleted — likely in use by another program (e.g. OneDrive holds its current logs open). Close that program and retry.`, "warn");
      }
    } catch (e) {
      toastErr("Delete failed", e);
    } finally {
      popLoading();
      if (state.scanRootPath) startScan(state.scanRootPath);
    }
  });
}

async function runDuplicateFinder(idx: number) {
  pushLoading("Hashing files to find duplicates…");
  let report;
  try {
    // Skip tiny files (configurable in Settings): small dupes inflate the list.
    report = await ipc.findDuplicates(idx, state.dupeMinSize);
  } catch (e) {
    popLoading();
    toastErr("Duplicate scan failed", e);
    return;
  }
  popLoading();
  if (report.total_groups === 0) {
    toast("No duplicate files found here (files ≥ 4 KB compared by content).", "info");
    return;
  }
  showDupesModal(report);
}

function showDupesModal(report: import("./ipc").DupeReport) {
  // Recycle all but the first copy in every group.
  const redundant = report.groups.flatMap((g) => g.paths.slice(1));
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  const truncNote = report.truncated
    ? ` (showing top ${report.groups.length.toLocaleString()})`
    : "";
  backdrop.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true">
      <div class="modal-head">
        <div class="modal-title">Duplicates — ${report.total_groups.toLocaleString()} groups, ${fmtBytes(
          report.total_redundant_bytes,
        )} reclaimable${truncNote}</div>
        <button class="btn ghost small" id="dp-close">✕</button>
      </div>
      <div class="modal-body" style="max-height:55vh; min-width: 660px"></div>
      <div class="modal-foot">
        <span class="muted small" style="margin-right:auto">Each group is byte-identical. "Recycle redundant" keeps the first copy of each.</span>
        <button class="btn" id="dp-recycle">Recycle redundant (${redundant.length.toLocaleString()})</button>
        <button class="btn" id="dp-cancel">Close</button>
      </div>
    </div>`;
  const body = backdrop.querySelector(".modal-body")!;
  for (const g of report.groups) {
    const group = document.createElement("div");
    group.className = "dupe-group";
    const head = document.createElement("div");
    head.className = "dupe-group-head";
    head.innerHTML = `<span>${g.paths.length} copies · ${escapeHtml(
      fmtBytes(g.size),
    )} each · <span class="muted">${escapeHtml(fmtBytes(g.redundant_bytes))} reclaimable</span></span>`;
    group.appendChild(head);
    g.paths.forEach((p, i) => {
      const row = document.createElement("div");
      row.className = "dupe-path" + (i === 0 ? " keep" : "");
      row.innerHTML =
        `<span class="dupe-tag">${i === 0 ? "keep" : "dup"}</span>` +
        `<span class="dupe-pathtext">${escapeHtml(p)}</span>`;
      group.appendChild(row);
    });
    body.appendChild(group);
  }
  document.body.appendChild(backdrop);
  const close = () => backdrop.remove();
  backdrop.querySelector("#dp-close")?.addEventListener("click", close);
  backdrop.querySelector("#dp-cancel")?.addEventListener("click", close);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) close();
  });
  backdrop.querySelector("#dp-recycle")?.addEventListener("click", async () => {
    if (redundant.length === 0) return;
    if (
      !confirm(
        `Move ${redundant.length.toLocaleString()} redundant copies to the Recycle Bin?\n\nOne copy of each set is kept. You can restore from Explorer if needed.`,
      )
    )
      return;
    close();
    pushLoading("Recycling duplicates…");
    try {
      const n = await ipc.recyclePaths(redundant);
      elStatusSummary.textContent = `Recycled ${n} of ${redundant.length} duplicate copies (${fmtBytes(
        report.total_redundant_bytes,
      )} flagged)`;
    } catch (e) {
      toastErr("Recycle failed", e);
    } finally {
      popLoading();
      if (state.scanRootPath) startScan(state.scanRootPath);
    }
  });
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
      toastErr("Checksum failed", e);
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
            : r.length_only_diff
              ? `✗ Differ — same content, one is longer (${fmtBytes(r.size_a)} vs ${fmtBytes(r.size_b)})`
              : `✗ Differ — sizes ${fmtBytes(r.size_a)} vs ${fmtBytes(r.size_b)}`;
        showResultsModal("Compare result", [
          { label: note, sub: "" },
          { label: `A: ${compareMarkName}`, sub: `${fmtBytes(r.size_a)} · ${r.sha256_a}` },
          { label: `B: ${name}`, sub: `${fmtBytes(r.size_b)} · ${r.sha256_b}` },
        ]);
      } catch (e) {
        toastErr("Compare failed", e);
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
      toastErr("Stego scan failed", e);
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
    toastErr("Extract failed", e);
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
    toastErr("Embed failed", e);
  }
}

// ---------- config ----------

const CONFIG_KEY = "treelens.ui.v1";

function applyConfig(raw: string | null) {
  try {
    if (raw) {
      const v = JSON.parse(raw) as Partial<UiState>;
      if (v.theme === "light" || v.theme === "dark") state.theme = v.theme;
      if (typeof v.themeFollowsSystem === "boolean") state.themeFollowsSystem = v.themeFollowsSystem;
      if (v.sizeMode === "logical" || v.sizeMode === "allocated") state.sizeMode = v.sizeMode;
      if (v.colorMode === "type" || v.colorMode === "heat") state.colorMode = v.colorMode;
      if (v.sort) state.sort = v.sort;
      if (Array.isArray(v.excludes)) state.excludes = v.excludes.filter((x) => typeof x === "string");
      if (typeof v.treemapDepth === "number")
        state.treemapDepth = Math.min(6, Math.max(2, Math.round(v.treemapDepth)));
      if (Array.isArray(v.recents))
        state.recents = v.recents.filter((x) => typeof x === "string").slice(0, RECENTS_MAX);
      if (typeof v.dupeMinSize === "number" && v.dupeMinSize >= 0)
        state.dupeMinSize = Math.round(v.dupeMinSize);
    }
  } catch {}
  // Apply visible state to controls.
  elModeAlloc.classList.toggle("active", state.sizeMode === "allocated");
  elModeLogical.classList.toggle("active", state.sizeMode === "logical");
  elHeatBtn.setAttribute("aria-pressed", state.colorMode === "heat" ? "true" : "false");
  treemap.setMode(state.colorMode);
}

/** Load settings from the portable on-disk config file (written next to the
 *  exe, or %APPDATA%\Treelens as a fallback). Falls back to the localStorage
 *  cache if the disk read is empty or the backend is unavailable. */
async function loadConfig() {
  let raw: string | null = null;
  try {
    const disk = await ipc.loadConfig();
    if (disk && disk.trim()) raw = disk;
  } catch {}
  if (!raw) {
    try {
      raw = localStorage.getItem(CONFIG_KEY);
    } catch {}
  }
  applyConfig(raw);
}

function saveConfig() {
  const v: Partial<UiState> = {
    theme: state.theme,
    themeFollowsSystem: state.themeFollowsSystem,
    sizeMode: state.sizeMode,
    colorMode: state.colorMode,
    sort: state.sort,
    excludes: state.excludes,
    treemapDepth: state.treemapDepth,
    recents: state.recents,
    dupeMinSize: state.dupeMinSize,
  };
  const json = JSON.stringify(v);
  // Fast local cache (sync) + durable portable file (async, best-effort).
  try { localStorage.setItem(CONFIG_KEY, json); } catch {}
  ipc.saveConfig(json).catch(() => {});
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
