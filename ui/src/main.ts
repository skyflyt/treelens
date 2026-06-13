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
  type DirRow,
  type Rect,
  type SizeMode,
  type SortKey,
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

  // Tab switching for side panel.
  document.querySelectorAll(".tab").forEach((t) => {
    t.addEventListener("click", () => {
      const tab = (t as HTMLElement).dataset.tab!;
      document.querySelectorAll(".tab").forEach((x) => x.classList.toggle("active", x === t));
      document.querySelectorAll(".tab-pane").forEach((x) => x.classList.toggle("active", x.id === `tab-${tab}`));
    });
  });

  // Keyboard shortcuts.
  document.addEventListener("keydown", (e) => {
    if (e.target instanceof HTMLInputElement) return;
    if (e.key === "Backspace") {
      e.preventDefault();
      drillUp();
    } else if (e.key === "F5") {
      e.preventDefault();
      if (state.scanRootPath) startScan(state.scanRootPath);
    } else if (e.key === "F2") {
      e.preventDefault();
      if (state.selectedIdx !== null) promptRename(state.selectedIdx);
    } else if (e.key === "Delete") {
      e.preventDefault();
      if (state.selectedIdx !== null) confirmRecycle(state.selectedIdx);
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (state.selectedIdx !== null) {
        if (nameIsDir(state.selectedIdx) && !reparseIdxs.has(state.selectedIdx)) {
          drillInto(state.selectedIdx);
        } else {
          ipc.openFile(state.selectedIdx).catch(() => {});
        }
      }
    } else if (e.key === "Escape") {
      closeCtxMenu();
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
