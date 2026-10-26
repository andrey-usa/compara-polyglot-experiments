//! The engine registry and the [`compare`] entry point.

pub mod native;
pub mod pqdiff;
pub mod sortmerge;
pub mod turbo;

use std::fs;
use std::path::Path;
use std::time::Instant;

use chrono::{Local, SecondsFormat};

use crate::contract::{CompareResult, EngineResult, FileMeta, Meta};
use crate::error::{Error, Result};
use crate::options::{Engine, Options};

/// Runs the comparison and attaches the run's own facts to the result.
///
/// ```no_run
/// use compara::{Options, compare};
/// use std::path::Path;
///
/// let mut opt = Options::with_key(["order_id", "line_no"]);
/// opt.ignore = vec!["updated_at".to_string()];
/// let result = compare(Path::new("july.csv"), Path::new("august.csv"), &mut opt)?;
/// println!("{} rows changed", result.counts.changed);
/// # Ok::<(), compara::Error>(())
/// ```
pub fn compare(a_path: &Path, b_path: &Path, opt: &mut Options) -> Result<CompareResult> {
    opt.validate()?;
    for path in [a_path, b_path] {
        if !path.is_file() {
            return Err(Error::new(format!("file not found: {}", path.display())));
        }
    }

    // Only the byte-level engine reads JSON and Parquet, so an input in either
    // format decides `auto` on its own rather than being handed to a CSV
    // reader and reported as the parse error it makes of a binary footer.
    let requested = opt.engine_name()?;
    let engine = if requested == Engine::Auto
        && (turbo::only_this_engine_reads(a_path) || turbo::only_this_engine_reads(b_path))
    {
        Engine::Turbo
    } else {
        resolve_engine(requested)
    };
    let start = Instant::now();
    // The label comes back from `run` rather than being reconstructed here: a
    // Parquet pair no longer always takes the columnar path, so which reader
    // actually ran is known only where the choice was made. Saying `parquet`
    // for a run that fell through to `turbo` would claim a path it did not take.
    let (result, label) = run(requested, engine, a_path, b_path, opt)?;
    let seconds = (start.elapsed().as_millis() as f64) / 1000.0;

    let meta = Meta {
        engine_meta: result.meta,
        a: file_meta(a_path),
        b: file_meta(b_path),
        engine: label,
        seconds,
        generated: Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
        options: opt.clone(),
    };
    Ok(CompareResult {
        meta,
        counts: result.counts,
        columns: result.columns,
        changed: result.changed,
        added: result.added,
        removed: result.removed,
        dup_a: result.dup_a,
        dup_b: result.dup_b,
    })
}

/// Runs the comparison, and reports the engine label the run actually earned.
fn run(
    requested: Engine,
    engine: Engine,
    a: &Path,
    b: &Path,
    opt: &Options,
) -> Result<(EngineResult, String)> {
    // Parquet is not a text format and is not read as one. Two Parquet files go
    // to the columnar path, which never materialises a row: it joins on the key
    // columns and then compares whole columns as integers. `--engine turbo`
    // changes the route, because there is a second reader behind it and asking
    // for it should reach it.
    //
    // One Parquet file against a text one has no column to compare a byte
    // stream against, so that path is not available. `turbo` reads it anyway,
    // by decoding the pages into rows (`turbo/parquet.rs`); it costs what the
    // columnar path exists to avoid, and it answers the question rather than
    // refusing it.
    //
    // sortmerge and native read CSV only, so naming one of them on a Parquet
    // or JSON input is refused: taking another path instead would report an
    // answer from an engine that was not asked for.
    if matches!(requested, Engine::SortMerge | Engine::Native)
        && (turbo::only_this_engine_reads(a) || turbo::only_this_engine_reads(b))
    {
        return Err(Error::new(format!(
            "the {requested} engine reads CSV only; for Parquet or JSON use --engine auto \
             (or turbo)"
        )));
    }
    let (a_pq, b_pq) = (pqdiff::is_parquet(a), pqdiff::is_parquet(b));
    if a_pq && b_pq {
        // `requested`, not `engine`: `auto` has already been resolved to
        // `Turbo` for any Parquet input by this point, so testing the resolved
        // value here would send every pair to `turbo` and quietly retire the
        // columnar path. Only an explicit `--engine turbo` should do that.
        if requested == Engine::Turbo {
            return turbo::compare(a, b, opt)
                .map(|r| (r, Engine::Turbo.label().to_string()))
                .map_err(|e| e.context("the turbo engine failed: "));
        }
        // The columnar path reads a narrow slice of Parquet on purpose -- PLAIN
        // and dictionary v1 pages of byte arrays and word-sized types -- and
        // everything outside it used to come back as a flat refusal, from a
        // binary carrying a second reader that handles more. A real
        // file (NYC TLC trip data, zstd) was turned away by it, and naming
        // `--engine turbo` did not help because this branch ran first. So a
        // refusal on capability grounds -- and only that, never a corrupt file
        // or a missing column -- falls through to `turbo` instead.
        match pqdiff::compare(a, b, opt) {
            Ok(result) => return Ok((result, "parquet".to_string())),
            Err(e) if e.is_unsupported() => {
                return turbo::compare(a, b, opt)
                    .map(|r| (r, Engine::Turbo.label().to_string()))
                    .map_err(|turbo_err| {
                        // Both refusals: the first says what the fast path could
                        // not read, the second what actually stopped the run.
                        Error::new(format!(
                            "the parquet engine failed: {e}; and the turbo \
engine, which reads more of the format, failed too: {turbo_err}"
                        ))
                    });
            }
            Err(e) => return Err(Error::new(format!("the parquet engine failed: {e}"))),
        }
    }
    if a_pq || b_pq {
        return turbo::compare(a, b, opt)
            .map(|r| (r, Engine::Turbo.label().to_string()))
            .map_err(|e| e.context("the turbo engine failed: "));
    }

    // Below this, the sortmerge engine's own buffers and threads don't fit
    // and the process would abort mid-run (measured: 12 MB here, 16 MB on a
    // runner with more threads): a named sortmerge is refused up front.
    if engine == Engine::SortMerge
        && let Some(heap) = crate::memory::heap_limit().filter(|h| *h < SORTMERGE_MIN)
    {
        return Err(Error::new(format!(
            "the sortmerge engine needs about {} of heap for its batches, run buffers and \
             threads, and this process may use {} (a container limit or `ulimit -d`): \
             raise it",
            crate::source::human(SORTMERGE_MIN),
            crate::source::human(heap)
        )));
    }
    let result = match engine {
        Engine::Turbo => turbo::compare(a, b, opt),
        Engine::SortMerge => sortmerge::compare(a, b, opt),
        Engine::Native => native::compare(a, b, opt),
        Engine::Auto => unreachable!("auto is resolved before this point"),
    };
    match result {
        // `turbo` holds its indexes in memory; a CSV pair whose indexes do not
        // fit what this process may have -- a container's limit, or
        // `--memory` -- is compared by the engine that sorts and spills to
        // disk instead, at a few times the time and a bounded footprint. Only
        // under `auto`: a named engine is the one that answers. And only with
        // room for that engine's own buffers -- it needs some tens of
        // megabytes whatever the input -- or it would fail where the refusal
        // said plainly what was missing.
        Err(e)
            if requested == Engine::Auto
                && engine == Engine::Turbo
                && e.is_out_of_memory()
                && crate::memory::heap_limit().is_none_or(|h| h >= SORTMERGE_FLOOR)
                && sortmerge::available()
                && !turbo::only_this_engine_reads(a)
                && !turbo::only_this_engine_reads(b) =>
        {
            eprintln!(
                "note: {e}, more than this process may use; comparing with the sortmerge \
                 engine, which spills to disk ({})",
                std::env::temp_dir().display()
            );
            crate::memory::release_free();
            sortmerge::compare(a, b, opt)
                .map(|r| (r, Engine::SortMerge.label().to_string()))
                .map_err(|e| e.context("the sortmerge engine failed: "))
        }
        // A pair that didn't fit `turbo`, in a heap too small to hand it to
        // `sortmerge` with room to spare: say that the engine can be named.
        Err(e)
            if requested == Engine::Auto
                && engine == Engine::Turbo
                && e.is_out_of_memory()
                && crate::memory::heap_limit()
                    .is_some_and(|h| (SORTMERGE_MIN..SORTMERGE_FLOOR).contains(&h))
                && sortmerge::available() =>
        {
            Err(e
                .context(format!("the {engine} engine failed: "))
                .context_after(format!(
                    "; below {} of heap the sortmerge engine isn't tried on its own (it may not \
                 fit): `--engine sortmerge` tries it",
                    crate::source::human(SORTMERGE_FLOOR)
                )))
        }
        result => result
            .map(|r| (r, engine.label().to_string()))
            .map_err(|e| e.context(format!("the {engine} engine failed: "))),
    }
}

/// The least heap a named sortmerge is started with; see [`SORTMERGE_FLOOR`]
/// for the fallback's, which leaves room for larger inputs.
const SORTMERGE_MIN: u64 = 24 << 20;

/// The heap below which a CSV pair that does not fit `turbo` is refused
/// rather than handed to `sortmerge`: about what that engine needs for its
/// batches, run buffers and merge on a 2M-row pair (measured: 44 MB runs,
/// 30 MB aborts), with room. An 80 MB container's heap is above it.
const SORTMERGE_FLOOR: u64 = 48 << 20;

/// Turns [`Engine::Auto`] into a concrete backend.
///
/// `turbo` is preferred: it is the byte-level engine every benchmark here
/// measures, and the only one that reads Parquet and newline-delimited JSON.
pub fn resolve_engine(requested: Engine) -> Engine {
    if requested != Engine::Auto {
        return requested;
    }
    Engine::CONCRETE
        .into_iter()
        .find(|&candidate| available(candidate))
        .unwrap_or(Engine::Native)
}

/// Whether a backend can run in this build.
pub fn available(engine: Engine) -> bool {
    match engine {
        Engine::Turbo => turbo::available(),
        Engine::SortMerge => sortmerge::available(),
        Engine::Native => native::available(),
        Engine::Auto => false,
    }
}

fn file_meta(path: &Path) -> FileMeta {
    FileMeta {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned(),
        size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    }
}
