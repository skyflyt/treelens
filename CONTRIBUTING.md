# Contributing to Treelens

Thanks for your interest! Treelens is a free, open-source, **Windows-only**
disk-space visualizer built with Tauri 2 (Rust core + TypeScript/canvas UI).

## Prerequisites

- Windows 10/11
- Rust (stable, MSVC toolchain) — `rustup toolchain install stable`
- Node.js 22+
- The Tauri 2 prerequisites (WebView2 is preinstalled on current Windows)

## Project layout

```
crates/
  scanner/   parallel filesystem walk
  tree/      arena tree, aggregation, treemap layout, queries, search
  fileops/   create/rename/recycle/permanent-delete, junk finder
  analysis/  checksums, file compare, duplicate finder, steganography
src-tauri/   Tauri shell: IPC commands, scan state, events, --selftest
ui/          TypeScript + canvas frontend (Vite)
```

Architectural rule (see `PLAN.md`): **the tree never crosses the IPC boundary.**
Commands answer narrow questions in bounded payloads; the frontend re-queries
after a drill-in, sort, or size-mode change. Every per-tab command takes a
`tab` id.

## Develop

```bash
npm install
npm run tauri dev      # run the app with hot reload
```

## Before you open a PR

Run the same checks CI runs:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude treelens   # library crates
npm run typecheck && npm run build
npm run build && cargo test -p treelens      # Tauri crate (needs ui/dist)
cargo run -p treelens -- --selftest          # destructive-op smoke test
```

`--selftest` exercises create/rename/recycle/permanent-delete plus checksum,
compare, and the three steganography round-trips against a temp scratch dir —
it's how we validate the destructive paths without driving WebView2 (synthesized
input doesn't reach WebView JS handlers).

## Conventions

- Conventional-commit-style messages (`feat:`, `fix:`, `sprint N:` …).
- Keep `package.json`, `src-tauri/tauri.conf.json`, and the `Cargo.toml`
  `[workspace.package]` version **in sync** — CI fails if they drift. Release
  versions are derived from the `v*` tag.
- Add tests with behavior changes; prefer a failing test first.
- **No secrets, ever** — see `SECURITY.md`. CI runs gitleaks on every push.

## Releasing (maintainers)

Bump the three versions + `CHANGELOG.md`, then push a `vX.Y.Z` tag. The release
workflow syncs the tag into all version fields, builds the portable EXE + NSIS
installer, writes `SHA256SUMS.txt`, and publishes a GitHub Release.
