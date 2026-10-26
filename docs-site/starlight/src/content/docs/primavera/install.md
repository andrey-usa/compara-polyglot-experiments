---
title: Install primavera
description: "Every way to install primavera, from the README. Pre-release: no package-manager release yet."
---

# Install

Primavera is pre-release. There is no npm, pip, Cargo or Homebrew
release yet; the ways to get it are below.

## Build from source

Primavera links navigera as a library, checked out beside it:

```text
work/
  navigera/     github.com/andrey-usa/navigera
  primavera/    this repository
```

```console
$ cd primavera && cargo build --release
$ ./target/release/primavera --version
```

Pin the navigera checkout to `session-history`: that is what
primavera's own CI builds against (`NAVIGERA_REF`, default
`session-history`, in its `ci.yml`). Navigera `master` does not
currently compile against primavera `next` — at primavera `next`
`fa3fe34` against navigera `master` `ba37ede` the build fails with 18
errors (missing `cli` module, `in_working_dir`, `locate_js`, private
MCP items, and others). Until the repos re-align, treat
`session-history` as the pairing the docs promise, and `master` as a
known docs gap (proposed README fix: name the navigera ref in the
Building section).

## GitHub Action

```yaml
- uses: andrey-usa/primavera@next
  with:
    paths: tests
    base-url: http://127.0.0.1:3000
    args: --workers auto --retries 1 --junit report.xml
```

It runs on the runner's Chrome (Linux, macOS, Windows). `version`
picks a release (`latest` by default, or a tag); with no release binary
for the runner it builds primavera from the action's own commit
(`source` asks for that), with navigera at `navigera-ref`. Failures are
annotations on the lines that failed; the `evidence` output is the
directory holding their screenshots.

## Container

`Dockerfile` builds an image with primavera and Debian's headless
Chromium, amd64 or arm64, from the directory holding both checkouts
(see Building above):

```console
$ docker build -f primavera/Dockerfile -t primavera .
$ docker run --rm --network host -v "$PWD/tests:/tests:ro" primavera run /tests --base-url http://127.0.0.1:3000
```

It runs as an unprivileged user, with a read-only root filesystem if
asked, and without Chrome's own sandbox: a container without user
namespaces can't give Chrome one, and the container is the boundary.
