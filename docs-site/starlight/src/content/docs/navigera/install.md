---
title: Install navigera
description: "Every way to install navigera, from the README."
---

# Install

Every channel installs the same prebuilt binary. Pick one.

## Installer script

```sh
curl -fsSL https://raw.githubusercontent.com/andrey-usa/navigera/master/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/andrey-usa/navigera/master/install.ps1 | iex
```

## Package managers

| Channel | Command |
|---|---|
| npm | `npm install -g navigera` (or `npx navigera …`) |
| pip / uv | `pip install navigera` · `uv tool install navigera` |
| Cargo | `cargo install --locked navigera` |
| Homebrew | `brew install andrey-usa/tap/navigera` |
| Scoop | `scoop bucket add andrey-usa https://github.com/andrey-usa/scoop-bucket` then `scoop install navigera` |

## After installing

Give your agent the usage guide:

```sh
navigera install-skill        # -> ./.agents/skills (Gemini CLI, Codex, Cline, Kilo, …)
                              #    --claude -> ./.claude/skills, --continue -> ./.continue/skills
```

## Browser

navigera finds Chrome or Chromium on its own: a system install,
`$CHROME_BIN`, or a Playwright/Puppeteer browser cache. For the fastest
cold start, point it at chrome-headless-shell:

```sh
npx @puppeteer/browsers install chrome-headless-shell@stable
```

Linux, macOS and Windows are supported.
