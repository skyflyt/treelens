# Treelens

**A free, open-source, portable disk-space visualizer for Windows.**

> 🚧 **Status: planning phase.** No application code yet — the full technical plan lives in
> [PLAN.md](PLAN.md). Implementation starts once the open questions at the bottom of that
> document are settled.

## Why

TreeSize Free answered "*what is eating my disk?*" for years — then it went behind a
paywall. The free alternatives each miss: WinDirStat has the beloved treemap but
2003-era single-threaded scanning; WizTree is blazing fast but closed-source and
free-for-personal-use-only; most everything else is abandoned.

Treelens aims to be all three at once:

- **Fast like WizTree** — reads the NTFS Master File Table directly, so a multi-terabyte
  volume with millions of files scans in seconds, not minutes.
- **Visual like WinDirStat** — a modern, GPU-smooth squarified treemap as the centerpiece,
  with a TreeSize-style sortable directory list beside it.
- **Free forever, open source** — MIT licensed, built in the open.

## What it will be

- A **portable single `.exe`** — download, double-click, scan. No installer needed.
  (An optional installer will exist for Start-menu convenience.)
- **Runs as administrator** by design: that's what unlocks MFT-speed scans and full
  visibility into protected folders. It makes no system changes without you — the only
  write operation in v1 is *delete to Recycle Bin*, with confirmation, always undo-able.
- Treemap + directory list + top-N largest files panel, drill-in navigation, breadcrumbs,
  light/dark theme, scan progress, open-in-Explorer.
- Planned stack: **Tauri 2** — Rust scanning core, TypeScript/canvas UI. Full rationale
  and comparison in [PLAN.md §2](PLAN.md#2-tech-stack-recommendation).

## A note on Windows warnings (future releases)

Release binaries will initially be **unsigned** (no Authenticode certificate yet), so
SmartScreen may show "Windows protected your PC" and the UAC prompt will say "Publisher:
Unknown." The source is right here, releases will ship SHA-256 checksums, and signing is
on the roadmap if the project earns adoption.

## License

[MIT](LICENSE) © 2026 Skylar Pearce.

## No secrets — ever

This repository is public and **contains no credentials, API keys, or tokens, and never
will**. Treelens is a fully client-side desktop app: there is no server component, no
telemetry, and nothing a secret could legitimately do here. This is a strict, permanent
rule for this repo — enforced in [AGENTS.md](AGENTS.md), defensively in
[.gitignore](.gitignore), and (once CI exists) by secret scanning on every PR.
