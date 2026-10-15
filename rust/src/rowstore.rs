//! The in-memory half of a comparison.
//!
//! [`RowStore`] owns the parts that must not drift between engines —
//! first-occurrence-wins de-duplication, the duplicate-key report, the full
//! outer join and the sparse cell diffs — so an engine only has to supply
//! normalised rows. [`Packed`] is how rows are held while they wait.
//!

use std::borrow::Cow;
use std::cmp::Ordering;
use std::ops::Range;

use crate::columns::{compare_keys, differs};
use crate::contract::{Cell, CellDiff, ColumnStat, Counts, Section, Val};
use crate::error::{Error, Result};
use crate::options::Options;

/// Rows of text cells, laid end to end in one buffer.
///
/// Ten million rows of twenty columns is two hundred million cells. Held as an
/// `Option<String>` each, every cell is an allocation with its own header and
/// its own free, and the engines that held rows that way spent most of their
/// time in the allocator and most of their memory on its bookkeeping: `native`
/// took 27 s and 2.9 GB for a pair of a million rows. Here a row costs its
/// text and four bytes a cell.
#[derive(Default)]
pub struct Packed {
    width: usize,
    text: String,
    /// Per cell, where it ends within its row's text, with [`ABSENT`] set on a
    /// cell that has no value.
    ends: Vec<u32>,
    /// Per row, where its text starts.
    starts: Vec<usize>,
}

const ABSENT: u32 = 1 << 31;

impl Packed {
    pub fn new(width: usize) -> Self {
        Packed {
            width,
            ..Packed::default()
        }
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Makes room for `rows` rows of about `bytes` of text each, as far as
    /// the allocator will: it is a hint, and growing as rows arrive is the
    /// fallback.
    pub fn reserve(&mut self, rows: usize, bytes: usize) {
        let more = rows.saturating_sub(self.len());
        let _ = self.text.try_reserve(more.saturating_mul(bytes));
        let _ = self.ends.try_reserve(more.saturating_mul(self.width));
        let _ = self.starts.try_reserve(more);
    }

    /// The text a row holds on average, so far.
    pub fn bytes_per_row(&self) -> usize {
        self.text.len() / self.len().max(1)
    }

    /// The bytes held, for a caller that keeps to a budget.
    pub fn bytes(&self) -> usize {
        self.text.len() + self.ends.len() * 4 + self.starts.len() * 8
    }

    /// Appends one row: a cell per column, `None` where there is no value.
    pub fn push<'a>(&mut self, cells: impl IntoIterator<Item = Option<&'a str>>) -> Result<()> {
        let start = self.text.len();
        self.starts.push(start);
        for cell in cells {
            if let Some(s) = cell {
                self.text.push_str(s);
            }
            let end = self.text.len() - start;
            if end >= ABSENT as usize {
                return Err(Error::new("a row holds more than 2 GB of text"));
            }
            self.ends
                .push(end as u32 | if cell.is_none() { ABSENT } else { 0 });
        }
        Ok(())
    }

    /// Appends one row of raw cells as [`crate::columns::normalise`] leaves
    /// them -- empty is absent, then `--trim`, `--ignore-case` and
    /// `--empty-is-null` -- without a `String` per cell where none is needed.
    pub fn push_normalised<'a>(
        &mut self,
        cells: impl IntoIterator<Item = Option<&'a str>>,
        opt: &Options,
    ) -> Result<()> {
        if !opt.trim && !opt.ignore_case {
            return self.push(cells.into_iter().map(|c| c.filter(|s| !s.is_empty())));
        }
        let normalised: Vec<Option<Cow<'_, str>>> = cells
            .into_iter()
            .map(|cell| {
                let s = cell.filter(|s| !s.is_empty())?;
                let s = if opt.trim { s.trim() } else { s };
                let s = if opt.ignore_case {
                    Cow::Owned(s.to_lowercase())
                } else {
                    Cow::Borrowed(s)
                };
                (!(opt.empty_is_null && s.is_empty())).then_some(s)
            })
            .collect();
        self.push(normalised.iter().map(|c| c.as_deref()))
    }

    /// Appends a copy of another store's row.
    pub fn push_row_of(&mut self, other: &Packed, row: usize) -> Result<()> {
        self.push((0..other.width).map(|i| other.cell(row, i)))
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
        self.starts.clear();
    }

    /// Drops the last row.
    pub fn pop(&mut self) {
        if let Some(start) = self.starts.pop() {
            self.text.truncate(start);
            self.ends.truncate(self.starts.len() * self.width);
        }
    }

    #[inline]
    pub fn cell(&self, row: usize, i: usize) -> Option<&str> {
        let (from, to) = self.span(row, i)?;
        Some(&self.text[from..to])
    }

    /// A cell's bytes. Two cells compare as their bytes do as their text --
    /// UTF-8 orders as the code points it encodes -- without the check that
    /// slicing a `str` makes at both ends.
    #[inline]
    pub fn cell_bytes(&self, row: usize, i: usize) -> Option<&[u8]> {
        let (from, to) = self.span(row, i)?;
        Some(&self.text.as_bytes()[from..to])
    }

    #[inline]
    fn span(&self, row: usize, i: usize) -> Option<(usize, usize)> {
        let at = row * self.width + i;
        let end = self.ends[at];
        if end & ABSENT != 0 {
            return None;
        }
        let from = if i == 0 {
            0
        } else {
            self.ends[at - 1] & !ABSENT
        };
        let start = self.starts[row];
        Some((start + from as usize, start + end as usize))
    }

    /// One row as values, for the report.
    pub fn row(&self, row: usize) -> Vec<Val> {
        (0..self.width)
            .map(|i| self.cell(row, i).map(str::to_string))
            .collect()
    }

    /// [`compare_keys`] on two packed rows.
    pub fn key_order(
        &self,
        row: usize,
        other: &Packed,
        other_row: usize,
        key_size: usize,
    ) -> Ordering {
        for i in 0..key_size {
            match (self.cell_bytes(row, i), other.cell_bytes(other_row, i)) {
                (None, None) => continue,
                (None, Some(_)) => return Ordering::Greater,
                (Some(_), None) => return Ordering::Less,
                (Some(a), Some(b)) => match a.cmp(b) {
                    Ordering::Equal => continue,
                    other => return other,
                },
            }
        }
        Ordering::Equal
    }

    /// A hash of the row's key cells, with an absent cell distinct from any
    /// value, the empty one included.
    pub fn key_hash(&self, row: usize, key_size: usize) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for i in 0..key_size {
            match self.cell_bytes(row, i) {
                None => h = (h ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(0x100000001b3),
                Some(v) => {
                    let (words, tail) = v.as_chunks::<8>();
                    for w in words {
                        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x100000001b3);
                        h ^= h >> 29;
                    }
                    for &b in tail {
                        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
                    }
                    h = (h ^ v.len() as u64).wrapping_mul(0x100000001b3);
                }
            }
        }
        // The index takes its slot from the low bits and its tag from the
        // high ones; this spreads every input bit into both.
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^ (h >> 33)
    }

    /// Whether two rows carry the same key.
    pub fn key_eq(&self, row: usize, other: &Packed, other_row: usize, key_size: usize) -> bool {
        (0..key_size).all(|i| self.cell_bytes(row, i) == other.cell_bytes(other_row, i))
    }
}

/// Each distinct key's row in a [`Packed`] store: open addressing over row
/// numbers, keyed by the rows' own cells, so no key is copied out of the store.
///
/// A slot is the top half of the key's hash beside the row plus one, zero
/// for empty, so a probe that meets another key is settled by the word it
/// already loaded. Every row's hash is kept, so growing the table never reads
/// a key again. A map of copied keys -- an allocation each, dereferenced by
/// every probe and every resize -- took four times as long per row at four
/// million rows as at one.
#[derive(Default)]
struct KeyIndex {
    slots: Vec<u64>,
    hashes: Vec<u64>,
}

impl KeyIndex {
    fn slot(hash: u64, row: usize) -> u64 {
        (hash & 0xFFFF_FFFF_0000_0000) | (row as u64 + 1)
    }

    /// The row holding the key `hash` belongs to, as `same` judges a
    /// candidate row, if one does.
    fn find(&self, hash: u64, same: impl Fn(usize) -> bool) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return None;
            }
            if slot >> 32 == hash >> 32 {
                let row = (slot & 0xFFFF_FFFF) as usize - 1;
                if same(row) {
                    return Some(row);
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Makes room for `keys` keys, so a table that would have doubled its way
    /// there -- a pass over every key at each step -- is sized once.
    fn reserve(&mut self, keys: usize) {
        let size = (keys * 2 + 1).next_power_of_two().max(1024);
        let mut slots = Vec::new();
        if size <= self.slots.len() || slots.try_reserve_exact(size).is_err() {
            return;
        }
        slots.resize(size, 0);
        self.slots = slots;
        for (r, &h) in self.hashes.iter().enumerate() {
            Self::place(&mut self.slots, h, r);
        }
        let _ = self
            .hashes
            .try_reserve(keys.saturating_sub(self.hashes.len()));
    }

    /// Records `row` as the first holder of a key `find` did not find.
    fn insert(&mut self, hash: u64, row: usize) {
        self.hashes.push(hash);
        if self.hashes.len() * 2 > self.slots.len() {
            let size = (self.slots.len() * 2).max(1024);
            self.slots = vec![0; size];
            for (r, &h) in self.hashes.iter().enumerate() {
                Self::place(&mut self.slots, h, r);
            }
        } else {
            Self::place(&mut self.slots, hash, row);
        }
    }

    fn place(slots: &mut [u64], hash: u64, row: usize) {
        let mask = slots.len() - 1;
        let mut at = hash as usize & mask;
        while slots[at] != 0 {
            at = (at + 1) & mask;
        }
        slots[at] = Self::slot(hash, row);
    }
}

/// Rows shaped as key columns followed by compared columns.
pub struct RowStore {
    key: Vec<String>,
    compared: Vec<String>,
    key_size: usize,
    max_rows: usize,

    /// The first row carrying each key, in order of first appearance, which
    /// is the order the join walks, so a run is reproducible.
    rows: Packed,
    /// Per row of `rows`, how many rows of the file carry its key.
    counts: Vec<u32>,
    /// Each key's row in `rows`.
    index: KeyIndex,
    total: i64,
}

impl RowStore {
    pub fn new(opt: &Options, compared: &[String]) -> Self {
        RowStore {
            key: opt.key.clone(),
            compared: compared.to_vec(),
            key_size: opt.key.len(),
            max_rows: opt.max_rows,
            rows: Packed::new(opt.key.len() + compared.len()),
            counts: Vec::new(),
            index: KeyIndex::default(),
            total: 0,
        }
    }

    /// Makes room for about `rows` rows, which a caller that can estimate them
    /// -- from the file's size -- saves the store growing its way to.
    pub fn expect(&mut self, rows: usize) {
        self.index.reserve(rows);
        self.rows.reserve(rows, self.rows.bytes_per_row());
        let _ = self
            .counts
            .try_reserve(rows.saturating_sub(self.counts.len()));
    }

    /// Records one row of raw cells, normalised here.
    pub fn add<'a>(
        &mut self,
        cells: impl IntoIterator<Item = Option<&'a str>>,
        opt: &Options,
    ) -> Result<()> {
        self.total += 1;
        self.rows.push_normalised(cells, opt)?;
        let last = self.rows.len() - 1;
        let (rows, ks) = (&self.rows, self.key_size);
        let hash = rows.key_hash(last, ks);
        if let Some(first) = self.index.find(hash, |r| rows.key_eq(r, rows, last, ks)) {
            self.counts[first] += 1;
            self.rows.pop();
        } else {
            self.index.insert(hash, last);
            self.counts.push(1);
        }
        Ok(())
    }

    /// The number of rows read, duplicates included.
    pub fn rows(&self) -> i64 {
        self.total
    }

    /// The number of distinct composite keys.
    pub fn unique_keys(&self) -> i64 {
        self.rows.len() as i64
    }

    /// How many keys appear more than once.
    pub fn duplicate_keys(&self) -> i64 {
        self.counts.iter().filter(|&&c| c > 1).count() as i64
    }

    /// How many rows carry a duplicated key.
    pub fn duplicate_rows(&self) -> i64 {
        self.counts
            .iter()
            .filter(|&&c| c > 1)
            .map(|&c| i64::from(c))
            .sum()
    }

    /// The duplicate-key list: most duplicated first, then by key.
    pub fn duplicate_section(&self) -> Section {
        let mut entries: Vec<(Vec<Val>, u32)> = self
            .counts
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 1)
            .map(|(row, &count)| {
                let key = (0..self.key_size)
                    .map(|i| self.rows.cell(row, i).map(str::to_string))
                    .collect();
                (key, count)
            })
            .collect();
        entries
            .sort_by(|(x, xn), (y, yn)| yn.cmp(xn).then_with(|| compare_keys(x, y, self.key_size)));

        let truncated = entries.len() > self.max_rows;
        entries.truncate(self.max_rows);
        let rows = entries
            .into_iter()
            .map(|(key, count)| {
                let mut out: Vec<Cell> = key.into_iter().map(Cell::Value).collect();
                out.push(Cell::Count(i64::from(count)));
                out
            })
            .collect();

        let mut cols = self.key.clone();
        cols.push("count".to_string());
        Section {
            cols,
            rows,
            truncated,
        }
    }

    /// The compared column names this store was built for.
    pub fn compared(&self) -> &[String] {
        &self.compared
    }
}

/// Everything the full outer join produces, before the sections are capped.
pub struct Joined {
    pub counts: Counts,
    pub columns: Vec<ColumnStat>,
    pub changed: Vec<Vec<Cell>>,
    pub added: Vec<Vec<Cell>>,
    pub removed: Vec<Vec<Cell>>,
    /// A-side rows aligned with `changed`, for `--export-dir`.
    pub changed_a: Vec<Vec<Val>>,
    pub changed_b: Vec<Vec<Val>>,
}

/// The full outer join of two stores on the composite key.
pub fn join(a: &RowStore, b: &RowStore, opt: &Options) -> Joined {
    let key_size = opt.key.len();
    let nc = a.compared.len();

    // Each A row's match is looked up in B's index and its cells compared on
    // their own, so contiguous ranges of rows are joined at once and their
    // parts put together in order. Nearly all of the time is waiting on
    // memory -- B's rows are visited in A's order -- which is what threads
    // overlap. The sections are sorted by key below, so the order the parts
    // arrive in cannot show.
    let ways = if a.rows.len().max(b.rows.len()) < 1 << 16 {
        1
    } else {
        opt.thread_budget().max(1)
    };
    let parts = in_ranges(a.rows.len(), ways, |range| join_range(a, b, opt, range));
    let added_parts = in_ranges(b.rows.len(), ways, |range| {
        range
            .filter(|&rb| {
                let hash = b.rows.key_hash(rb, key_size);
                a.index
                    .find(hash, |r| a.rows.key_eq(r, &b.rows, rb, key_size))
                    .is_none()
            })
            .map(|rb| to_cells(&b.rows.row(rb)))
            .collect::<Vec<_>>()
    });

    let mut changed_per = vec![0i64; nc];
    let mut blanked_per = vec![0i64; nc];
    let mut filled_per = vec![0i64; nc];
    let mut changed: Vec<Vec<Cell>> = Vec::new();
    let mut changed_a: Vec<Vec<Val>> = Vec::new();
    let mut changed_b: Vec<Vec<Val>> = Vec::new();
    let mut removed: Vec<Vec<Cell>> = Vec::new();
    let mut matched = 0i64;
    for part in parts {
        for i in 0..nc {
            changed_per[i] += part.changed_per[i];
            blanked_per[i] += part.blanked_per[i];
            filled_per[i] += part.filled_per[i];
        }
        changed.extend(part.changed);
        changed_a.extend(part.changed_a);
        changed_b.extend(part.changed_b);
        removed.extend(part.removed);
        matched += part.matched;
    }
    let mut added: Vec<Vec<Cell>> = added_parts.into_iter().flatten().collect();

    let columns = (0..nc)
        .map(|i| ColumnStat {
            name: a.compared[i].clone(),
            changed: changed_per[i],
            blanked: blanked_per[i],
            filled: filled_per[i],
        })
        .collect();

    sort_by_key_cells(&mut added, key_size);
    sort_by_key_cells(&mut removed, key_size);
    sort_changed_together(&mut changed, &mut changed_a, &mut changed_b, key_size);

    let counts = Counts {
        a_rows: a.rows(),
        b_rows: b.rows(),
        a_keys: a.unique_keys(),
        b_keys: b.unique_keys(),
        matched,
        unchanged: matched - changed.len() as i64,
        changed: changed.len() as i64,
        added: added.len() as i64,
        removed: removed.len() as i64,
        a_dup_keys: a.duplicate_keys(),
        a_dup_rows: a.duplicate_rows(),
        b_dup_keys: b.duplicate_keys(),
        b_dup_rows: b.duplicate_rows(),
    };

    Joined {
        counts,
        columns,
        changed,
        added,
        removed,
        changed_a,
        changed_b,
    }
}

/// What one range of A's rows contributes to the join.
struct Part {
    changed_per: Vec<i64>,
    blanked_per: Vec<i64>,
    filled_per: Vec<i64>,
    changed: Vec<Vec<Cell>>,
    changed_a: Vec<Vec<Val>>,
    changed_b: Vec<Vec<Val>>,
    removed: Vec<Vec<Cell>>,
    matched: i64,
}

fn join_range(a: &RowStore, b: &RowStore, opt: &Options, rows: Range<usize>) -> Part {
    let key_size = opt.key.len();
    let nc = a.compared.len();
    let mut out = Part {
        changed_per: vec![0; nc],
        blanked_per: vec![0; nc],
        filled_per: vec![0; nc],
        changed: Vec::new(),
        changed_a: Vec::new(),
        changed_b: Vec::new(),
        removed: Vec::new(),
        matched: 0,
    };
    for ra in rows {
        let hash = a.rows.key_hash(ra, key_size);
        let found = b
            .index
            .find(hash, |r| b.rows.key_eq(r, &a.rows, ra, key_size));
        let Some(rb) = found else {
            out.removed.push(to_cells(&a.rows.row(ra)));
            continue;
        };
        out.matched += 1;

        let mut cells: Vec<CellDiff> = Vec::new();
        for i in 0..nc {
            // Equal text is never a difference, with or without a tolerance,
            // so only the cells that differ as text become values.
            if a.rows.cell_bytes(ra, key_size + i) == b.rows.cell_bytes(rb, key_size + i) {
                continue;
            }
            let (x, y) = (a.rows.cell(ra, key_size + i), b.rows.cell(rb, key_size + i));
            let (x, y) = (x.map(str::to_string), y.map(str::to_string));
            if differs(&x, &y, opt) {
                if y.is_none() {
                    out.blanked_per[i] += 1;
                }
                if x.is_none() {
                    out.filled_per[i] += 1;
                }
                out.changed_per[i] += 1;
                cells.push(CellDiff {
                    column: i,
                    a: x,
                    b: y,
                });
            }
        }
        if !cells.is_empty() {
            let (ar, br) = (a.rows.row(ra), b.rows.row(rb));
            let mut row: Vec<Cell> = ar[..key_size]
                .iter()
                .map(|v| Cell::Value(v.clone()))
                .collect();
            row.push(Cell::Diffs(cells));
            out.changed.push(row);
            out.changed_a.push(ar);
            out.changed_b.push(br);
        }
    }
    out
}

/// `f` over `0..n` split into `ways` contiguous ranges, each on a thread of
/// its own past the first, and the results in range order. A range whose
/// thread the OS refuses is run here instead.
fn in_ranges<T: Send>(n: usize, ways: usize, f: impl Fn(Range<usize>) -> T + Sync) -> Vec<T> {
    let ways = ways.clamp(1, n.max(1));
    let range = |p: usize| n * p / ways..n * (p + 1) / ways;
    if ways == 1 {
        return vec![f(range(0))];
    }
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = (1..ways)
            .map(|p| {
                std::thread::Builder::new()
                    .spawn_scoped(scope, move || f(range(p)))
                    .ok()
            })
            .collect();
        let mut out = vec![f(range(0))];
        for (p, handle) in (1..ways).zip(handles) {
            let done = handle.and_then(|h| h.join().ok());
            out.push(done.unwrap_or_else(|| f(range(p))));
        }
        out
    })
}

fn to_cells(row: &[Val]) -> Vec<Cell> {
    row.iter().map(|v| Cell::Value(v.clone())).collect()
}

fn sort_by_key_cells(rows: &mut [Vec<Cell>], key_size: usize) {
    rows.sort_by(|x, y| {
        for i in 0..key_size {
            let ord = match (x[i].value(), y[i].value()) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Sorts changed rows by key while keeping the parallel A and B row lists in
/// step, which `--export-dir` relies on.
fn sort_changed_together(
    changed: &mut Vec<Vec<Cell>>,
    changed_a: &mut Vec<Vec<Val>>,
    changed_b: &mut Vec<Vec<Val>>,
    key_size: usize,
) {
    let mut idx: Vec<usize> = (0..changed.len()).collect();
    idx.sort_by(|&p, &q| compare_keys(&changed_a[p], &changed_a[q], key_size));

    *changed = reorder(std::mem::take(changed), &idx);
    *changed_a = reorder(std::mem::take(changed_a), &idx);
    *changed_b = reorder(std::mem::take(changed_b), &idx);
}

fn reorder<T>(mut items: Vec<T>, idx: &[usize]) -> Vec<T> {
    let mut slots: Vec<Option<T>> = items.drain(..).map(Some).collect();
    idx.iter()
        .map(|&i| slots[i].take().expect("each index used once"))
        .collect()
}
