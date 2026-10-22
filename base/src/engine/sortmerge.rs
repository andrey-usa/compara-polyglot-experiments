//! The out-of-core engine: sort both files by key, then walk them together.
//!
//! The other engines hold at least one whole file in memory, so the largest
//! comparison is bounded by the machine. This one is bounded by disk instead.
//! Batches of rows are sorted and spilled, the spilled runs are merged back as
//! one ordered stream, and the join is a single ordered pass down both sides at
//! once — so memory holds a few batches, one row per run, and the capped
//! report, and nothing else grows with the file.
//!
//! The two files are sorted at once, on two threads, and each spills a batch on
//! a thread of its own while the next one is read: two batches per file are in
//! memory at the peak. A row is held as text packed into the batch's one buffer
//! ([`Packed`]), not as a `String` per cell, which is where this engine used to
//! spend most of its time.
//!
//! That is the classical answer to comparing data larger than memory, and it is
//! what reconciliation systems have done since the files lived on tape. It is
//! not the fastest engine here and is not meant to be: sorting is `O(n log n)`
//! where a hash join is linear, and the spill writes the data twice more. What
//! it buys is that the answer does not depend on how much RAM you have.
//!
//! Duplicate keys are nearly free, because sorting puts the repeats next to each
//! other, and the sections come out in key order for the same reason, so nothing
//! is sorted a second time at the end.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::columns::{compare_keys, differs, resolve};
use crate::contract::{Cell, CellDiff, ColumnStat, Counts, EngineResult, Section, Val};
use crate::error::{Error, Result};
use crate::options::Options;
use crate::phases::Phases;
use crate::rowstore::{Joined, Packed};
use crate::sections::assemble;

/// How much memory a batch may occupy before it is spilled: its packed text,
/// four bytes a cell and the sort's four bytes a row.
const DEFAULT_BATCH_BYTES: usize = 32 << 20;

/// Overrides the batch budget so the spill and merge path can be exercised on a
/// file small enough to assert about. A comparison below the default never
/// spills at all, which would leave the interesting half of this module untested.
pub const BATCH_BYTES_ENV: &str = "COMPARA_SORTMERGE_BATCH_BYTES";

fn batch_bytes() -> usize {
    std::env::var(BATCH_BYTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_BATCH_BYTES)
}

pub fn compare(a_path: &Path, b_path: &Path, opt: &Options) -> Result<EngineResult> {
    let a_header = read_header(a_path, opt)?;
    let b_header = read_header(b_path, opt)?;
    let resolved = resolve(&a_header, &b_header, opt)?;

    let key_size = opt.key.len();
    let width = key_size + resolved.compared.len();
    let work = temp_dir()?;
    let guard = DirGuard(work.clone());

    let mut phases = Phases::new("");
    let sort = |side: &str, path: &Path, header: &[String]| -> Result<Runs> {
        stream_into(
            &work.join(side),
            path,
            header,
            &resolved.compared,
            opt,
            width,
        )
    };
    let (a_runs, b_runs) = if opt.thread_budget() > 1 {
        std::thread::scope(|scope| {
            let b = scope.spawn(|| sort("b", b_path, &b_header));
            let a = sort("a", a_path, &a_header);
            let b = b
                .join()
                .unwrap_or_else(|_| Err(Error::new("sorting the B file panicked")));
            (a, b)
        })
    } else {
        (sort("a", a_path, &a_header), sort("b", b_path, &b_header))
    };
    let (a_runs, b_runs) = (a_runs?, b_runs?);
    let (a_rows, b_rows) = (a_runs.rows, b_runs.rows);
    phases.mark("read and sort (2 ways)");

    let (ca, cb) = (a_runs.sorted()?, b_runs.sorted()?);

    let mut dup_a = Dups::new(&opt.key, opt.max_rows);
    let mut dup_b = Dups::new(&opt.key, opt.max_rows);
    let mut join = |mut ca: Box<dyn Cursor>, mut cb: Box<dyn Cursor>| {
        merge_join(
            &mut ca,
            &mut cb,
            opt,
            &resolved.compared,
            a_rows,
            b_rows,
            &mut dup_a,
            &mut dup_b,
            opt.export_dir.is_some(),
        )
    };
    let joined = if opt.thread_budget() > 2 {
        std::thread::scope(|scope| {
            let ca = Ahead::start(scope, ca, width)?;
            let cb = Ahead::start(scope, cb, width)?;
            join(Box::new(ca), Box::new(cb))
        })?
    } else {
        join(ca, cb)?
    };
    phases.mark("merge join");

    let meta = resolved.meta(&opt.key, a_header.len(), b_header.len());
    let out = assemble(
        meta,
        joined,
        dup_a.section(),
        dup_b.section(),
        opt,
        &resolved.compared,
    );
    drop(guard);
    out
}

/// Removes the working directory, spilled runs and all, however the run ended.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir().join(format!(
        "compara-sortmerge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn reader(path: &Path, opt: &Options) -> Result<csv::Reader<File>> {
    use std::io::{Seek, SeekFrom};

    let mut file =
        File::open(path).map_err(|e| Error::new(format!("cannot read {}: {e}", path.display())))?;
    let delimiter = match opt.delimiter_byte()? {
        Some(d) => d,
        None => {
            let mut line = String::new();
            BufReader::new(&mut file)
                .read_line(&mut line)
                .map_err(|e| Error::new(format!("cannot read {}: {e}", path.display())))?;
            file.seek(SeekFrom::Start(0))?;
            crate::columns::detect_delimiter(&line)
        }
    };
    Ok(csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true)
        .from_reader(file))
}

fn read_header(path: &Path, opt: &Options) -> Result<Vec<String>> {
    let mut rdr = reader(path, opt)?;
    let headers = rdr
        .headers()
        .map_err(|_| Error::new(format!("file has no header row: {}", path.display())))?;
    if headers.is_empty() {
        return Err(Error::new(format!(
            "file has no header row: {}",
            path.display()
        )));
    }
    Ok(headers.iter().map(str::to_string).collect())
}

/// Streams a file into the sorter, projecting and normalising a row at a time.
fn stream_into(
    dir: &Path,
    path: &Path,
    header: &[String],
    compared: &[String],
    opt: &Options,
    width: usize,
) -> Result<Runs> {
    std::fs::create_dir_all(dir)?;
    let index: Vec<Option<usize>> = opt
        .key
        .iter()
        .chain(compared)
        .map(|name| header.iter().position(|c| c == name))
        .collect();
    let key_size = opt.key.len();
    let budget = batch_bytes();

    // Full batches go to a thread that sorts and writes them while this one
    // reads on. The channel holds one, so at most two are in memory: the one
    // being written and the one being filled.
    let (to_spill, batches) = mpsc::sync_channel::<Packed>(0);
    let spill_dir = dir.to_path_buf();
    std::thread::scope(|scope| {
        let spiller = scope.spawn(move || -> Result<Vec<PathBuf>> {
            let mut spilled = Vec::new();
            for batch in batches {
                let path = spill_dir.join(format!("run-{}.bin", spilled.len()));
                let mut w = BufWriter::with_capacity(1 << 16, File::create(&path)?);
                for row in sorted_order(&batch, key_size) {
                    write_row(&mut w, &batch, row as usize)?;
                }
                w.flush()?;
                spilled.push(path);
            }
            Ok(spilled)
        });

        let read = || -> Result<(Packed, i64)> {
            let mut batch = Packed::new(width);
            let mut rows = 0i64;
            let mut sent = false;
            let mut rdr = reader(path, opt)?;
            let mut record = csv::StringRecord::new();
            while rdr
                .read_record(&mut record)
                .map_err(|e| Error::new(format!("cannot read {}: {e}", path.display())))?
            {
                rows += 1;
                batch
                    .push_normalised(index.iter().map(|at| at.and_then(|i| record.get(i))), opt)?;
                if batch.bytes() + batch.len() * 4 >= budget {
                    let full = std::mem::replace(&mut batch, Packed::new(width));
                    sent = true;
                    if to_spill.send(full).is_err() {
                        break; // the spiller failed, and says why below
                    }
                }
            }
            // Once anything has spilled, the rest is one more run; a file that
            // never filled a batch stays in memory and is sorted here.
            if sent && !batch.is_empty() {
                let rest = std::mem::replace(&mut batch, Packed::new(width));
                let _ = to_spill.send(rest);
            }
            Ok((batch, rows))
        };
        let got = read();
        drop(to_spill);
        let spilled = spiller
            .join()
            .unwrap_or_else(|_| Err(Error::new("spilling a sorted run panicked")))?;
        let (last, rows) = got?;
        let order = if spilled.is_empty() {
            sorted_order(&last, key_size)
        } else {
            Vec::new()
        };
        Ok(Runs {
            width,
            key_size,
            spilled,
            last,
            order,
            rows,
        })
    })
}

// ---------------------------------------------------------------------------
// External sort
// ---------------------------------------------------------------------------

/// A batch's rows in key order. Stable, so equal keys keep file order: batches
/// are filled in file order and the merge breaks equal keys by run number, so
/// the first row carrying a key is still the first row the join sees -- which
/// is what makes first-occurrence-wins mean the same thing here as in every
/// other engine.
fn sorted_order(batch: &Packed, key_size: usize) -> Vec<u32> {
    let mut order: Vec<u32> = (0..batch.len() as u32).collect();
    order.sort_by(|&x, &y| batch.key_order(x as usize, batch, y as usize, key_size));
    order
}

/// One file, sorted: the runs spilled to disk, or the one batch it took and
/// that batch's order.
struct Runs {
    width: usize,
    key_size: usize,
    spilled: Vec<PathBuf>,
    last: Packed,
    order: Vec<u32>,
    rows: i64,
}

impl Runs {
    /// Finishes the sort and returns the rows in key order. A sort that never
    /// spilled is returned straight from memory, so the common case of a file
    /// that fits pays nothing for the machinery that handles one that does not.
    fn sorted(self) -> Result<Box<dyn Cursor + Send>> {
        if self.spilled.is_empty() {
            return Ok(Box::new(BatchCursor {
                rows: self.last,
                order: self.order,
                at: 0,
            }));
        }
        Ok(Box::new(MergeCursor::open(
            &self.spilled,
            self.width,
            self.key_size,
        )?))
    }
}

/// One ordered pass over sorted rows.
trait Cursor {
    /// The row at the cursor, as a row of a packed store, or `None` once the
    /// rows are exhausted.
    fn peek(&mut self) -> Option<(&Packed, usize)>;
    /// Advances past the current row.
    fn next(&mut self) -> Result<()>;
}

/// How many rows a cursor running ahead hands over at a time.
const AHEAD_ROWS: usize = 4096;

/// A cursor run on a thread of its own, a block of rows ahead of whoever reads
/// it, so that both files' merges and the join run at once rather than in turn.
struct Ahead {
    blocks: mpsc::Receiver<Result<Packed>>,
    block: Packed,
    at: usize,
}

impl Ahead {
    fn start<'s>(
        scope: &'s std::thread::Scope<'s, '_>,
        mut inner: Box<dyn Cursor + Send>,
        width: usize,
    ) -> Result<Ahead> {
        let (to_join, blocks) = mpsc::sync_channel(2);
        scope.spawn(move || {
            loop {
                let mut block = Packed::new(width);
                while block.len() < AHEAD_ROWS {
                    let copied = match inner.peek() {
                        Some((rows, row)) => block.push_row_of(rows, row),
                        None => break,
                    };
                    if let Err(e) = copied.and_then(|_| inner.next()) {
                        let _ = to_join.send(Err(e));
                        return;
                    }
                }
                if block.is_empty() || to_join.send(Ok(block)).is_err() {
                    return;
                }
            }
        });
        let mut ahead = Ahead {
            blocks,
            block: Packed::new(width),
            at: 0,
        };
        ahead.refill()?;
        Ok(ahead)
    }

    /// The next block, or an empty one once the rows are done. An error on the
    /// other thread arrives here rather than looking like the end of the file.
    fn refill(&mut self) -> Result<()> {
        self.at = 0;
        match self.blocks.recv() {
            Ok(block) => self.block = block?,
            Err(_) => self.block.clear(),
        }
        Ok(())
    }
}

impl Cursor for Ahead {
    fn peek(&mut self) -> Option<(&Packed, usize)> {
        (self.at < self.block.len()).then_some((&self.block, self.at))
    }

    fn next(&mut self) -> Result<()> {
        self.at += 1;
        if self.at >= self.block.len() && !self.block.is_empty() {
            self.refill()?;
        }
        Ok(())
    }
}

struct BatchCursor {
    rows: Packed,
    order: Vec<u32>,
    at: usize,
}

impl Cursor for BatchCursor {
    fn peek(&mut self) -> Option<(&Packed, usize)> {
        let row = *self.order.get(self.at)?;
        Some((&self.rows, row as usize))
    }

    fn next(&mut self) -> Result<()> {
        self.at += 1;
        Ok(())
    }
}

/// A k-way merge over the spilled runs, holding one row from each.
struct MergeCursor {
    readers: Vec<BufReader<File>>,
    /// Each run's current row, or an empty store once the run is done.
    heads: Vec<Packed>,
    /// The runs that still have rows, as a binary heap ordered by their heads:
    /// the smallest at the top. A file of ten million rows spills about eighty
    /// runs, and a scan of every head per row was eighty key comparisons a row
    /// where the heap takes about a dozen.
    heap: Vec<usize>,
    key_size: usize,
    scratch: Vec<u8>,
    spans: Vec<Option<(usize, usize)>>,
}

impl MergeCursor {
    fn open(paths: &[PathBuf], width: usize, key_size: usize) -> Result<Self> {
        let mut c = Self {
            readers: Vec::new(),
            heads: Vec::new(),
            heap: Vec::new(),
            key_size,
            scratch: Vec::new(),
            spans: Vec::new(),
        };
        for p in paths {
            c.readers
                .push(BufReader::with_capacity(1 << 16, File::open(p)?));
            c.heads.push(Packed::new(width));
        }
        for run in 0..c.readers.len() {
            c.pull(run)?;
            if !c.heads[run].is_empty() {
                c.heap.push(run);
            }
        }
        for at in (0..c.heap.len() / 2).rev() {
            c.sift_down(at);
        }
        Ok(c)
    }

    fn pull(&mut self, run: usize) -> Result<()> {
        self.heads[run].clear();
        read_row(
            &mut self.readers[run],
            &mut self.heads[run],
            &mut self.scratch,
            &mut self.spans,
        )
    }

    /// Whether run `x`'s head comes before run `y`'s: by key, and between equal
    /// keys by run number, so that file order survives the merge.
    fn before(&self, x: usize, y: usize) -> bool {
        match self.heads[x].key_order(0, &self.heads[y], 0, self.key_size) {
            Ordering::Less => true,
            Ordering::Equal => x < y,
            Ordering::Greater => false,
        }
    }

    fn sift_down(&mut self, mut at: usize) {
        loop {
            let (l, r) = (2 * at + 1, 2 * at + 2);
            let mut least = at;
            if l < self.heap.len() && self.before(self.heap[l], self.heap[least]) {
                least = l;
            }
            if r < self.heap.len() && self.before(self.heap[r], self.heap[least]) {
                least = r;
            }
            if least == at {
                return;
            }
            self.heap.swap(at, least);
            at = least;
        }
    }
}

impl Cursor for MergeCursor {
    fn peek(&mut self) -> Option<(&Packed, usize)> {
        Some((&self.heads[*self.heap.first()?], 0))
    }

    fn next(&mut self) -> Result<()> {
        let Some(&run) = self.heap.first() else {
            return Ok(());
        };
        self.pull(run)?;
        if self.heads[run].is_empty() {
            let last = self.heap.pop().expect("the heap holds this run");
            if self.heap.is_empty() {
                return Ok(());
            }
            self.heap[0] = last;
        }
        self.sift_down(0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The spill format: a length-prefixed field per column, varint lengths, where
// zero is an absent field and a real length is stored one higher.
// ---------------------------------------------------------------------------

fn write_row<W: Write>(w: &mut W, rows: &Packed, row: usize) -> Result<()> {
    for i in 0..rows.width() {
        match rows.cell(row, i) {
            None => write_varint(w, 0)?,
            Some(s) => {
                write_varint(w, s.len() as u64 + 1)?;
                w.write_all(s.as_bytes())?;
            }
        }
    }
    Ok(())
}

/// Reads one row into `into`, which is left empty at the end of the run.
fn read_row<R: BufRead>(
    r: &mut R,
    into: &mut Packed,
    scratch: &mut Vec<u8>,
    spans: &mut Vec<Option<(usize, usize)>>,
) -> Result<()> {
    let width = into.width();
    scratch.clear();
    spans.clear();
    for i in 0..width {
        match read_varint(r)? {
            None if i == 0 => return Ok(()),
            None => return Err(Error::new("spilled run ended mid-row".to_string())),
            Some(0) => spans.push(None),
            Some(n) => {
                let from = scratch.len();
                scratch.resize(from + (n - 1) as usize, 0);
                r.read_exact(&mut scratch[from..])
                    .map_err(|_| Error::new("spilled run ended mid-row".to_string()))?;
                spans.push(Some((from, scratch.len())));
            }
        }
    }
    let text = std::str::from_utf8(scratch)
        .map_err(|e| Error::new(format!("spilled run holds invalid UTF-8: {e}")))?;
    if !spans
        .iter()
        .flatten()
        .all(|&(a, b)| text.is_char_boundary(a) && text.is_char_boundary(b))
    {
        return Err(Error::new("spilled run holds invalid UTF-8"));
    }
    into.push(spans.iter().map(|c| c.map(|(a, b)| &text[a..b])))
}

fn write_varint<W: Write>(w: &mut W, value: u64) -> Result<()> {
    let mut v = value;
    while v >= 0x80 {
        w.write_all(&[(v as u8) | 0x80])?;
        v >>= 7;
    }
    w.write_all(&[v as u8])?;
    Ok(())
}

fn read_varint<R: BufRead>(r: &mut R) -> Result<Option<u64>> {
    // A length is almost always whole in the buffer: read it from there,
    // rather than through a one-byte `read` per byte.
    let buf = r.fill_buf()?;
    if buf.len() >= 10 {
        let mut result: u64 = 0;
        for (i, &b) in buf[..10].iter().enumerate() {
            result |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                r.consume(i + 1);
                return Ok(Some(result));
            }
        }
        return Err(Error::new(
            "corrupt spilled run: length is not a varint".to_string(),
        ));
    }
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        match r.read(&mut byte)? {
            0 if shift == 0 => return Ok(None),
            0 => return Err(Error::new("spilled run ended mid-row".to_string())),
            _ => {}
        }
        result |= ((byte[0] & 0x7F) as u64) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(Some(result));
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::new(
                "corrupt spilled run: length is not a varint".to_string(),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// The join
// ---------------------------------------------------------------------------

/// Walks two sorted cursors and builds the finished join.
///
/// Every other engine indexes a whole file so it can ask "is this key on the
/// other side?". This one never asks. Both sides arrive in key order, so the
/// smaller key can only be missing from the other file, equal keys are a match,
/// and the answer falls out of walking the two cursors forward.
#[allow(clippy::too_many_arguments)]
fn merge_join(
    a: &mut Box<dyn Cursor>,
    b: &mut Box<dyn Cursor>,
    opt: &Options,
    compared: &[String],
    a_rows: i64,
    b_rows: i64,
    dup_a: &mut Dups,
    dup_b: &mut Dups,
    exporting: bool,
) -> Result<Joined> {
    let key_size = opt.key.len();
    let nc = compared.len();
    let width = key_size + nc;

    let mut changed_per = vec![0i64; nc];
    let mut blanked_per = vec![0i64; nc];
    let mut filled_per = vec![0i64; nc];

    let mut changed = Capped::new(opt.max_rows, exporting);
    let mut added = Capped::new(opt.max_rows, exporting);
    let mut removed = Capped::new(opt.max_rows, exporting);
    let mut changed_a: Vec<Vec<Val>> = Vec::new();
    let mut changed_b: Vec<Vec<Val>> = Vec::new();

    let (mut matched, mut a_keys, mut b_keys) = (0i64, 0i64, 0i64);
    // Each side's current row, copied out of its cursor: the cursor moves on
    // past the row's duplicates while the row is still being compared.
    let (mut ar, mut br) = (Packed::new(width), Packed::new(width));
    current(a, &mut ar)?;
    current(b, &mut br)?;

    loop {
        let order = match (ar.is_empty(), br.is_empty()) {
            (true, true) => break,
            (false, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, false) => ar.key_order(0, &br, 0, key_size),
        };

        match order {
            Ordering::Less => {
                a_keys += 1;
                removed.push_with(|| to_cells(&ar.row(0)));
                skip_key(a, &mut ar, key_size, dup_a)?;
            }
            Ordering::Greater => {
                b_keys += 1;
                added.push_with(|| to_cells(&br.row(0)));
                skip_key(b, &mut br, key_size, dup_b)?;
            }
            Ordering::Equal => {
                a_keys += 1;
                b_keys += 1;
                matched += 1;

                let mut cells: Vec<CellDiff> = Vec::new();
                for i in 0..nc {
                    // Equal text is never a difference, with or without a
                    // tolerance, so only cells that differ as text become values.
                    if ar.cell_bytes(0, key_size + i) == br.cell_bytes(0, key_size + i) {
                        continue;
                    }
                    let (x, y) = (ar.cell(0, key_size + i), br.cell(0, key_size + i));
                    let (x, y) = (x.map(str::to_string), y.map(str::to_string));
                    if differs(&x, &y, opt) {
                        if y.is_none() {
                            blanked_per[i] += 1;
                        }
                        if x.is_none() {
                            filled_per[i] += 1;
                        }
                        changed_per[i] += 1;
                        cells.push(CellDiff {
                            column: i,
                            a: x,
                            b: y,
                        });
                    }
                }
                if !cells.is_empty() {
                    let arow = ar.row(0);
                    changed.push_with(|| {
                        let mut row: Vec<Cell> = arow[..key_size]
                            .iter()
                            .map(|v| Cell::Value(v.clone()))
                            .collect();
                        row.push(Cell::Diffs(cells));
                        row
                    });
                    if exporting || changed_a.len() <= opt.max_rows {
                        changed_a.push(arow);
                        changed_b.push(br.row(0));
                    }
                }
                skip_key(a, &mut ar, key_size, dup_a)?;
                skip_key(b, &mut br, key_size, dup_b)?;
            }
        }
    }

    let columns: Vec<ColumnStat> = (0..nc)
        .map(|i| ColumnStat {
            name: compared[i].clone(),
            changed: changed_per[i],
            blanked: blanked_per[i],
            filled: filled_per[i],
        })
        .collect();

    let counts = Counts {
        a_rows,
        b_rows,
        a_keys,
        b_keys,
        matched,
        unchanged: matched - changed.total,
        changed: changed.total,
        added: added.total,
        removed: removed.total,
        a_dup_keys: dup_a.keys,
        a_dup_rows: dup_a.rows,
        b_dup_keys: dup_b.keys,
        b_dup_rows: dup_b.rows,
    };

    Ok(Joined {
        counts,
        columns,
        changed: changed.held,
        added: added.held,
        removed: removed.held,
        changed_a,
        changed_b,
    })
}

/// Copies the cursor's row into `into`, which is left empty past the end.
fn current(c: &mut Box<dyn Cursor>, into: &mut Packed) -> Result<()> {
    into.clear();
    match c.peek() {
        Some((rows, row)) => into.push_row_of(rows, row),
        None => Ok(()),
    }
}

/// Advances past every row sharing the current key, counting the repeats as
/// duplicates, and loads the next key's first row into `row`. The first row
/// of a run is the one the join used, which is first-occurrence-wins: the sort
/// is stable and the merge breaks ties by run.
fn skip_key(
    c: &mut Box<dyn Cursor>,
    row: &mut Packed,
    key_size: usize,
    dups: &mut Dups,
) -> Result<()> {
    c.next()?;
    let mut repeats = 0i64;
    while let Some((rows, at)) = c.peek() {
        if rows.key_order(at, row, 0, key_size) != Ordering::Equal {
            break;
        }
        repeats += 1;
        c.next()?;
    }
    if repeats > 0 {
        dups.record(&row.row(0), repeats + 1);
    }
    current(c, row)
}

fn to_cells(row: &[Val]) -> Vec<Cell> {
    row.iter().map(|v| Cell::Value(v.clone())).collect()
}

/// A row list that stops growing at the report cap but keeps counting.
///
/// The counts in the contract are always exact and the embedded rows are always
/// capped, so holding more than the cap only ever serves `--export-dir`. Not
/// holding them is what lets this engine compare a file far larger than memory.
struct Capped {
    held: Vec<Vec<Cell>>,
    cap: usize,
    unbounded: bool,
    total: i64,
}

impl Capped {
    fn new(cap: usize, unbounded: bool) -> Self {
        Self {
            held: Vec::new(),
            cap,
            unbounded,
            total: 0,
        }
    }

    /// Counts a row, and builds it only if it will be held.
    fn push_with(&mut self, row: impl FnOnce() -> Vec<Cell>) {
        self.total += 1;
        // One past the cap, so a section can still report that it was truncated.
        if self.unbounded || self.held.len() <= self.cap {
            self.held.push(row());
        }
    }
}

/// The duplicate-key report, kept to the top `--max-rows` by count.
///
/// A bounded list of the worst offenders, rather than every duplicate, so a
/// pathological file where every key repeats does not undo the memory bound.
/// Ordering matches every other engine: most duplicated first, then by key.
struct Dups {
    key_cols: Vec<String>,
    key_size: usize,
    cap: usize,
    best: Vec<(Vec<Val>, i64)>,
    keys: i64,
    rows: i64,
}

impl Dups {
    fn new(key_cols: &[String], cap: usize) -> Self {
        Self {
            key_cols: key_cols.to_vec(),
            key_size: key_cols.len(),
            cap,
            best: Vec::new(),
            keys: 0,
            rows: 0,
        }
    }

    fn record(&mut self, row: &[Val], count: i64) {
        self.keys += 1;
        self.rows += count;
        self.best.push((row[..self.key_size].to_vec(), count));
        if self.best.len() > self.cap + 1 {
            self.sort_best();
            self.best.truncate(self.cap + 1);
        }
    }

    fn sort_best(&mut self) {
        let key_size = self.key_size;
        self.best.sort_by(|x, y| {
            y.1.cmp(&x.1)
                .then_with(|| compare_keys(&x.0, &y.0, key_size))
        });
    }

    fn section(&mut self) -> Section {
        self.sort_best();
        let rows: Vec<Vec<Cell>> = self
            .best
            .iter()
            .take(self.cap)
            .map(|(key, count)| {
                let mut row: Vec<Cell> = key.iter().map(|v| Cell::Value(v.clone())).collect();
                row.push(Cell::Count(*count));
                row
            })
            .collect();
        let mut cols = self.key_cols.clone();
        cols.push("count".to_string());
        Section {
            cols,
            rows,
            truncated: self.keys > self.cap as i64,
        }
    }
}

/// The sort-merge engine has no optional dependency.
pub fn available() -> bool {
    true
}
