# Treelens

**A free, open-source, portable disk-space visualizer for Windows.**

> ![Treelens v0.1](assets/screenshot-scanned.png)
>
> The spiritual successor to TreeSize Free — fast scan, modern treemap, dark/light theming, recycle-bin delete, MIT-licensed, no telemetry, no installer required.

## Download

Grab the latest portable EXE from [Releases](https://github.com/skyflyt/treelens/releases). Double-click — no installer needed.

| Asset | Description |
| --- | --- |
| `Treelens-x.y.z-portable.exe` | Single-file portable build. Drop on a USB stick and go. |
| `Treelens-x.y.z-setup.exe` | NSIS installer with Start-menu shortcut + uninstaller. |
| `SHA256SUMS.txt` | Verify your download. |

## What v0.1 ships

- **Scan** any folder or drive — parallel directory walk, works without admin and on any filesystem (NTFS / exFAT / ReFS / network shares).
- **Squarified treemap** with file-type coloring, depth-tinted directory headers, hover + click + drill-in.
- **Age-heat mode** — overlay modification age on the treemap (warm = recently changed, cold = old). One-click toggle.
- **Side panel** with three views: folder contents, top-50 files, top-50 folders. Sortable. Inline % bars, modified date, file count.
- **Breadcrumb** drill-up; <kbd>Backspace</kbd> as a keyboard shortcut.
- **Size mode toggle** — "size on disk" (allocated) by default, with a one-click flip to logical bytes.
- **Power-user file ops:**
  - Recycle-bin delete (via Shell `IFileOperation`, undo-able from Explorer's Recycle Bin).
  - "Open in Explorer" / "Open in Terminal" / "Copy full path".
  - "Find files older than 1 year (≥10 MB)" and "Find empty folders" under any subtree.
- **Admin banner** — runs degraded without admin (and quietly nags you to relaunch elevated for full visibility).
- **Light + dark theme** following OS by default; manual override is persisted.

## Why

TreeSize Free answered _"what is eating my disk?"_ for years — then it went behind a paywall. The free alternatives each miss:

- **WinDirStat** has the beloved treemap, but a 2003-era single-threaded scanner.
- **WizTree** is blazing fast (reads the NTFS MFT directly), but closed-source, free-for-personal-use-only, and the UI is functional rather than pleasant.
- Most other tools (SpaceSniffer, SequoiaView) are abandoned.

Treelens aims to be all three at once: fast like WizTree, visual like WinDirStat, organized like TreeSize, free forever, open source.

## Roadmap

| Tier | Feature |
| --- | --- |
| v0.2 | NTFS MFT fast path (FSCTL_ENUM_USN_DATA) for the 10× scan speedup |
| v0.2 | Truly portable `treelens.config.json` next to the exe |
| v0.2 | Long-path manifest + `requireAdministrator` opt-in |
| v0.x | Search / filter, CSV/JSON export, exclude patterns, multi-drive overview |
| v1.x | Scan history / snapshots, "what changed since last scan" diff |
| v2.x | Duplicate finder (size-prefilter → hash) |

Full plan: [PLAN.md](PLAN.md).

## A note on Windows warnings

Release binaries are currently **unsigned** (no Authenticode certificate yet), so SmartScreen may show "Windows protected your PC" and the UAC prompt will say "Publisher: Unknown."
Click _More info → Run anyway._ Source is right here; `SHA256SUMS.txt` lets you verify the bits.

## Build from source

```powershell
# Prerequisites: Rust (stable, MSVC toolchain) + Node 20+ + Windows 10/11 with WebView2
git clone https://github.com/skyflyt/treelens.git
cd treelens
npm install
npx tauri build
```

The portable EXE lands at `target/release/treelens.exe`.

## License

[MIT](LICENSE) © 2026 Skylar Pearce.

## No secrets — ever

This repository is public and **contains no credentials, API keys, or tokens, and never will**. Treelens is a fully client-side desktop app: there is no server component, no telemetry, and nothing a secret could legitimately do here. This is a strict, permanent rule for this repo — enforced in [AGENTS.md](AGENTS.md), defensively in [.gitignore](.gitignore), and by `gitleaks` in CI.
