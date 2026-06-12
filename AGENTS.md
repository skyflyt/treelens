# Agent instructions — Treelens

## ⛔ STRICT PERMANENT RULE: NO SECRETS. EVER.

**No secrets ever in this repo or its history.** No credentials, API keys, tokens,
connection strings, certificates, or anything that must be kept private — not in code,
not in config, not in CI workflow files, not in commit messages, not in issues or PR
descriptions, not "temporarily."

This is a **public** repo and a portfolio piece for a fully client-side desktop app. It
has **no server, no external APIs, no telemetry — by design**. There is nothing
legitimate a secret could do here. `.gitignore` enforces this defensively (its secret
patterns should never match anything — that's the point).

**If a future agent believes it needs a secret to implement something here, the design
of that change is wrong. Stop and redesign.**

History is permanent on a public repo: if a secret ever lands, removal means rewriting
public history *and* rotating the credential. Don't let it happen.

## Project state

- **Phase: planning.** [PLAN.md](PLAN.md) is the spec; no application code exists yet.
  Read PLAN.md fully before implementing anything, and work the milestone order in
  PLAN.md §10. Update PLAN.md by PR when decisions change — never silently.

## Conventions (details in PLAN.md §8)

- Conventional Commits; trunk-based; short-lived branches; PRs gated by CI; squash-merge.
- Stack (post-approval): Tauri 2 — Rust workspace (`scanner`, `tree`, `fileops`,
  `src-tauri`) + TypeScript/Vite UI. Windows-only target; the exe runs elevated
  (`requireAdministrator` manifest).
- `main` is always buildable. No build artifacts committed.
