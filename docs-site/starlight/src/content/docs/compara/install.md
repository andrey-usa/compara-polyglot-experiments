---
title: Install compara
description: "Every way to install compara, from the README."
---

# Install

Prebuilt binaries for Linux (x86_64, aarch64), macOS (arm64, x86_64)
and Windows (x64) are on the
[releases page](https://github.com/andrey-usa/compara/releases), and
the same binaries through package managers:

| Channel | Command |
|---|---|
| Cargo | `cargo install --locked compara` |
| npm | `npm install -g compara` (or `npx compara …`) |
| pip / uv | `pip install compara` · `uv tool install compara` |
| Homebrew | `brew install andrey-usa/tap/compara` |
| Scoop | `scoop bucket add andrey-usa https://github.com/andrey-usa/scoop-bucket` then `scoop install compara` |
| Docker | `docker run --rm -v "$PWD:/data" ghcr.io/andrey-usa/compara compare a.csv b.csv -k id` |

One static binary; the report is a single HTML file with nothing to host.
