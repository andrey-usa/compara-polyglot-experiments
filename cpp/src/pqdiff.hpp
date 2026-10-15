// Comparing two Parquet files without turning them back into rows.
//
// The CSV and JSON paths in csvdiff.cpp share one shape: map the file, find
// every row, and reduce each row to a handful of (offset, length) fields. That
// shape is right for a text format, where a value's boundaries are only known
// by scanning for them.
//
// Parquet is not that. A value's boundaries are written down, values of one
// column are contiguous, and -- this is the part worth exploiting -- a
// low-cardinality column is stored as small integers indexing a dictionary of
// its distinct values. Reading such a file back into rows in order to compare
// them row by row throws away the one thing the format gives you.
//
// So this path is columnar end to end. It reads the key columns, joins on them
// once to produce a list of matched (a_row, b_row) pairs, and then walks the
// compared columns one at a time, releasing each before reading the next. Where
// both sides of a column are dictionary encoded, the two dictionaries are
// mapped onto one shared id space once -- a few thousand string comparisons --
// after which "did this cell change" is `int32 != int32`, which the compiler
// vectorises, and the resulting mismatch mask is scanned eight bytes at a time
// with the same SWAR trick the CSV parser uses to find delimiters.
//
// The result is the same `Result`, and therefore the same JSON, as comparing
// the same data as CSV. That equivalence is the test: cpp/test.sh converts a
// CSV pair to Parquet and requires the two reports to be byte-identical.
#pragma once

#include <string>

#include "csvdiff.hpp"

namespace csvdiff {

// True when the file begins with Parquet's `PAR1` magic. Cheap: four bytes.
bool is_parquet(const std::string& path);

// Both files must be Parquet. Throws Error naming the problem otherwise.
Result compare_parquet(const std::string& a_path, const std::string& b_path, const Options& opt);

}  // namespace csvdiff
