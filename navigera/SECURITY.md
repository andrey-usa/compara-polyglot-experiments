# Security policy

Please report vulnerabilities privately through
[GitHub security advisories](https://github.com/andrey-usa/navigera/security/advisories/new)
rather than a public issue. Only the latest release receives fixes.

## What navigera protects, and how

navigera runs a real browser for an agent that opens pages nobody vetted.

| boundary | protection |
|---|---|
| a malicious page vs. your account | Chrome's renderer sandbox stays on. navigera adds `--no-sandbox` only when Chrome can't have one: as root (containers, CI), with `NAVIGERA_NO_SANDBOX=1`, or after Chrome reports at launch that the machine has no usable sandbox, in which case it says so on stderr. It remembers that for that browser binary in the private session directory, until the binary changes or the machine reboots, so later launches skip the attempt; deleting `no-sandbox-*` there brings it back |
| other local users vs. your session | Unix: the session socket and log live in `$XDG_RUNTIME_DIR/navigera` or `$TMPDIR/navigera-<uid>`, mode 0700, refused if it is a symlink, someone else's or open to others; the log is 0600 and never follows a symlink. Windows: a loopback port that drops any client whose first line isn't the 128-bit token from your own `%TEMP%` |
| a web page vs. the CDP connection | the default transport is a private pipe (Linux/macOS): no port at all. Chrome rejects DevTools WebSocket connections from web origins |
| the downloaded binary | `install.sh` / `install.ps1` verify the SHA-256 published with each release; release builds carry GitHub build-provenance attestations (`gh attestation verify <file> -R andrey-usa/navigera`) |
| the MCP server's file access | `browser_file_upload` takes files inside the working directory only, and `file:` URLs are refused, unless `NAVIGERA_MCP_ALLOW_FILES=1` |
| page content vs. third parties | nothing leaves the machine but the browser's own traffic, unless you configure a decision model (`TYPESAFE_API_KEY` / `NAVIGERA_DECIDER_URL`). Then `find` and `check` send the page's accessibility tree to it, with text typed into fields replaced by `(filled)`; the key goes to `curl` on stdin, not in its arguments |

## Known exposures

- **DevTools port.** `--transport ws`, Windows sessions and the persistent
  `--profile` browser listen on a loopback DevTools port. Chrome has no
  authentication there: any process on the machine that finds the port can
  drive that browser. For a `--profile` browser (your sign-ins), that lasts as
  long as the browser stays open. On a shared machine, close it with
  `quit --close-browser` when you're done.
- **`eval` runs any JavaScript** in the page, and `upload` (CLI) sends any
  file you name: by design, for the agent you run.
- **Prompt injection.** Page text reaches the model. The skill and the MCP
  server tell the agent to treat it as data; the agent and its approval
  settings decide what it does with it.
