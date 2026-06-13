# Changelog

All notable changes to Treelens are documented here.
The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning is [SemVer](https://semver.org/) (0.x while pre-1.0).

## [0.1.1] — 2026-06-12

Hotfix for three real bugs found scanning a 396 GB / 1.5 M-file Windows home directory.

### Fixed
- **Tree topology was scrambled.** The scanner emits records concurrently from
  many worker threads, but tagged each record's `parent` with the scanner-local
  idx — while the receiver indexed records by their Vec position. Because
  workers race, Vec position ≠ scanner idx, and `Tree::build` cross-attached
  arbitrary subtrees to the wrong parent. The visible symptom: a 38-char
  git-object FILE in `.vault-git\objects\07\<hash>` had 159 GB of AppData /
  OneDrive content reported as nested inside it. Fix: stamp each record with
  its scanner-allocated idx; sort by idx in `Tree::build`.
- **Top files and top folders contained duplicates.** The arena layout in
  v0.1.0 reordered nodes via DFS preorder, which does NOT put siblings
  contiguously — a parent's `first_child + child_count` range sliced across
  the prior sibling's subtree, so `walk_subtree` revisited deep nodes
  multiple times. (Six rows of `devsense.php.ls.exe` at one path, 24 of
  `intelliphp.ls.exe`.) Fix: BFS (level-order) layout — siblings are
  genuinely contiguous.
- **OneDrive placeholders inflated "size on disk."** Files with
  FILE_ATTRIBUTE_OFFLINE / RECALL_ON_DATA_ACCESS / RECALL_ON_OPEN occupy
  ~0 bytes locally; v0.1 cluster-rounded their logical bytes and reported
  that. Now: allocated = 0 for cloud placeholders.
- **Side-panel name column was ~60 px.** With the 380 px pane and three
  fixed columns of 90/100/110 px, the name flex cell got 60 px → ~4 chars
  visible → `$Rec` instead of `$Recycle.Bin`, no ellipsis. Widened the pane
  to 460 px, shrunk fixed columns to 78/100/78, and moved the ellipsis
  CSS onto its own `.filename` span where it can actually trigger.
- **% OF PARENT bars rendered as empty space.** The `.pct-bar` span was
  inline by default, so its `width: 90px` was ignored. Now `inline-block`.

### Improved
- `top_dirs` suppresses "passthrough" ancestors — directories where one child
  accounts for ≥95% of the size. Stops the panel from showing
  `AppData / Local / Microsoft / OneDrive / logs / ListSync / Local`
  as seven near-identical rows.

### Added
- `crates/scanner/examples/diag.rs` — `cargo run -p scanner --example diag --release -- <path>` dumps totals, reparse counts, top-30 dirs/files of a real path. Used to verify the fixes.
- Three new regression tests: junction-not-double-counted, robust-to-shuffled-emission, deeper-tree-no-duplicate-idxs.

[0.1.1]: https://github.com/skyflyt/treelens/releases/tag/v0.1.1

## [0.1.0] — 2026-06-12

First public release. The MUST-tier feature set from `PLAN.md §4` is in.

### Added
- **Scanner.** Parallel directory walk (rayon-based work-stealing pool, large-buffer
  `read_dir`), ~10 Hz progress events, cooperative cancellation, no admin required,
  works on NTFS / exFAT / ReFS / network shares. Reparse points are surfaced as
  zero-size badge'd leaves (no traversal — zero cycles, zero double-count).
- **Arena tree** in `crates/tree`: flat `Vec<Node>` with `u32` indexes, contiguous
  children, bottom-up size + count + mtime aggregation.
- **Squarified treemap layout** (Bruls/Huizing/van Wijk) computed in Rust;
  frontend renders the flat `Rect[]` to a single canvas with file-type hue,
  depth-tinted directory headers, hover highlight, click select,
  double-click drill-in.
- **Age-heat color mode** — warm-to-cold gradient over modification age,
  overlaid on the treemap. One-click toggle in the toolbar.
- **Side panel** with three tabs: Contents (sortable virtualized list with
  inline % bars), Top files (top-50), Top folders (top-50).
- **Breadcrumb navigation** + <kbd>Backspace</kbd> drill-up + <kbd>F5</kbd> rescan.
- **Size mode toggle** — "size on disk" (allocated, default) vs. logical bytes.
- **File ops:** recycle-bin delete (via `IFileOperation`), "Open in Explorer"
  (via `SHOpenFolderAndSelectItems`), "Open in Terminal" (Windows Terminal
  with PowerShell fallback), "Copy full path".
- **Super-skill helpers** — "Find files older than 1 year (≥10 MB)" and
  "Find empty folders" under any subtree (right-click a directory).
- **Admin banner** — runs degraded without admin and nags subtly; one-click
  `runas` relaunch.
- **Light + dark theme** following `prefers-color-scheme` by default; manual
  override persisted.
- **Drive picker modal** listing all logical drives with label, filesystem,
  free/used capacity bar. "Pick folder…" falls through to the OS folder picker.
- **CLI flag:** `treelens --scan <path>` auto-starts a scan after launch.

### Build / release
- Cargo workspace: `crates/scanner`, `crates/tree`, `crates/fileops`, `src-tauri`.
- TypeScript UI under `ui/` (Vite, no framework — vanilla TS).
- GitHub Actions: CI on PR (fmt, clippy, tests, typecheck, debug Tauri build);
  release on `v*` tag push (portable EXE + NSIS installer + SHA-256 sums).
- Self-designed SVG icon → multi-resolution `.ico` (sizes 16/32/48/64/128/256).

### Known v0.1 limitations
- **No MFT fast path yet.** Scans use the parallel walk; the MFT-native
  ~5 s / 5M-file MFT path is v0.2.
- **Allocated size** is computed as logical size cluster-aligned to 4 KB —
  accurate to within a cluster for typical files, but not for compressed,
  sparse, or OneDrive-placeholder files. The `FileIdBothDirInfo` accurate
  path is v0.2.
- Config persistence uses the WebView's localStorage; the planned `treelens.config.json`
  next-to-exe (USB-friendly) is v0.2.
- Side-panel virtualization is naive (renders all rows up to a cap of 1000);
  full windowed virtualization is a v0.x cleanup.

[0.1.0]: https://github.com/skyflyt/treelens/releases/tag/v0.1.0
