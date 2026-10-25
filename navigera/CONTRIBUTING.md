# Contributing to navigera

Bug reports and pull requests are welcome. For anything larger than a fix,
open an issue first so we can agree on the approach.

## Build and test

```sh
cargo build
cargo test                                   # unit + browser e2e tests
cargo clippy --all-targets -- -D warnings    # lint gate
cargo fmt --check
```

The e2e tests drive a real Chrome. navigera finds `$CHROME_BIN`, a system
Chrome/Chromium/Edge, or the Playwright and Puppeteer browser caches; without
any of them the browser tests are skipped (CI sets `NAVIGERA_REQUIRE_BROWSER=1`
so they can't be skipped there). The Acme Supply scenario in
`tests/site_scenarios.rs` takes about 10 s.

For manual testing, run the local shop and drive it with a session:

```sh
python3 bench/site/server.py --port 8765
navigera -s dev start
navigera -s dev goto localhost:8765
navigera -s dev ax
```

### Offline builds

`vendor.yml` publishes the vendored dependencies to the git ref
`refs/cache/vendor` whenever `Cargo.lock` changes, for machines that can reach
GitHub but not crates.io:

```sh
git fetch origin refs/cache/vendor:refs/cache/vendor
mkdir -p ../nv-vendor && git archive refs/cache/vendor | tar -x -C ../nv-vendor
mkdir -p ~/.cargo && printf '[source.crates-io]\nreplace-with = "v"\n[source.v]\ndirectory = "%s"\n' \
    "$(cd ../nv-vendor && pwd)/vendor" >> ~/.cargo/config.toml
cargo test --offline
```

## CI

| workflow | runs | what |
|---|---|---|
| `ci.yml` | push, PR, weekly | tests, lint, Windows, install scripts, Chrome Stable + three milestones back, chrome-headless-shell, Beta |
| `agent-check.yml` | push, PR, nightly | natural purchase checks with a real agent on Linux and Windows |
| `agent-eval.yml` | manual, weekly | Gemini CLI doing the Acme tasks with navigera, playwright-cli and agent-browser |
| `bench.yml` | manual | the driver ladder: navigera against Playwright, Puppeteer, go-rod, chromedp, chromiumoxide |
| `chrome-bisect.yml` | `.github/bisect.env` on a branch | one Chrome milestone under several flag/transport variants |
| `release.yml` | manual, `v*` tags | binaries, GitHub release, registries |

Every workflow reports what you need to act on as check annotations (failing
test output, the CDP methods each Chrome build accepted, bench tables, eval
results), so the checks API is enough to read a run:

```sh
gh api repos/andrey-usa/navigera/check-runs/<job-id>/annotations -q '.[] | "[\(.title)] \(.message)"'
```

Before using a new CDP method or parameter, check that the oldest supported
milestone has it: `bash tools/protocol_dump.sh <chrome> protocol.json`.

### Windows

Windows differs in three places: CDP runs over a WebSocket (no pipe), sessions
use loopback TCP plus a token file (`src/session.rs`, `endpoint`), and Chrome
runs in a kill-on-close job object (`src/cdp/procjob.rs`). The `windows` job in
`ci.yml` builds, lints and runs every test on the runner's Chrome.

## Performance changes

Runners differ between runs by more than most changes are worth, so judge a
change with an A/B inside one run: `baseline_ref` builds a second navigera from
any ref and runs it beside your build.

```sh
gh workflow run bench.yml --ref my-branch -f reps=3 \
    -f only=nv-serve,nv-baseline,gorod -f scenarios=eval,cold -f baseline_ref=master
```

Measurement rules the ladder follows:

- Driver CPU and memory are the driver's own (`utime+stime` from the zombie's
  `/proc/<pid>/stat`, VmHWM), never `wait4` totals, which include every child
  the driver reaped.
- Browser numbers cover the whole process tree as PSS.
- Every number in the README cites the run it came from; regenerate the table
  from that run's `publish.json` with `bench/ladder/gen_readme.py`.

## Keeping docs in step

When ops or flags change, update `skills/navigera/SKILL.md` (compiled into the
binary as `navigera skill`), `docs/navigera.md`, `llms.txt` and the `OPS` table
in `src/protocol.rs` together.

## Releasing

1. Bump `version` in `Cargo.toml` and add a `CHANGELOG.md` entry on `master`.
2. Actions → release → Run workflow with that version. The run checks the
   version, builds every target, tags `v<version>`, creates the GitHub release
   and publishes to each registry that is switched on (see below).

Registries are switched on per repository with Actions variables, once the
account and trusted publisher exist:

| variable | publishes to | trusted publisher to configure |
|---|---|---|
| `PUBLISH_CRATES=true` | crates.io | crate `navigera`, workflow `release.yml` |
| `PUBLISH_NPM=true` | npm (`navigera` + one package per platform) | each package, workflow `release.yml` |
| `PUBLISH_PYPI=true` | PyPI (platform wheels) | project `navigera`, workflow `release.yml`, environment `release` |
| `HOMEBREW_TAP=owner/homebrew-tap` | Homebrew formula | secret `TAP_TOKEN` with write access to the tap |
| `SCOOP_BUCKET=owner/scoop-bucket` | Scoop manifest | the same `TAP_TOKEN` |

Re-running the workflow for a version that already exists rebuilds its files
and publishes it to any registry that doesn't have it yet.
