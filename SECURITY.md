# Security Policy

## Reporting a vulnerability

Please report security issues privately via GitHub's **"Report a vulnerability"**
(Security → Advisories) on this repository, or by opening a minimal private
report rather than a public issue. Include:

- what the issue is and the impact you see,
- steps to reproduce (a sample file or path is ideal — but **never** attach
  anything containing real secrets or personal data),
- the Treelens version (shown in the status bar) and your Windows version.

You'll get an acknowledgement as soon as possible. Since Treelens is a small
open-source project there's no formal SLA, but security reports are triaged
ahead of feature work.

## Scope & threat model

Treelens is a **fully local, client-side** Windows application:

- no network access, no telemetry, no analytics, no auto-update calls;
- it reads the filesystem you point it at and only writes outside its own
  config when you explicitly create, rename, recycle, or delete something;
- the steganography tools operate only on local files you select.

Because there is no server and no network surface, the relevant risks are
local: incorrect destructive operations (delete/recycle), path handling, and
parsing untrusted file contents (images for stego/LSB analysis, archives, etc.).
Reports in those areas are especially welcome.

## No secrets, ever

This repository must never contain secrets — no credentials, API keys, tokens,
connection strings, or certificates, in code, config, CI, or commit history. CI
runs a secret scan (gitleaks) on every push. Treelens needs no secrets to build
or run; if a change appears to require one, the design is wrong — stop and
redesign.

## Verifying downloads

Release builds are **not yet code-signed**, so Windows SmartScreen will warn and
the UAC prompt will show an unknown publisher. Every release ships a
`SHA256SUMS.txt`; verify your download against it:

```powershell
Get-FileHash .\Treelens-<version>-portable.exe -Algorithm SHA256
```
