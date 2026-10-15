#!/usr/bin/env python3
"""Check `--json` documents against the result contract.

    scripts/contract.py doc.json [doc.json ...]
    scripts/contract.py --self-test

Two layers, because the contract has two kinds of rule:

* Shape, from `docs/contract.schema.json`. Validated here with a small
  stdlib-only subset of JSON Schema (type, required, properties,
  additionalProperties, items, minimum) so that CI and a fresh clone need
  nothing installed. The schema file is ordinary JSON Schema and any full
  validator will read it too.
* Arithmetic, which a schema cannot say. Every port must satisfy these on any
  input; a port that breaks one has miscounted, whatever the other ports say:

      matched                    = unchanged + changed
      removed                    = a_keys - matched
      added                      = b_keys - matched
      a_rows - a_keys            = a_dup_rows - a_dup_keys      (likewise for b)
      each column's changed     <= counts.changed
      each column's blanked + filled <= that column's changed

Exit status: 0 every document conforms, 1 at least one does not, 2 usage.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

SCHEMA = Path(__file__).resolve().parent.parent / "docs" / "contract.schema.json"


def _type_ok(value, want: str) -> bool:
    if want == "object":
        return isinstance(value, dict)
    if want == "array":
        return isinstance(value, list)
    if want == "string":
        return isinstance(value, str)
    if want == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if want == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if want == "boolean":
        return isinstance(value, bool)
    return True


def validate(value, schema: dict, path: str = "$") -> list[str]:
    """Errors for `value` against the supported subset of `schema`."""
    errors: list[str] = []
    want = schema.get("type")
    if want and not _type_ok(value, want):
        return [f"{path}: expected {want}, got {type(value).__name__}"]
    if isinstance(value, dict):
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{path}: missing required key {key!r}")
        props = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            for key in value:
                if key not in props:
                    errors.append(f"{path}: unexpected key {key!r}")
        for key, sub in props.items():
            if key in value:
                errors += validate(value[key], sub, f"{path}.{key}")
    elif isinstance(value, list) and "items" in schema:
        for i, item in enumerate(value):
            errors += validate(item, schema["items"], f"{path}[{i}]")
    elif isinstance(value, (int, float)) and "minimum" in schema:
        if value < schema["minimum"]:
            errors.append(f"{path}: {value} is below the minimum {schema['minimum']}")
    return errors


def arithmetic(doc: dict) -> list[str]:
    """The identities the schema cannot express. Assumes the shape is valid."""
    c = doc["counts"]
    errors = []

    def need(ok: bool, message: str) -> None:
        if not ok:
            errors.append(message)

    need(c["matched"] == c["unchanged"] + c["changed"],
         f"matched {c['matched']} != unchanged {c['unchanged']} + changed {c['changed']}")
    need(c["removed"] == c["a_keys"] - c["matched"],
         f"removed {c['removed']} != a_keys {c['a_keys']} - matched {c['matched']}")
    need(c["added"] == c["b_keys"] - c["matched"],
         f"added {c['added']} != b_keys {c['b_keys']} - matched {c['matched']}")
    for side in ("a", "b"):
        extra = c[f"{side}_rows"] - c[f"{side}_keys"]
        dups = c[f"{side}_dup_rows"] - c[f"{side}_dup_keys"]
        need(extra == dups,
             f"{side}_rows - {side}_keys = {extra}, but {side}_dup_rows - {side}_dup_keys = {dups}")
    for col in doc["columns"]:
        need(col["changed"] <= c["changed"],
             f"column {col['name']!r}: changed {col['changed']} exceeds counts.changed {c['changed']}")
        need(col["blanked"] + col["filled"] <= col["changed"],
             f"column {col['name']!r}: blanked + filled exceeds its changed {col['changed']}")
    return errors


def check(doc, schema: dict | None = None) -> list[str]:
    """Every problem with `doc`; empty means it conforms."""
    schema = schema if schema is not None else json.loads(SCHEMA.read_text())
    errors = validate(doc, schema)
    return errors if errors else arithmetic(doc)


def _self_test() -> int:
    good = {
        "counts": {"a_rows": 5, "b_rows": 5, "a_keys": 5, "b_keys": 4, "matched": 3,
                   "unchanged": 2, "changed": 1, "added": 1, "removed": 2,
                   "a_dup_keys": 0, "a_dup_rows": 0, "b_dup_keys": 1, "b_dup_rows": 2},
        "columns": [{"name": "x", "changed": 1, "blanked": 0, "filled": 1}],
        "meta": {"anything": True},
    }
    cases = []

    def mutate(label: str, edit, expect_bad: bool = True) -> None:
        doc = json.loads(json.dumps(good))
        edit(doc)
        cases.append((label, doc, expect_bad))

    mutate("the baseline document conforms", lambda d: None, expect_bad=False)
    mutate("an unknown key beside counts is fine", lambda d: d.update(samples=[]), expect_bad=False)
    mutate("missing counts", lambda d: d.pop("counts"))
    mutate("a count is a string", lambda d: d["counts"].update(matched="3"))
    mutate("a count is a bool", lambda d: d["counts"].update(added=True))
    mutate("a count is negative", lambda d: d["counts"].update(added=-1))
    mutate("an extra key inside counts", lambda d: d["counts"].update(surprise=1))
    mutate("a missing count", lambda d: d["counts"].pop("b_dup_rows"))
    mutate("a column lacks a field", lambda d: d["columns"][0].pop("filled"))
    mutate("matched disagrees with unchanged + changed", lambda d: d["counts"].update(unchanged=9))
    mutate("removed disagrees with a_keys - matched", lambda d: d["counts"].update(removed=1))
    mutate("added disagrees with b_keys - matched", lambda d: d["counts"].update(added=2))
    mutate("dup accounting is off", lambda d: d["counts"].update(b_dup_rows=5))
    mutate("a column changed more rows than exist", lambda d: d["columns"][0].update(changed=2))
    mutate("blanked + filled exceeds changed", lambda d: d["columns"][0].update(blanked=1))

    schema = json.loads(SCHEMA.read_text())
    failed = 0
    for label, doc, expect_bad in cases:
        bad = bool(check(doc, schema))
        ok = bad == expect_bad
        failed += not ok
        print(f"  {'ok  ' if ok else 'FAIL'} {label}")
    print(f"contract self-test: {len(cases) - failed}/{len(cases)}")
    return 1 if failed else 0


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return _self_test()
    if not argv or argv[0].startswith("-"):
        print(__doc__, file=sys.stderr)
        return 2
    schema = json.loads(SCHEMA.read_text())
    bad = 0
    for name in argv:
        try:
            doc = json.loads(Path(name).read_text())
        except (OSError, ValueError) as err:
            print(f"::error::{name}: unreadable: {err}")
            bad += 1
            continue
        errors = check(doc, schema)
        for e in errors:
            print(f"::error::{name}: {e}")
        bad += bool(errors)
        if not errors:
            print(f"ok  {name}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
