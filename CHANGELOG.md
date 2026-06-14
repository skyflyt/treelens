# Changelog

All notable changes to Treelens are documented here.
The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning is [SemVer](https://semver.org/) (0.x while pre-1.0).

## [0.6.0] — 2026-06-13

A UI/UX-focused pass (ten sprints) polishing the whole experience.

### Added
- **Drive overview cards** on the empty state — a card per drive with a usage
  bar; click to scan.
- **Command palette** (`Ctrl`/`Cmd`+`K`) — fuzzy-search and run any action
  (scan, export, dupes, settings, theme, …) from the keyboard.
- **Selection action bar** — selecting 2+ items raises a floating bar with the
  count, total size, and Recycle / Delete / Clear.
- **Resizable + collapsible side panel** (drag handle, persisted width, collapse
  toggle with a reopen tab).
- **Richer treemap tooltip** (name, size, % of view, type, modification age) and
  **animated drill-in/out transitions**.
- **Breadcrumb overflow** with a "…" dropdown for deep paths, and an
  indeterminate **scan progress bar**.
- **Row density** toggle (comfortable/compact) and keyboard **focus rings**.

### Changed
- Context menu: per-item icons, viewport-aware flipping, and full keyboard
  navigation (↑/↓/Enter/Esc).
- Micro-interactions across buttons, tabs, panes, and modals — all gated on
  `prefers-reduced-motion`.

[0.6.0]: https://github.com/skyflyt/treelens/releases/tag/v0.6.0

## [0.5.0] — 2026-06-13

A second ten-sprint pass focused on features and usability.

### Added
- **Scan exclusions** — glob patterns (name globs like `node_modules` / `*.tmp`,
  or full-path globs like `C:\Windows\*`) are skipped at scan time; edited in
  the new Settings panel and persisted.
- **File-type breakdown** (Types tab) — size and count aggregated by extension
  with inline bars.
- **Save & open scan snapshots** — write a scan to a portable `.treelens` file
  and reopen it later without rescanning.
- **Live Contents filter** — an instant, client-side name filter over the
  current folder.
- **Treemap depth control + color legend + keyboard navigation** — choose how
  many levels render, read the type/age color key, and move/drill with the
  arrow keys + Enter.
- **Recent scans** menu next to the Scan button.
- **Inaccessible-items reporting** — a status-bar pill shows how many entries a
  scan couldn't read and lists a sample of the actual paths.
- **Settings panel** — theme, default size mode, treemap depth, duplicate-finder
  minimum size, and exclusions, all saved to the portable config file.

### Changed
- The duplicate finder's minimum size is now configurable (was a hardcoded
  4 KiB).
- The Contents / Top-files / Top-folders lists use event delegation instead of
  per-row listeners.

### Tooling / tests
- New scanner glob + exclusion tests, fileops delete/empty-dir tests, tree
  extension-breakdown + search tests, and a scan-snapshot round-trip test.

[0.5.0]: https://github.com/skyflyt/treelens/releases/tag/v0.5.0

## [0.4.0] — 2026-06-13

A hardening + features release from a full audit and deep-dive test, delivered
as ten focused sprints.

### Added
- **Search & filter** (Search side-tab): find files/folders anywhere under the
  current view by name substring, with size and files/dirs-only filters; results
  show full paths and jump you to the item. `Ctrl/Cmd+F` opens it.
- **Click-to-sort columns** in the Contents list (Name / Size / % / Modified)
  with a direction indicator; choice persists.
- **Export** the current subtree to **CSV or JSON** (streamed, scales to whole
  drives).
- **Duplicate finder**: byte-identical files via a size → 4 KiB-prefix-hash →
  full-hash funnel, grouped most-reclaimable-first, with one-click "recycle
  redundant copies (keep one)".
- **Toast notifications** for non-blocking success/warn/error surfacing, plus a
  **keyboard-shortcuts help overlay** (`?` / `F1`).
- **Portable on-disk config** (`treelens.config.json` next to the exe, or
  `%APPDATA%\Treelens` fallback) so settings travel with a portable copy.

### Changed
- **Treemap performance**: the static layer is cached to an offscreen canvas, so
  hover/selection no longer re-render every rect — large maps stay smooth.
  Window-resize bursts are coalesced to one relayout per frame.
- File-comparison reads each file **once** (was twice) in a single streaming
  pass, and reports an explicit *length-only difference*.
- The size-mode toggle no longer refetches the breadcrumb.

### Fixed
- **Core correctness**: `Tree::build` is now orphan/cycle-safe and no longer
  relies on a release-stripped debug assert; `path()` is bounds/cycle-guarded;
  the scanner guards a reparse-point scan root, caps `u32` index overflow, and
  cancels on a dead record receiver.
- **Cross-tab scan correctness**: scan events carry their tab id, so a scan that
  finishes in a background tab updates that tab's snapshot instead of corrupting
  the foreground view. Added a per-tab scan watchdog and centralized stale-render
  (`drillSeq`) guards on expand and size-mode changes.
- **Steganography accuracy**: PNG/JPEG/GIF appended-data detection now parses
  real container structure (chunk/segment/block walks) instead of substring-
  searching for end markers — correct even when markers appear in pixel data or
  the payload; whitespace capacity matches what embed accepts; LSB chi-square is
  windowed to catch partial embeds.

### Tooling
- CI: version-sync check across package.json / tauri.conf.json / Cargo.toml,
  Node 22, the Tauri crate's tests + `--selftest` run after the UI builds, and a
  criterion build-bench compile check. Added `dependabot.yml`, `CONTRIBUTING.md`,
  and `SECURITY.md`. Release also syncs the tag version into package.json so the
  in-app version label is correct.

[0.4.0]: https://github.com/skyflyt/treelens/releases/tag/v0.4.0

## [0.3.7] — 2026-06-13

### Changed
- **Folder click behavior.** Single-clicking a folder in the Contents list now
  **expands/collapses** it in place (like the chevron); **double-click** drills
  in / filters the view to that folder. Previously a single click drilled in,
  which made casual browsing jumpy. Files are unchanged (single-click selects,
  double-click opens). A short single-vs-double-click timer keeps a double-click
  from toggling the expand on its way to drilling. Ctrl/Shift-click still
  multi-selects; the breadcrumb and Enter/arrow-key navigation are unchanged.

[0.3.7]: https://github.com/skyflyt/treelens/releases/tag/v0.3.7

## [0.3.6] — 2026-06-13

### Fixed
- **Permanent delete now actually deletes large batches.** It was using the
  shell `IFileOperation` COM API, which silently no-ops / fails when driven
  headless from a worker thread with no message pump — so deleting tens of
  thousands of files (e.g. 110 GB of OneDrive ListSync `.odl` logs) appeared to
  do nothing. Switched permanent delete to a direct `std::fs` syscall
  (`DeleteFileW` / recursive `RemoveDirectory`) — the same reliable path
  `del` / PowerShell use, which clears 100k+ files in seconds. (Confirmed
  against the real OneDrive log folder: 113,098 / 113,099 deleted, the one
  survivor being the single log OneDrive currently holds open.) Recycle still
  uses the shell API (it has to, to reach the Recycle Bin).

[0.3.6]: https://github.com/skyflyt/treelens/releases/tag/v0.3.6

## [0.3.5] — 2026-06-13

### Added
- **Reclaimable-junk finder** ("🧹 Junk" toolbar button, or right-click a
  folder → "Find reclaimable junk…"). Walks the subtree and flags throwaway
  files — `.log` / `.odl` / `.etl` logs, `.tmp`/`.temp`, crash dumps
  (`.dmp`/`.mdmp`), `.bak`/`.old` backups, zero-byte files, `Thumbs.db`, and
  anything sitting inside `temp` / `cache` / `logs` folders — and shows the
  total reclaimable space. One click to **Recycle all (safe)** or **Delete all
  permanently**, with the same honest "deleted X of N — Y in use" reporting.
  Real files (documents, images, code, etc.) are deliberately left alone.
  This surfaces things like OneDrive's runaway ListSync `.odl` logs as the
  top reclaimable item. Classification is unit-tested.

[0.3.5]: https://github.com/skyflyt/treelens/releases/tag/v0.3.5

## [0.3.4] — 2026-06-13

### Fixed
- **Permanent delete hung, popped a Windows dialog, and could silently fail.**
  v0.3.3 ran `IFileOperation` with only `FOF_NOCONFIRMATION`, so the shell still
  showed its own progress / "file in use" dialog — which misbehaves when called
  from a worker thread with no message pump (the multi-second hang you saw). And
  if items were locked (e.g. OneDrive's `.odl` ListSync logs, held open by the
  running OneDrive process), they were skipped with no feedback, so files
  "disappeared" from the dialog but remained on disk.
  Now permanent delete runs fully **headless** (no shell dialogs) and **verifies
  what actually got removed** by re-checking the paths afterward. The UI reports
  honestly: "Deleted X of N — Y could not be deleted (in use by another
  program)", shows a "Deleting…" indicator, and always rescans. Treelens can't
  force-delete a file another process holds open (close that program and retry).

[0.3.4]: https://github.com/skyflyt/treelens/releases/tag/v0.3.4

## [0.3.3] — 2026-06-13

### Added
- **Permanent delete** (bypasses the Recycle Bin). Right-click → "Delete
  permanently…" or **Shift+Delete**; works on a single item or a whole
  multi-selection. Gated behind a deliberately strong, unrecoverable-warning
  confirmation. Implemented via `IFileOperation` without the recycle/undo flags
  (`delete_permanent` / `delete_permanent_many`); `--selftest` covers it.
  Plain **Delete** / "Move to Recycle Bin" is unchanged (still the safe,
  undo-able default).

[0.3.3]: https://github.com/skyflyt/treelens/releases/tag/v0.3.3

## [0.3.2] — 2026-06-13

### Fixed
- **Couldn't navigate back up the tree via the breadcrumb.** Breadcrumb
  segments called `drillInto`, which guards on "is this idx a directory in the
  current view" — but ancestors aren't in the current view's set, so the guard
  silently rejected every up-click. Breadcrumb segments now use a dedicated
  `navigateToFolder` with no such guard (an ancestor crumb is a directory by
  definition). Backspace-to-go-up was unaffected; this was breadcrumb-only.

### Added
- **Multi-select** in the Contents list: **Ctrl-click** toggles individual
  rows, **Shift-click** selects a range, plain click still single-selects (and
  still drills into folders). The selection count shows in the status bar.
- **Bulk recycle**: with several rows selected, **Delete** or right-click →
  "Move N items to Recycle Bin…" sends them all to the Recycle Bin in one
  shell operation (new `recycle_nodes` command over `IFileOperation`).

[0.3.2]: https://github.com/skyflyt/treelens/releases/tag/v0.3.2

## [0.3.1] — 2026-06-13

### Fixed
- **Layout collapsed to a thin strip when the admin banner was hidden.** v0.3.0
  added the tab bar as a new CSS-grid row. When the "switch to admin" banner is
  hidden (running elevated, or dismissed), Chromium drops the `display:none`
  banner from grid auto-placement, which shifted the main content area into the
  32px tab-bar track and the footer into the `1fr` track — so the treemap and
  side panel rendered as a ~30px sliver at the top of an otherwise black window,
  and the Contents list looked empty. Fixed by pinning every region to an
  explicit `grid-row`, so a hidden banner just leaves its (auto) row empty
  without moving anything else. Verified elevated (banner hidden) on a live
  scan — treemap and panel now fill the window.

[0.3.1]: https://github.com/skyflyt/treelens/releases/tag/v0.3.1

## [0.3.0] — 2026-06-13

Treelens grows a forensics/analysis layer, multi-tab scanning, and world-class
keyboard navigation.

### Added — analysis
- **Checksums.** CRC32 / MD5 / SHA-1 / SHA-256 of any file, computed in one
  streaming pass (Inspect panel). Known-vector tested.
- **File comparison.** Mark a file, then "Compare with…" another: identical?
  first-differing byte offset? size delta? side-by-side SHA-256.
- **Steganography toolkit** (`crates/analysis`) — detect, **reverse/extract**,
  and (for round-trip testing / watermarking your own files) embed hidden data
  by three classic techniques, each with a framed `TLNS`-magic payload so
  recovery is unambiguous:
  - **LSB** (PNG/BMP) — least-significant-bit embedding in pixel data, plus a
    chi-square "pairs of values" detector that flags other tools' heavy LSB
    embedding as a statistical advisory.
  - **Whitespace / SNOW** — trailing space/tab per line (space=0, tab=1), with
    a trailing-whitespace-fraction advisory.
  - **Format-based** — payload appended after a file's logical EOF
    (PNG `IEND` / JPEG `EOI` / GIF trailer); always recoverable as raw bytes.
  - Detection separates a **definitive "found & extractable"** verdict from a
    weaker **statistical advisory**, so there are no cry-wolf false positives.
  - Extracted payloads can be saved to a file.

### Added — UI
- **Tabs.** Multiple independent scans, each with its own tree (kept in the
  Rust backend, keyed by tab id) and view state. New-tab button, click to
  switch, close button; tabs are named after the scanned folder.
- **Inspect panel** (4th side-panel view): per-file checksums, a one-click
  steganography scan with per-method verdicts (found / advisory / clean),
  extract buttons for recoverable payloads, an embed action (writes a new
  `.stego` copy, never touches the original), and the compare flow.
- **World-class keyboard tree navigation:** ↑/↓ move selection (auto-scroll
  into view), → expand or step in, ← collapse or jump to parent, Enter
  drill/open, PageUp/PageDown, Home/End, and type-ahead to jump to a name by
  typing. Selection survives virtual-scroll re-renders.

### Engineering
- Backend tree store refactored from a single tree to a per-tab `HashMap`
  behind an `RwLock`; every query/op command now carries a `tab` id.
- `--selftest` extended to round-trip checksums, compare, and all three stego
  methods (embed → detect → extract → verify) — runs ALL PASS in the shipping
  binary, in both regular and admin contexts.

### Note on the steganography feature
This is a **local forensic/analysis** capability: it reads and writes only
files you select on your own machine, and nothing leaves the machine. The
headline use is *detecting and reversing* hidden data; the embed side exists
for round-trip testing and watermarking your own files.

[0.3.0]: https://github.com/skyflyt/treelens/releases/tag/v0.3.0

## [0.2.0] — 2026-06-13

Treelens becomes a real file explorer with size superpowers: create, rename,
open(edit), and recycle files directly, and a tree that works all the way down
to the individual file with no row cap.

### Added
- **Create / rename / open files** — Treelens is no longer read-only-plus-recycle:
  - **New folder** and **New file** — toolbar buttons (`＋ Folder`, `＋ File`,
    act on the current folder) and right-click → New folder/New file on any
    directory. Never clobbers an existing entry.
  - **Rename** — right-click → Rename… or press <kbd>F2</kbd> on the selected row.
  - **Open (edit)** — right-click a file → Open (edit), press <kbd>Enter</kbd>,
    or double-click; launches the file in its default app (ShellExecute "open").
  - All names are validated against path-traversal and illegal Windows
    filename characters before the operation runs.
  - After any mutation the current root is re-scanned so the view reflects the
    change immediately.
- **Real side-panel virtualization.** The Contents list now renders only the
  rows visible in the viewport (windowed by `padding-top` offset over spacer
  divs), so a folder with tens of thousands of entries scrolls smoothly with a
  small DOM. This replaces v0.1.3's 500-row cap — the tree is now fully
  functional all the way down to the file level.
- **Keyboard shortcuts:** <kbd>F2</kbd> rename, <kbd>Delete</kbd> recycle,
  <kbd>Enter</kbd> drill-into-folder / open-file, on the selected row.
- **`treelens --selftest`** — a built-in smoke test that exercises the
  create → rename → recycle pipeline against a temp scratch dir and exits 0/1.
  Used to verify the destructive ops (including under elevation) without
  driving the WebView.

### Verified
- **Regular mode:** scanned an 800-file folder — virtualized list renders and
  scrolls smoothly; New folder/file buttons present + enabled; admin banner
  shown (correct, non-elevated). `--selftest`: create_folder / create_file /
  rename / recycle / clobber-guard all PASS.
- **Admin mode:** relaunched elevated — admin banner correctly hidden
  (`is_elevated()` true), scan + treemap + list render; elevated `--selftest`
  all PASS, confirming destructive ops route to the same user's Recycle Bin
  under elevation.

[0.2.0]: https://github.com/skyflyt/treelens/releases/tag/v0.2.0

## [0.1.3] — 2026-06-13

### Fixed
- **App froze on second drill.** Every IPC command (`treemap_layout`, `list_dir`,
  `top_n`, `breadcrumb`) was grabbing a single `Mutex<Tree>`, so the 4
  "parallel" calls on every drill actually serialized behind each other —
  back-to-back drills compounded into multi-second hangs that tripped
  Windows's "Not Responding" treatment. Switched `state.tree` to a
  `parking_lot::RwLock` so reads run concurrently on the Tauri worker pool.
- **Treemap layout could return tens of thousands of rects** on a pathological
  subtree (deep nesting + tiny rects rounding up); the JSON serialization
  alone could stall the WebView. Added a hard `MAX_RECTS = 8192` defensive
  cap that bails out of the squarify recursion once hit.
- **Stale renders from a previous drill could land after a new drill,
  overwriting the new view with the old data.** Every drill now bumps a
  `drillSeq` counter; every IPC result checks the captured seq before
  applying to the DOM and discards if superseded.
- **Version stamp drifted across releases** because it was hardcoded in
  `index.html` (v0.1.1 EXE displayed "v0.1.0", v0.1.2 stayed "v0.1.0").
  Now injected at build time from `package.json` via a Vite `define`, so
  every build reads one source of truth.

### Added
- **Loading indicator in the status bar** during drill (spinner +
  "Loading <folder>…"). Drill errors surface as `"Drill failed: <msg>"`
  instead of silently freezing.
- **Per-IPC-command timing logs** (`TimedSpan`) for debug builds and any
  release-build call that exceeds 200 ms, so the next slow path is
  observable instead of guessed at.
- **Cloud-placeholder detection** for OneDrive Files-On-Demand files
  (`FILE_ATTRIBUTE_OFFLINE` / `RECALL_ON_DATA_ACCESS` / `RECALL_ON_OPEN`) —
  reports `allocated = 0` for files whose bytes are not on local disk.
- **Inline-expand depth tracking is preserved.** Drilling now also resets
  the expanded set so chevron state from a different parent doesn't leak
  into the new view.

### Known limitations
- The side panel still renders up to 500 rows at once (capped, with a
  "… N more rows hidden" note). A genuine windowed virtual scroller is
  the headline of **v0.1.4** — it kept getting in the way of shipping the
  freeze fix, so it's split out.

[0.1.3]: https://github.com/skyflyt/treelens/releases/tag/v0.1.3

## [0.1.2] — 2026-06-13

### Fixed
- **Folder rows in the side panel didn't drill in.** Single-clicking a row only
  highlighted it; the only way to navigate down was a double-click that wasn't
  discoverable from the affordance. Now a single click on a folder row drills
  into that folder (replaces the view, updates the breadcrumb, reframes the
  treemap), matching what the `▶` chevron + cursor:pointer promised.
- **The chevron itself was decorative.** It had no click handler at all.
  Wired it: the chevron toggles **inline expansion** without leaving the
  current view — children of the expanded folder appear indented beneath it,
  TreeSize-style. `▶` rotates to `▼` when open. The chevron is now a real
  click target with its own hover affordance, separate from the row body.

### Behavior
- **Two distinct interactions on one row, by design.**
  - Click the **chevron** → toggle inline expand (stay where you are).
  - Click the **row body** → drill into the folder (replace the view).
  - Double-click → drill (kept for muscle memory); on files, opens Explorer.
- Backspace still drills up; the breadcrumb segments are still buttons; the
  treemap reframes on drill in either direction. The expanded set resets on
  drill so you don't carry stale idxs from a different parent.

[0.1.2]: https://github.com/skyflyt/treelens/releases/tag/v0.1.2

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
