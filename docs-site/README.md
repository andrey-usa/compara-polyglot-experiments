# Docs site generator comparison

The same four navigera docs pages (README, the `navigera` reference,
the multi-model guide, the agent skill) built with four static site
generators, defaults plus a sidebar, search, dark mode, and code copy
buttons:

- `starlight/` — Astro + Starlight (Node toolchain, Pagefind search)
- `mkdocs/` — MkDocs Material, pinned to `mkdocs<2` + material 9.7.1
  (Material is in maintenance mode; see the comparison report for the
  Zensical succession question)
- `mdbook/` — mdBook 0.5.2 release binary (Rust toolchain, single file)
- `zensical/` — Zensical, the Material team's next-generation generator
  (Python toolchain, Material-compatible config)

## Layout

- `content/` — the four source pages, verbatim from navigera master.
- `sync-content.py` — copies `content/` into each generator's source
  layout (only frontmatter/layout adaptation per generator). Run from
  this directory before building, or rely on the workflow, which runs it.
- `check-links.py` — internal link checker. Run as
  `python3 check-links.py <site-dir> --allow link-baseline.txt`.
  `link-baseline.txt` lists the source-repo links that point outside
  this four-page extract; those are identical across generators and are
  not generator defects. Any other break fails the check.
- `mkdocs/site/`, `mdbook/book/`, `zensical/site/`, `starlight/dist/`
  are build outputs and are not committed.

## Build locally

```sh
python3 sync-content.py
npm --prefix starlight ci && npm --prefix starlight run build
pip install "mkdocs<2" "mkdocs-material==9.7.1" pymdown-extensions "zensical==0.0.72"
mkdocs build -f mkdocs/mkdocs.yml
mdbook build mdbook
(cd zensical && zensical build)
```

## CI

`.github/workflows/docs-site.yml` (workflow_dispatch) builds all four,
runs the link checker, and uploads each built site as a CI artifact.
It does not publish anywhere: no Pages deploy, no release, no tag.
