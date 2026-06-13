# Changelog

All notable changes to Treelens are documented here.
The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning is [SemVer](https://semver.org/) (0.x while pre-1.0).

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
- **No MFT fast path yet.** Scans use the parallel walk; the WizTree-class
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
