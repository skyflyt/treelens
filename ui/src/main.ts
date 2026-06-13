/**
 * Treelens main entry. Wires DOM events to IPC, manages frontend state,
 * coordinates the treemap canvas and the side panels.
 */
import {
  ipc,
  onScanCancelled,
  onScanComplete,
  onScanProgress,
  type DirRow,
  type Rect,
  type SizeMode,
  type SortKey,
} from "./ipc";
import { Treemap, type TreemapTheme } from "./treemap";
import { fmtBytes, fmtCount, fmtDuration, fmtMtime, fmtPct } from "./format";
import { type ColorMode } from "./colors";
import { pickScanRoot } from "./drives";

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
  const { listen } = await import("@tauri-apps/api/event");
  listen<string>("scan:auto", (e) => {
    if (typeof e.payload === "string" && e.payload.length > 0) {
      startScan(e.payload);
    }
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
  await refreshAll();
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

async function drillInto(idx: number) {
  // Only drill into directories.
  const name = state.rectNames.get(idx) || "(node)";
  if (!nameIsDir(idx)) return;
  state.currentRoot = idx;
  state.selectedIdx = null;
  expandedIdxs.clear();
  await refreshAll();
  const summary = await ipc.nodeSummary(idx).catch(() => null);
  elStatusSummary.textContent = summary
    ? `${escapeText(summary.full_path)} — ${fmtBytes(state.sizeMode === "allocated" ? summary.allocated : summary.logical)} · ${fmtCount(summary.file_count)} files`
    : name;
}

async function drillUp() {
  if (state.currentRoot === null || state.scanRoot === null) return;
  if (state.currentRoot === state.scanRoot) return;
  const crumbs = await ipc.breadcrumb(state.currentRoot).catch(() => []);
  if (crumbs.length < 2) return;
  state.currentRoot = crumbs[crumbs.length - 2].idx;
  state.selectedIdx = null;
  expandedIdxs.clear();
  await refreshAll();
}

// We stash is_dir per visible idx because list rows and breadcrumb come from
// different IPC calls; nameIsDir checks the most recent dir list and treemap rects.
const dirIdxs = new Set<number>();
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
  await refreshDirList();
}

// ---------- refresh ----------

async function refreshAll() {
  if (state.currentRoot === null) return;
  await Promise.all([refreshBreadcrumb(), refreshTreemap(), refreshDirList(), refreshTopN()]);
}

async function refreshBreadcrumb() {
  if (state.currentRoot === null) return;
  const crumbs = await ipc.breadcrumb(state.currentRoot).catch(() => []);
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

async function refreshTreemap() {
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
  // Build a name lookup from a one-deep list_dir call so the treemap can label
  // the visible directories. For non-list visible idxs (depth >= 2), we leave
  // the name empty; the dir header label only renders when the rect is large.
  state.rectNames.clear();
  dirIdxs.clear();
  // We need names for all visible idxs; fetch the full direct child list and
  // also the deeper rects' names via repeated queries is wasteful — instead
  // fetch a flat name map by walking the visible rects and looking up siblings.
  // v0.1: only label depth-1 rects (parents seen directly). For deeper rects,
  // use a synthetic "" so the canvas skips the label.
  const directChildren = await ipc.listDir(state.currentRoot, "size_desc", 0, 4096, state.sizeMode);
  for (const r of directChildren) {
    state.rectNames.set(r.idx, r.name);
    if (r.is_dir) dirIdxs.add(r.idx);
  }
  // Also record dir-ness for visible rects so right-click knows.
  for (const r of rects) {
    if (r.is_dir) dirIdxs.add(r.idx);
  }
  treemap.setData(rects, state.currentRoot, (idx) => state.rectNames.get(idx) || "");
}

async function refreshDirList() {
  if (state.currentRoot === null) return;
  const saved = elDirList.scrollTop;
  const frag = document.createDocumentFragment();
  await renderListLevel(state.currentRoot, 0, frag);
  elDirList.innerHTML = "";
  elDirList.appendChild(frag);
  elDirList.scrollTop = saved;
}

/** Recursively render one parent's children at the given depth, descending into
 *  any expanded directories (chevron-toggle). Each row gets a left-indent of
 *  16px per level so the hierarchy reads at a glance. */
async function renderListLevel(parent: number, depth: number, frag: DocumentFragment) {
  const rows = await ipc.listDir(parent, state.sort, 0, 1000, state.sizeMode);
  for (const row of rows) {
    if (row.is_dir) dirIdxs.add(row.idx);
    state.rectNames.set(row.idx, row.name);
    frag.appendChild(renderRow(row, depth));
    if (expandedIdxs.has(row.idx) && row.is_dir && !row.is_reparse) {
      await renderListLevel(row.idx, depth + 1, frag);
    }
  }
}

async function refreshTopN() {
  if (state.currentRoot === null) return;
  const t = await ipc.topN(state.currentRoot, 50, state.sizeMode);
  elTopFilesList.innerHTML = "";
  elTopDirsList.innerHTML = "";
  const ff = document.createDocumentFragment();
  for (const r of t.files) {
    state.rectNames.set(r.idx, r.name);
    ff.appendChild(renderRow(r));
  }
  elTopFilesList.appendChild(ff);
  const fd = document.createDocumentFragment();
  for (const r of t.dirs) {
    state.rectNames.set(r.idx, r.name);
    if (r.is_dir) dirIdxs.add(r.idx);
    fd.appendChild(renderRow(r));
  }
  elTopDirsList.appendChild(fd);
}

function renderRow(row: DirRow, depth: number = 0): HTMLElement {
  const el = document.createElement("div");
  el.className = "list-row" + (row.is_dir ? " dir" : "");
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
  const items: CtxItem[] = [
    isDir
      ? { label: "Drill into folder", shortcut: "Enter", action: () => drillInto(idx) }
      : { label: "Open in Explorer", action: () => ipc.openInExplorer(idx).catch(() => {}) },
    { label: "Reveal in Explorer", action: () => ipc.openInExplorer(idx).catch(() => {}) },
    ...(isDir ? [{ label: "Open in Terminal", action: () => ipc.openInTerminal(idx).catch(() => {}) }] : []),
    { label: "Copy full path", action: async () => {
      try {
        const p = await ipc.copyPath(idx);
        await navigator.clipboard.writeText(p);
      } catch {}
    } },
    ...(isDir ? [
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
