#!/usr/bin/env python3
"""One markdown table renderer, so a table reads the same in all three places.

A benchmark table here is looked at in three places and only one of them renders
markdown: the step summary does, the job log does not, and the `.txt` or `.md`
downloaded from the run's artifacts does not either. An unpadded table -- which
is what every script here used to print -- is fine in the first and a wall of
pipes in the other two.

Padding the cells costs nothing in a renderer, because GitHub sizes table
columns from the content rather than from the source. So one padded table is
legible everywhere, and that is the whole idea.

`aligns` is one of "l" or "r" per column and sets both the markdown marker and
the side the padding goes on, so the two cannot disagree.
"""
from __future__ import annotations


def table(headers: list[str], aligns: list[str], rows: list[list[str]]) -> list[str]:
    cols = range(len(headers))
    width = [max([len(headers[i])] + [len(r[i]) for r in rows]) for i in cols]

    def fmt(cells: list[str]) -> str:
        return "| " + " | ".join(
            cells[i].rjust(width[i]) if aligns[i] == "r" else cells[i].ljust(width[i])
            for i in cols) + " |"

    rule = ["-" * (width[i] - 1) + ":" if aligns[i] == "r" else "-" * width[i] for i in cols]
    return [fmt(headers), "| " + " | ".join(rule) + " |"] + [fmt(r) for r in rows]


def render(headers: list[str], aligns: list[str], rows: list[list[str]]) -> str:
    """The same, as one string with a trailing newline."""
    return "\n".join(table(headers, aligns, rows)) + "\n"
