---
title: Quick start navigera
description: "One copy-paste path from install to a driven browser session. Run in CI on every push."
---

# Quick start

One path, start to finish. It installs navigera, opens a headless
session, loads a page, reads it, and closes the session. This exact
sequence is run in CI on every push to the site branch.

```sh
npm install -g navigera
navigera -s qs start --headless
navigera -s qs goto https://example.com
navigera -s qs ax --raw
navigera -s qs quit
```

What you should see: `ax --raw` prints the page as an indented tree,
starting with a line like `page "Example Domain" https://example.com/`
and a `heading "Example Domain"` node. If Chrome is not found, set
`$CHROME_BIN` to your Chrome or Chromium binary and run the same
commands again.

Next: the [agent skill](/navigera/skill/) is the version-matched guide
to hand an agent, and [Reference](/navigera/reference/) lists every
command.
