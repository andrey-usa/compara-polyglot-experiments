---
title: Quick start compara
description: "One copy-paste path from install to a first comparison. Run in CI on every push."
---

# Quick start

One path, start to finish. It installs compara, compares two tiny tables
that agree, then changes one value and compares again. This exact
sequence is run in CI on every push to the site branch.

```sh
npm install -g compara
printf 'id,amount\n1,10\n2,20\n' > a.csv
printf 'id,amount\n1,10\n2,20\n' > b.csv
compara compare a.csv b.csv -k id
```

That prints identical counts and exits 0. Then:

```sh
printf 'id,amount\n1,10\n2,25\n' > b.csv
compara compare a.csv b.csv -k id -o report.html
```

That prints the differences, writes `report.html` (counts, per-column
statistics, and the changed rows), and exits 1 — differences, not an
error. Open `report.html` in a browser; it needs nothing else to open.

The same shape works on real files: `compara compare export.csv
warehouse.parquet -k id`, and the repo's own example is
`compara compare examples/orders_2026-08.csv
examples/orders_2026-09.csv -k order_id,line_no -i updated_at -o
examples/report.html` from a checkout of compara. Exit codes are made
for pipelines: 0 identical, 1 differences, 2 error, 3 duplicate keys
(with `--fail-on-dups`).
