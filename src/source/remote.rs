//! Object storage and HTTP inputs (feature `cloud`).
//!
//! `s3://`, `gs://` and `az://` (with `adl`, `azure`, `abfs`, `abfss`) go
//! through Apache Arrow's `object_store`, each configured from its own
//! environment the way that cloud's own tools are: `AWS_*` (keys, session
//! token, region, `AWS_ENDPOINT_URL` for MinIO and the like, web identity for
//! EKS, the ECS task role, instance metadata) and a profile in
//! `~/.aws/credentials`; `GOOGLE_*` (a service account file, application
//! default credentials, the metadata server); `AZURE_*` (an account key, a SAS
//! token, a service principal, managed or workload identity, Azurite).
//! `http://` and `https://` are plain GETs, query string and all, so a
//! presigned S3 URL or an Azure SAS URL reads as it is; one that is refused
//! and names a bucket or a storage account is tried again signed.
//!
//! Everything is read in ranges of [`PART`] bytes, [`CONCURRENCY`] at a time
//! (`COMPARA_FETCH_CONCURRENCY`), and written where it belongs in a temporary
//! file as it arrives, so memory holds a few buffers whatever the object's
//! size. A Parquet object is read as far as the comparison reads it: the
//! first request is for its last 64 KB, which holds the footer of any file
//! not unusually wide; the footer names the column chunks; and only the
//! chunks of the columns compared are fetched. The rest of the local file is
//! a hole, which no reader here touches.

use std::fs::File;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::{StreamExt, TryStreamExt};
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, path::Path as ObjectPath};

use super::delta;
use super::many::{self, Combined, read_at, write_at};
use super::{Source, Want, human, room_for, temp_file};
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Bytes per ranged request.
const PART: u64 = 8 << 20;
/// Ranged requests in flight at once.
const CONCURRENCY: usize = 16;
/// The first request: the end of the object, where a Parquet footer is.
const TAIL: u64 = 64 << 10;
/// Two wanted ranges closer than this are fetched as one.
const GAP: u64 = 256 << 10;

pub(super) fn fetch(spec: &str, want: &Want) -> Result<Source> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| Error::new(format!("cannot start the download runtime: {e}")))?;
    runtime.block_on(fetch_async(spec, want))
}

/// How the bytes are asked for.
enum Reader {
    Store(Arc<dyn ObjectStore>, ObjectPath),
    Http(reqwest::Client, String),
}

async fn fetch_async(spec: &str, want: &Want) -> Result<Source> {
    let started = Instant::now();
    let url = url::Url::parse(spec).map_err(|e| Error::new(format!("{spec}: not a URL: {e}")))?;
    let scheme = url.scheme().to_ascii_lowercase();
    let store_scheme = scheme != "http" && scheme != "https";
    if store_scheme
        && let Some((_, key)) = split_key(spec)
        && (key.is_empty()
            || key.ends_with('/')
            || many::is_pattern(key)
            || delta::split_version(spec).1.is_some())
    {
        return fetch_many(spec, want).await;
    }
    let mut reader = if scheme == "http" || scheme == "https" {
        if let Some(why) = metadata_url(&url).filter(|_| !link_local_allowed()) {
            return Err(Error::new(why));
        }
        Reader::Http(http_client()?, spec.to_string())
    } else {
        store_for(spec, &url).await?
    };

    // The end of the object first: its size comes with it, and for Parquet
    // the footer.
    let mut public = false;
    let got = match tail(&reader, spec).await {
        Ok(found) => found,
        Err(Refused(status)) if signable(&url) && matches!(reader, Reader::Http(..)) => {
            // A plain GET of a private object: sign it as its cloud would.
            reader = store_for(spec, &url).await.map_err(|e| {
                Error::new(format!(
                    "{spec}: HTTP {status} unsigned, and signing it failed: {e}"
                ))
            })?;
            tail(&reader, spec).await.map_err(|e| e.into_error(spec))?
        }
        // No credentials anywhere: a public bucket reads unsigned.
        Err(FetchError::Other(message)) if store_scheme && no_credentials(&message) => {
            let unsigned = store_signed(spec, &url, false).await?;
            match tail(&unsigned, spec).await {
                Ok(found) => {
                    reader = unsigned;
                    public = true;
                    found
                }
                Err(_) => return Err(Error::new(format!("{spec}: {message}"))),
            }
        }
        // No such object, but perhaps a prefix: `s3://bucket/orders` for the
        // parts under `orders/`.
        Err(Refused(404)) if store_scheme && !spec.ends_with('/') => {
            let prefix = format!("{spec}/");
            return match fetch_many(&prefix, want).await {
                Ok(source) => Ok(source),
                Err(e) if e.to_string().starts_with("no objects") => {
                    Err(Error::new(format!("not found: {spec}")))
                }
                Err(e) => Err(e),
            };
        }
        Err(e) => return Err(e.into_error(spec)),
    };
    let (size, tail, etag) = match got {
        Got::Tail(size, tail, etag) => (size, tail, etag),
        Got::Whole(path, size) => {
            let secs = started.elapsed().as_secs_f64();
            return Ok(Source {
                spec: spec.to_string(),
                path: path.clone(),
                stored: size,
                temps: vec![path],
                steps: vec![format!(
                    "downloaded {} whole (the server sends no ranges) in {secs:.1}s ({}/s)",
                    human(size),
                    human((size as f64 / secs.max(1e-3)) as u64)
                )],
            });
        }
    };

    let (path, file) = temp_file("download")?;
    let mut source = Source {
        spec: spec.to_string(),
        path: path.clone(),
        stored: size,
        temps: vec![path],
        steps: Vec::new(),
    };
    file.set_len(size)?;
    let tail_at = size - tail.len() as u64;
    write_at(&file, &tail, tail_at)?;

    let mut note = String::new();
    let mut ranges: Vec<Range<u64>> = Vec::new();
    let parquet = size >= 12 && tail.ends_with(b"PAR1");
    if parquet {
        let footer_len =
            u32::from_le_bytes(tail[tail.len() - 8..tail.len() - 4].try_into().unwrap()) as u64;
        let footer_at = size.checked_sub(8 + footer_len).ok_or_else(|| {
            Error::new(format!(
                "{spec}: the parquet footer is longer than the file"
            ))
        })?;
        if footer_at < tail_at {
            fetch_ranges(
                &reader,
                &file,
                std::iter::once(footer_at..tail_at).collect(),
                etag.as_deref(),
                spec,
            )
            .await?;
        }
        let mut footer = vec![0u8; (size - footer_at) as usize];
        read_at(&file, &mut footer, footer_at)?;
        let (names, chunks) = crate::parquet::chunk_ranges(&footer, &|n| want.needs(n))
            .map_err(|e| e.context(format!("{spec}: ")))?;
        let wanted = names.iter().filter(|n| want.needs(n)).count();
        ranges.push(0..4.min(tail_at));
        ranges.extend(chunks);
        note = format!(", {wanted} of {} columns", names.len());
    } else if tail_at > 0 {
        ranges.push(0..tail_at);
    }
    let ranges = coalesce(ranges, tail_at);
    let fetched: u64 = ranges.iter().map(|r| r.end - r.start).sum::<u64>() + tail.len() as u64;
    room_for(fetched, spec)?;
    fetch_ranges(&reader, &file, ranges, etag.as_deref(), spec).await?;
    file.sync_data().ok();
    let secs = started.elapsed().as_secs_f64();
    if public {
        note.push_str(", unsigned (no credentials here; a public bucket)");
    }
    source.steps.push(format!(
        "downloaded {} of {}{note} in {secs:.1}s ({}/s)",
        human(fetched),
        human(size),
        human((fetched as f64 / secs.max(1e-3)) as u64)
    ));
    Ok(source)
}

/// `scheme://bucket/` and the key after it, as the spec spells them: a glob
/// is matched against keys as written, not as a URL parser would read `?`.
fn split_key(spec: &str) -> Option<(&str, &str)> {
    let (_, rest) = spec.split_once("://")?;
    let slash = rest.find('/')?;
    let root_len = spec.len() - rest.len() + slash + 1;
    Some((&spec[..root_len], &spec[root_len..]))
}

/// Whether an object's name says it is compressed: it is then downloaded and
/// expanded whole, rather than read in place.
fn named_compressed(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [".gz", ".gzip", ".zst", ".zstd", ".bz2", ".xz", ".zip"]
        .iter()
        .any(|e| k.ends_with(e))
}

/// One object of several.
struct Part {
    name: String,
    reader: Reader,
    size: u64,
    tail: Vec<u8>,
    etag: Option<String>,
}

/// Every object under a prefix, or matching a glob, read as one table (see
/// [`many`]): listed, sorted by key, the end of each fetched, and then each
/// fetched into its place in one local file -- for Parquet only as far as the
/// comparison reads it, as for one object.
async fn fetch_many(spec: &str, want: &Want) -> Result<Source> {
    let started = Instant::now();
    // `table@v12`: a version of a Delta table, listed as its directory.
    let (table, version) = delta::split_version(spec);
    let as_dir;
    let (root, key) = {
        let (root, key) =
            split_key(table).ok_or_else(|| Error::new(format!("{spec}: no bucket in the URL")))?;
        if version.is_some() && !key.is_empty() && !key.ends_with('/') {
            as_dir = format!("{key}/");
            (root, as_dir.as_str())
        } else {
            (root, key)
        }
    };
    let pattern = many::is_pattern(key).then_some(key);
    let prefix = match pattern {
        Some(p) => many::literal_prefix(p),
        None => key,
    };
    let url = url::Url::parse(root).map_err(|e| Error::new(format!("{spec}: not a URL: {e}")))?;
    let trimmed = prefix.trim_end_matches('/');
    let list = |store: Arc<dyn ObjectStore>| async move {
        if trimmed.is_empty() {
            store.list(None).try_collect::<Vec<_>>().await
        } else {
            store
                .list(Some(&ObjectPath::from(trimmed)))
                .try_collect()
                .await
        }
    };
    let Reader::Store(mut store, _) = store_for(root, &url).await? else {
        unreachable!("store_for returns a store");
    };
    let mut public = false;
    let listed: Vec<object_store::ObjectMeta> = match list(store.clone()).await {
        Ok(listed) => listed,
        Err(e) => {
            let message = explain(&e.to_string());
            // No credentials anywhere: a public bucket lists unsigned.
            let retried = if no_credentials(&message) {
                let Reader::Store(unsigned, _) = store_signed(root, &url, false).await? else {
                    unreachable!("store_signed returns a store");
                };
                list(unsigned.clone()).await.ok().map(|l| (unsigned, l))
            } else {
                None
            };
            let Some((unsigned, listed)) = retried else {
                return Err(Error::new(format!("cannot list {spec}: {message}")));
            };
            store = unsigned;
            public = true;
            listed
        }
    };
    let relative = |m: &object_store::ObjectMeta| -> String {
        let loc = m.location.as_ref();
        loc.strip_prefix(trimmed)
            .unwrap_or(loc)
            .trim_start_matches('/')
            .to_string()
    };
    let mut delta_note = String::new();
    let mut delta_files: Option<BTreeSet<String>> = None;
    if pattern.is_none() {
        let names: Vec<String> = listed.iter().map(relative).collect();
        if delta::is_delta(names.iter().map(String::as_str)) {
            // A Delta table: its log says which files are this version.
            let log: Vec<String> = names
                .iter()
                .filter_map(|n| n.strip_prefix("_delta_log/"))
                .filter(|n| !n.contains('/'))
                .map(str::to_string)
                .collect();
            let plan = delta::plan(&log, version, spec)?;
            let wanted: Vec<String> = plan
                .checkpoint
                .iter()
                .chain(&plan.commits)
                .cloned()
                .collect();
            let fetched: Vec<(String, Vec<u8>)> =
                futures::stream::iter(wanted.into_iter().map(|n| {
                    let store = store.clone();
                    let path = ObjectPath::from(
                        format!("{trimmed}/_delta_log/{n}").trim_start_matches('/'),
                    );
                    async move {
                        let fail = |e: object_store::Error| {
                            Error::new(format!("{spec}: cannot read the log ({n}): {e}"))
                        };
                        let got = store.get(&path).await.map_err(fail)?;
                        let bytes = got.bytes().await.map_err(fail)?;
                        Ok::<_, Error>((n, bytes.to_vec()))
                    }
                }))
                .buffered(concurrency())
                .try_collect()
                .await?;
            let files: BTreeMap<String, Vec<u8>> = fetched.into_iter().collect();
            let mut read = |n: &str| -> Result<Vec<u8>> {
                files
                    .get(n)
                    .cloned()
                    .ok_or_else(|| Error::new(format!("{spec}: {n} was not read")))
            };
            let set: BTreeSet<String> =
                delta::replay(&plan, &mut read, spec)?.into_iter().collect();
            let present: BTreeSet<String> = names.iter().cloned().collect();
            if let Some(gone) = set.iter().find(|f| !present.contains(*f)) {
                return Err(Error::new(format!(
                    "{spec}: version {} names {gone}, which is no longer there (vacuumed?)",
                    plan.version
                )));
            }
            delta_note = format!(", Delta version {}", plan.version);
            delta_files = Some(set);
        } else if version.is_some() {
            return Err(Error::new(format!(
                "{spec}: @v{} names a version of a Delta table, and {table} is not one",
                version.unwrap_or_default()
            )));
        } else if let Some(found) = many::table_format(names.iter().map(String::as_str)) {
            return Err(many::refuse_table(spec, &found));
        }
    }
    let mut objects: Vec<object_store::ObjectMeta> = listed
        .into_iter()
        .filter(|m| {
            m.size > 0
                && match (&delta_files, pattern) {
                    (Some(set), _) => set.contains(&relative(m)),
                    (None, Some(p)) => many::glob_matches(p, m.location.as_ref()),
                    (None, None) => !many::hidden(&relative(m)),
                }
        })
        .collect();
    if objects.is_empty() {
        return Err(Error::new(match pattern {
            Some(_) => format!("no objects match {spec}"),
            None => format!("no objects under {spec}"),
        }));
    }
    objects.sort_by(|a, b| a.location.as_ref().cmp(b.location.as_ref()));
    let listed_bytes: u64 = objects.iter().map(|m| m.size).sum();

    let compressed = objects
        .iter()
        .any(|m| named_compressed(m.location.as_ref()));
    let mut combined = Combined::new()?;
    let mut source = Source {
        spec: spec.to_string(),
        path: combined.path.clone(),
        stored: listed_bytes,
        temps: vec![combined.path.clone()],
        steps: Vec::new(),
    };
    let label = |m: &object_store::ObjectMeta| format!("{root}{}", m.location.as_ref());

    let relative_names: Vec<String> = objects.iter().map(relative).collect();
    let partitions = many::table_partitions(&relative_names);
    if let Some((names, _)) = &partitions {
        combined.set_partitions(names.clone());
    }
    let values_of =
        |i: usize| -> Vec<String> { partitions.as_ref().map_or(Vec::new(), |p| p.1[i].clone()) };
    let whole: Vec<(String, ObjectPath, Vec<String>)> = objects
        .iter()
        .enumerate()
        .map(|(i, m)| (label(m), m.location.clone(), values_of(i)))
        .collect();

    if compressed {
        append_whole(&store, &whole, &mut combined).await?;
    } else {
        // The end of every object first: sizes, versions, Parquet footers.
        let parts: Vec<Part> = futures::stream::iter(objects.iter().map(|m| {
            let reader = Reader::Store(store.clone(), m.location.clone());
            let name = label(m);
            async move {
                let (size, tail, etag) = tail(&reader, &name)
                    .await
                    .map_err(|e| e.into_error(&name))?
                    .ranged(&name)?;
                Ok::<_, Error>(Part {
                    name,
                    reader,
                    size,
                    tail,
                    etag,
                })
            }
        }))
        .buffered(concurrency())
        .try_collect()
        .await?;
        let parquet = parts[0].size >= 12 && parts[0].tail.ends_with(b"PAR1");
        if !parquet && combined.has_partitions() {
            // Text gets its partition values written into every row, so it
            // is downloaded whole and rewritten into place rather than read
            // in place.
            drop(parts);
            append_whole(&store, &whole, &mut combined).await?;
            let size = combined.finish()?;
            let _ = size;
            source.steps.push(format!(
                "downloaded {} in {} objects in {:.1}s",
                human(listed_bytes),
                whole.len(),
                started.elapsed().as_secs_f64()
            ));
            source
                .steps
                .push(format!("{}{delta_note}", combined.note()));
            return Ok(source);
        }
        if let Some(odd) = parts
            .iter()
            .find(|p| (p.size >= 12 && p.tail.ends_with(b"PAR1")) != parquet)
        {
            return Err(Error::new(format!(
                "{} is {} but {} is {}; the parts of one side must be of one format",
                odd.name,
                if parquet { "not Parquet" } else { "Parquet" },
                parts[0].name,
                if parquet { "Parquet" } else { "not" }
            )));
        }
        // Text for `columns` needs the header, which the first part has.
        let parts = if !parquet && want.footer_only {
            &parts[..1]
        } else {
            &parts[..]
        };
        let mut bases = Vec::with_capacity(parts.len());
        for p in parts {
            bases.push(combined.reserve(p.size + u64::from(!parquet)));
        }
        combined.file.set_len(combined.end())?;
        for (p, &base) in parts.iter().zip(&bases) {
            write_at(&combined.file, &p.tail, base + p.size - p.tail.len() as u64)?;
        }
        let mut jobs: Vec<Job> = Vec::new();
        let mut columns = String::new();
        if parquet {
            // A footer longer than the tail first.
            let mut footers: Vec<Job> = Vec::new();
            let mut footer_at = Vec::with_capacity(parts.len());
            for (p, &base) in parts.iter().zip(&bases) {
                let t = &p.tail;
                let len =
                    u32::from_le_bytes(t[t.len() - 8..t.len() - 4].try_into().unwrap()) as u64;
                let at = p
                    .size
                    .checked_sub(8 + len)
                    .filter(|&a| a >= 4)
                    .ok_or_else(|| {
                        Error::new(format!(
                            "{}: the parquet footer is longer than the file",
                            p.name
                        ))
                    })?;
                let tail_at = p.size - t.len() as u64;
                if at < tail_at {
                    footers.push(Job {
                        reader: &p.reader,
                        range: at..tail_at,
                        shift: base,
                        etag: p.etag.as_deref(),
                        spec: &p.name,
                    });
                }
                footer_at.push(at);
                write_at(&combined.file, b"PAR1", base)?;
            }
            fetch_jobs(footers, &combined.file, spec).await?;
            let mut wanted = 0;
            for ((p, &base), &at) in parts.iter().zip(&bases).zip(&footer_at) {
                let mut footer = vec![0u8; (p.size - at) as usize];
                read_at(&combined.file, &mut footer, base + at)?;
                let (names, chunks) = crate::parquet::chunk_ranges(&footer, &|n| want.needs(n))
                    .map_err(|e| e.context(format!("{}: ", p.name)))?;
                if columns.is_empty() {
                    wanted = names.iter().filter(|n| want.needs(n)).count();
                    columns = format!(", {wanted} of {} columns", names.len());
                }
                for range in coalesce(chunks, p.size - p.tail.len() as u64) {
                    jobs.push(Job {
                        reader: &p.reader,
                        range,
                        shift: base,
                        etag: p.etag.as_deref(),
                        spec: &p.name,
                    });
                }
            }
            let _ = wanted;
        } else {
            for (p, &base) in parts.iter().zip(&bases) {
                let rest = p.size - p.tail.len() as u64;
                if rest > 0 {
                    jobs.push(Job {
                        reader: &p.reader,
                        range: 0..rest,
                        shift: base,
                        etag: p.etag.as_deref(),
                        spec: &p.name,
                    });
                }
            }
        }
        let fetched: u64 = jobs
            .iter()
            .map(|j| j.range.end - j.range.start)
            .sum::<u64>()
            + parts.iter().map(|p| p.tail.len() as u64).sum::<u64>();
        room_for(fetched, spec)?;
        fetch_jobs(jobs, &combined.file, spec).await?;
        for (i, (p, &base)) in parts.iter().zip(&bases).enumerate() {
            combined.placed(&p.name, base, p.size, !parquet, &values_of(i))?;
        }
        let secs = started.elapsed().as_secs_f64();
        if public {
            columns.push_str(", unsigned (no credentials here; a public bucket)");
        }
        source.steps.push(format!(
            "downloaded {} of {} in {} objects{columns} in {secs:.1}s ({}/s)",
            human(fetched),
            human(listed_bytes),
            parts.len(),
            human((fetched as f64 / secs.max(1e-3)) as u64)
        ));
    }
    let size = combined.finish()?;
    let _ = size;
    source
        .steps
        .push(format!("{}{delta_note}", combined.note()));
    Ok(source)
}

/// Downloads each object whole, four at a time, and appends it to `combined`
/// in order -- expanded when compressed, its partition values added when
/// text -- each download deleted once it is in.
async fn append_whole(
    store: &Arc<dyn ObjectStore>,
    objects: &[(String, ObjectPath, Vec<String>)],
    combined: &mut Combined,
) -> Result<()> {
    for group in objects.chunks(4) {
        let done = futures::future::try_join_all(group.iter().map(|(name, location, values)| {
            let reader = Reader::Store(store.clone(), location.clone());
            async move {
                let (size, tail, etag) = tail(&reader, name)
                    .await
                    .map_err(|e| e.into_error(name))?
                    .ranged(name)?;
                let (path, file) = temp_file("part")?;
                let guard = TempPath(path);
                file.set_len(size)?;
                write_at(&file, &tail, size - tail.len() as u64)?;
                let rest = size - tail.len() as u64;
                if rest > 0 {
                    fetch_ranges(
                        &reader,
                        &file,
                        std::iter::once(0..rest).collect(),
                        etag.as_deref(),
                        name,
                    )
                    .await?;
                }
                Ok::<_, Error>((name, guard, values))
            }
        }))
        .await?;
        for (name, temp, values) in done {
            combined.append_file(name, &temp.0, values)?;
        }
    }
    Ok(())
}

/// A temporary file removed when this goes.
struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes the local file `from` to `dest`: in one request when it is small,
/// as a multipart upload of 16 MB parts, eight in flight, when it is not. A
/// plain `https://` URL -- a presigned PUT -- takes the whole file in one PUT.
pub(super) fn upload(from: &std::path::Path, dest: &str) -> Result<u64> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| Error::new(format!("cannot start the upload runtime: {e}")))?;
    runtime.block_on(upload_async(from, dest))
}

async fn upload_async(from: &std::path::Path, dest: &str) -> Result<u64> {
    use std::io::Read;
    // Overridable so the multipart path can be tested on a small file.
    let env_size = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(default)
    };
    let one_shot = env_size("COMPARA_UPLOAD_ONE_SHOT", 32 << 20);
    let chunk = env_size("COMPARA_UPLOAD_PART", 16 << 20) as usize;
    let url = url::Url::parse(dest).map_err(|e| Error::new(format!("{dest}: not a URL: {e}")))?;
    let size = std::fs::metadata(from)?.len();
    let plain = matches!(url.scheme(), "http" | "https") && !signable(&url);
    let fail = |e: String| Error::new(format!("cannot write {dest}: {e}"));
    if plain {
        let body = std::fs::read(from)?;
        let response = http_client()?
            .put(dest)
            .body(body)
            .send()
            .await
            .map_err(|e| fail(said(&e)))?;
        if !response.status().is_success() {
            return Err(fail(format!("HTTP {}", response.status())));
        }
        return Ok(size);
    }
    let Reader::Store(store, path) = store_for(dest, &url).await? else {
        unreachable!("store_for returns a store");
    };
    if size <= one_shot {
        let bytes = std::fs::read(from)?;
        store
            .put(&path, bytes.into())
            .await
            .map_err(|e| fail(explain(&e.to_string())))?;
        return Ok(size);
    }
    let upload = store
        .put_multipart(&path)
        .await
        .map_err(|e| fail(explain(&e.to_string())))?;
    let mut writer = object_store::WriteMultipart::new_with_chunk_size(upload, chunk);
    let mut file = std::fs::File::open(from)?;
    let mut buf = vec![0u8; chunk];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer
            .wait_for_capacity(8)
            .await
            .map_err(|e| fail(e.to_string()))?;
        writer.write(&buf[..n]);
    }
    writer.finish().await.map_err(|e| fail(e.to_string()))?;
    Ok(size)
}

/// Why a request failed, kept apart so a refused unsigned GET can be retried.
/// What a request for the end of an object got.
enum Got {
    /// Its size, its last bytes, its strong validator.
    Tail(u64, Vec<u8>, Option<String>),
    /// A server that doesn't do ranges sent the whole object: already
    /// written to this temporary file, of this size.
    Whole(PathBuf, u64),
}

impl Got {
    /// The end of an object from a store, which always takes ranges.
    fn ranged(self, spec: &str) -> Result<(u64, Vec<u8>, Option<String>)> {
        match self {
            Got::Tail(size, tail, etag) => Ok((size, tail, etag)),
            Got::Whole(path, ..) => {
                let _ = std::fs::remove_file(path);
                Err(Error::new(format!(
                    "{spec}: the store sent the whole object for a range"
                )))
            }
        }
    }
}

enum FetchError {
    Refused(u16),
    /// Worth another try: a dropped connection, a 5xx, a 429.
    Transient(String),
    Other(String),
}
use FetchError::Refused;

impl FetchError {
    fn into_error(self, spec: &str) -> Error {
        match self {
            Refused(404) => Error::new(format!("not found: {spec}")),
            Refused(status) => Error::new(format!("{spec}: HTTP {status}")),
            FetchError::Transient(e) | FetchError::Other(e) => Error::new(format!("{spec}: {e}")),
        }
    }
}

impl From<object_store::Error> for FetchError {
    fn from(e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => Refused(404),
            object_store::Error::Precondition { .. } => FetchError::Other(changed()),
            e => FetchError::Other(explain(&e.to_string())),
        }
    }
}

/// A store error, with what to do about the common one: no credentials.
fn explain(e: &str) -> String {
    let lower = e.to_ascii_lowercase();
    if let Some(trouble) = PROFILE_TROUBLE.get()
        && (lower.contains("credential") || lower.contains("metadata"))
    {
        return format!("{e}. No usable credentials: {trouble}");
    }
    if lower.contains("credential") || lower.contains("metadata") || lower.contains("token") {
        format!(
            "{e}. No usable credentials: set the cloud's usual variables (AWS_ACCESS_KEY_ID and \
             AWS_SECRET_ACCESS_KEY or AWS_PROFILE; GOOGLE_APPLICATION_CREDENTIALS; \
             AZURE_STORAGE_ACCOUNT_KEY or AZURE_STORAGE_SAS_KEY), run where a role is attached, \
             or for a public bucket set AWS_SKIP_SIGNATURE=true"
        )
    } else {
        e.to_string()
    }
}

/// The object's size and its last [`TAIL`] bytes (all of it, if smaller). A
/// plain HTTP server that answers 5xx or 429, or drops the connection, is
/// asked again, three times, a little later each time, as a range is.
async fn tail(reader: &Reader, spec: &str) -> std::result::Result<Got, FetchError> {
    let mut attempt = 0u32;
    loop {
        let result = tail_once(reader, spec).await;
        let retryable = match &result {
            Err(FetchError::Transient(_)) => true,
            Err(Refused(status)) => *status >= 500 || *status == 429,
            _ => false,
        };
        if !retryable || attempt >= 3 || !matches!(reader, Reader::Http(..)) {
            return result.map_err(|e| match e {
                FetchError::Transient(m) => FetchError::Other(m),
                e => e,
            });
        }
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_millis(250 << attempt)).await;
    }
}

async fn tail_once(reader: &Reader, spec: &str) -> std::result::Result<Got, FetchError> {
    match reader {
        Reader::Store(store, path) => {
            let got = store
                .get_opts(
                    path,
                    GetOptions {
                        range: Some(GetRange::Suffix(TAIL)),
                        ..Default::default()
                    },
                )
                .await;
            let got = match got {
                Ok(g) => g,
                // Azure takes no suffix range: the size first, then the end.
                Err(object_store::Error::NotSupported { .. }) => {
                    let meta = store.head(path).await?;
                    if meta.size == 0 {
                        return Ok(Got::Tail(0, Vec::new(), meta.e_tag));
                    }
                    let from = meta.size.saturating_sub(TAIL);
                    store
                        .get_opts(
                            path,
                            GetOptions {
                                range: Some(GetRange::Bounded(from..meta.size)),
                                if_match: meta.e_tag.clone(),
                                ..Default::default()
                            },
                        )
                        .await?
                }
                // An empty object has no suffix to give.
                Err(e) => {
                    let meta = store.head(path).await;
                    match meta {
                        Ok(m) if m.size == 0 => return Ok(Got::Tail(0, Vec::new(), m.e_tag)),
                        _ => return Err(e.into()),
                    }
                }
            };
            let size = got.meta.size;
            let etag = got.meta.e_tag.clone();
            let bytes = got.bytes().await?;
            Ok(Got::Tail(size, bytes.to_vec(), etag))
        }
        Reader::Http(client, url) => {
            let response = client
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes=-{TAIL}"))
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        FetchError::Transient(said(&e))
                    } else {
                        FetchError::Other(said(&e))
                    }
                })?;
            let status = response.status();
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                // An empty body: nothing to take the end of.
                return Ok(Got::Tail(0, Vec::new(), None));
            }
            // A strong validator only: a weak one does not promise the bytes.
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.starts_with("W/"))
                .map(str::to_string);
            if !status.is_success() {
                return Err(Refused(status.as_u16()));
            }
            if status == reqwest::StatusCode::PARTIAL_CONTENT {
                let total = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| {
                        FetchError::Other(format!(
                            "{spec}: a partial response without a total size"
                        ))
                    })?;
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| FetchError::Other(e.to_string()))?;
                return Ok(Got::Tail(total, bytes.to_vec(), etag));
            }
            // The server sent the whole object: it does not do ranges, so this
            // response is the download. It goes to disk as it comes, never
            // whole into memory (a 1 GB object aborted the process), with
            // the temporary directory's room checked first and as it grows.
            let other = |e: Error| FetchError::Other(e.to_string());
            const STEP: u64 = 64 << 20;
            if let Some(len) = response.content_length() {
                room_for(len, spec).map_err(other)?;
            }
            let (path, mut file) = temp_file("download").map_err(other)?;
            let mut written = 0u64;
            let mut checked = 0u64;
            let mut response = response;
            let result: std::result::Result<(), FetchError> = async {
                while let Some(chunk) = response.chunk().await.map_err(|e| {
                    if e.is_timeout() || e.is_body() {
                        FetchError::Transient(e.to_string())
                    } else {
                        FetchError::Other(e.to_string())
                    }
                })? {
                    std::io::Write::write_all(&mut file, &chunk)
                        .map_err(|e| FetchError::Other(format!("{spec}: {e}")))?;
                    written += chunk.len() as u64;
                    if written >= checked + STEP {
                        room_for(STEP, spec).map_err(other)?;
                        checked = written;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            Ok(Got::Whole(path, written))
        }
    }
}

/// Fetches `ranges` into `file` at their offsets, in parts, several at a time.
async fn fetch_ranges(
    reader: &Reader,
    file: &File,
    ranges: Vec<Range<u64>>,
    etag: Option<&str>,
    spec: &str,
) -> Result<()> {
    let jobs = ranges
        .into_iter()
        .map(|range| Job {
            reader,
            range,
            shift: 0,
            etag,
            spec,
        })
        .collect();
    fetch_jobs(jobs, file, spec).await
}

/// A range of one object, written to the local file `shift` bytes further on
/// than it sits in the object.
struct Job<'a> {
    reader: &'a Reader,
    range: Range<u64>,
    shift: u64,
    etag: Option<&'a str>,
    spec: &'a str,
}

fn concurrency() -> usize {
    std::env::var("COMPARA_FETCH_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(CONCURRENCY)
}

/// Fetches every job into `file`, cut into parts of at most [`PART`] bytes,
/// [`concurrency`] at a time across all of them: many small objects keep as
/// many requests in flight as one large one.
async fn fetch_jobs(jobs: Vec<Job<'_>>, file: &File, label: &str) -> Result<()> {
    let parts: Vec<(&Job, Range<u64>)> = jobs
        .iter()
        .flat_map(|job| {
            let mut out = Vec::new();
            let mut at = job.range.start;
            while at < job.range.end {
                let end = (at + PART).min(job.range.end);
                out.push((job, at..end));
                at = end;
            }
            out
        })
        .collect();
    let total: u64 = parts.iter().map(|(_, r)| r.end - r.start).sum();
    let done = std::sync::atomic::AtomicU64::new(0);
    let done = &done;
    let work = futures::stream::iter(parts)
        .map(|(job, part)| async move {
            let len = part.end - part.start;
            fetch_part(job.reader, file, part, job.shift, job.etag)
                .await
                .map_err(|e| e.into_error(job.spec))?;
            done.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
            Ok::<(), Error>(())
        })
        .buffer_unordered(concurrency())
        .try_collect::<Vec<()>>();
    // A long download says how far it has got, every so often, so a log --
    // a CI job's, a pod's -- shows it moving.
    let started = Instant::now();
    let every = std::env::var("COMPARA_PROGRESS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(PROGRESS_EVERY_SECS);
    let ticker = async {
        if every <= 0.0 {
            return std::future::pending::<()>().await;
        }
        let mut every = tokio::time::interval(std::time::Duration::from_secs_f64(every));
        every.tick().await;
        loop {
            every.tick().await;
            let got = done.load(std::sync::atomic::Ordering::Relaxed);
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "{label}: {} of {} ({}%) in {secs:.0}s, {}/s",
                human(got),
                human(total),
                got * 100 / total.max(1),
                human((got as f64 / secs.max(1e-3)) as u64)
            );
        }
    };
    let work = std::pin::pin!(work);
    let ticker = std::pin::pin!(ticker);
    match futures::future::select(work, ticker).await {
        futures::future::Either::Left((result, _)) => result.map(|_| ()),
        futures::future::Either::Right(((), _)) => unreachable!("the ticker never ends"),
    }
}

/// How often, in seconds, a download in progress says how far it has got
/// (`COMPARA_PROGRESS`; 0 for never).
const PROGRESS_EVERY_SECS: f64 = 10.0;

/// One part, pinned to the version of the object the first request saw: a
/// part of a later version fails rather than lands beside parts of the
/// earlier one. A plain HTTP part that fails in transit or with a 5xx or 429
/// is tried again, three times, a little later each time; object_store does
/// that for its own.
async fn fetch_part(
    reader: &Reader,
    file: &File,
    part: Range<u64>,
    shift: u64,
    etag: Option<&str>,
) -> std::result::Result<(), FetchError> {
    let mut attempt = 0u32;
    loop {
        let result = fetch_part_once(reader, file, part.clone(), shift, etag).await;
        let retryable = match &result {
            Err(FetchError::Transient(_)) => matches!(reader, Reader::Http(..)),
            _ => false,
        };
        if !retryable || attempt >= 3 {
            return result.map_err(|e| match e {
                FetchError::Transient(m) => FetchError::Other(m),
                e => e,
            });
        }
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_millis(250 << attempt)).await;
    }
}

async fn fetch_part_once(
    reader: &Reader,
    file: &File,
    part: Range<u64>,
    shift: u64,
    etag: Option<&str>,
) -> std::result::Result<(), FetchError> {
    let mut at = part.start;
    match reader {
        Reader::Store(store, path) => {
            let got = store
                .get_opts(
                    path,
                    GetOptions {
                        range: Some(GetRange::Bounded(part.clone())),
                        if_match: etag.map(str::to_string),
                        ..Default::default()
                    },
                )
                .await?;
            let mut stream = got.into_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                write_at(file, &chunk, at + shift).map_err(|e| FetchError::Other(e.to_string()))?;
                at += chunk.len() as u64;
            }
        }
        Reader::Http(client, url) => {
            let mut request = client.get(url).header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", part.start, part.end - 1),
            );
            if let Some(tag) = etag {
                request = request.header(reqwest::header::IF_MATCH, tag);
            }
            let response = request
                .send()
                .await
                .map_err(|e| FetchError::Transient(said(&e)))?;
            let status = response.status();
            if status == reqwest::StatusCode::PRECONDITION_FAILED {
                return Err(FetchError::Other(changed()));
            }
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(FetchError::Transient(format!("HTTP {status}")));
            }
            if status != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(if status.is_success() {
                    FetchError::Other(format!(
                        "HTTP {status} to a range request: the server stopped honouring ranges"
                    ))
                } else {
                    Refused(status.as_u16())
                });
            }
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| FetchError::Transient(e.to_string()))?;
                write_at(file, &chunk, at + shift).map_err(|e| FetchError::Other(e.to_string()))?;
                at += chunk.len() as u64;
            }
        }
    }
    crate::memory::write_behind(file, part.start + shift, at - part.start, None);
    if at != part.end {
        return Err(FetchError::Transient(format!(
            "a range ended early: {} of {} bytes",
            at - part.start,
            part.end - part.start
        )));
    }
    Ok(())
}

fn changed() -> String {
    "the object changed while it was being read; run the comparison again".to_string()
}

/// Sorted, merged where they touch or nearly do, and cut at `limit` (what is
/// already held past it).
fn coalesce(mut ranges: Vec<Range<u64>>, limit: u64) -> Vec<Range<u64>> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<u64>> = Vec::new();
    for r in ranges {
        let r = r.start.min(limit)..r.end.min(limit);
        if r.start >= r.end {
            continue;
        }
        match out.last_mut() {
            Some(last) if r.start <= last.end.saturating_add(GAP) => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Whether a refused `https://` URL names a bucket or account to sign for.
fn signable(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    url.query().is_none()
        && (host.ends_with(".amazonaws.com")
            || host.ends_with(".blob.core.windows.net")
            || host.ends_with(".dfs.core.windows.net")
            || host.ends_with(".r2.cloudflarestorage.com"))
}

/// Cloud metadata services (where a machine's credentials come from) answer
/// at link-local addresses: 169.254.169.254, fe80::/10, AWS's fd00:ec2::254.
/// An `http(s)://` table that is there, by its address, its name or a
/// redirect, is refused unless `COMPARA_ALLOW_LINK_LOCAL=1`.
fn metadata_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6 == std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
    }
}

fn link_local_allowed() -> bool {
    std::env::var("COMPARA_ALLOW_LINK_LOCAL").is_ok_and(|v| v.trim() == "1")
}

/// The refusal for a URL whose host is a metadata address, if it is one.
fn metadata_url(url: &url::Url) -> Option<String> {
    let ip = match url.host()? {
        url::Host::Ipv4(v4) => std::net::IpAddr::V4(v4),
        url::Host::Ipv6(v6) => std::net::IpAddr::V6(v6),
        url::Host::Domain(_) => return None,
    };
    metadata_address(ip).then(|| {
        format!(
            "{url}: a link-local address (a cloud metadata service) is not read; \
             COMPARA_ALLOW_LINK_LOCAL=1 allows it"
        )
    })
}

/// Name resolution that leaves out metadata addresses (see [`metadata_address`]).
struct NoMetadata;

impl reqwest::dns::Resolve for NoMetadata {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            use std::net::ToSocketAddrs;
            let lookup = host.clone();
            let found = tokio::task::spawn_blocking(move || {
                (lookup.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|a| a.collect::<Vec<_>>())
            })
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })??;
            let kept: Vec<_> = found
                .into_iter()
                .filter(|a| !metadata_address(a.ip()))
                .collect();
            if kept.is_empty() {
                return Err(format!(
                    "{host} is a link-local address (a cloud metadata service), which is not read; \
                     COMPARA_ALLOW_LINK_LOCAL=1 allows it"
                )
                .into());
            }
            Ok(Box::new(kept.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// An HTTP error with what caused it (a refused redirect says why only there).
fn said(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        let more = c.to_string();
        if !text.contains(&more) {
            text = format!("{text}: {more}");
        }
        cause = c.source();
    }
    text
}

fn http_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("compara/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30));
    if !link_local_allowed() {
        builder =
            builder
                .dns_resolver(Arc::new(NoMetadata))
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 10 {
                        return attempt.error("more than 10 redirects");
                    }
                    match metadata_url(attempt.url()) {
                        Some(why) => attempt.error(why),
                        None => attempt.follow(),
                    }
                }));
    }
    if let Some(proxy) = proxy_from_env() {
        builder = builder.proxy(
            reqwest::Proxy::all(&proxy)
                .map_err(|e| Error::new(format!("HTTPS_PROXY={proxy}: {e}")))?
                .no_proxy(reqwest::NoProxy::from_env()),
        );
    }
    builder
        .build()
        .map_err(|e| Error::new(format!("cannot start an HTTP client: {e}")))
}

fn proxy_from_env() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// Whether plain `http://` is allowed for this cloud: its endpoint is one (an
/// emulator, a MinIO), or `<PREFIX>_ALLOW_HTTP` says so -- which object_store
/// reads from the environment itself, into options replaced here.
fn allow_http(prefix: &str, endpoints: &[&str]) -> bool {
    std::env::var(format!("{prefix}_ALLOW_HTTP"))
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"))
        || endpoints
            .iter()
            .any(|k| std::env::var(k).is_ok_and(|v| v.trim().starts_with("http://")))
}

fn client_options() -> object_store::ClientOptions {
    let mut options = object_store::ClientOptions::new()
        .with_connect_timeout(std::time::Duration::from_secs(30))
        // A part is eight megabytes; on a slow link that is not quick, and the
        // default request timeout would cut it off.
        .with_timeout(std::time::Duration::from_secs(300));
    if let Some(proxy) = proxy_from_env() {
        options = options.with_proxy_url(proxy);
        if let Some(excludes) = ["NO_PROXY", "no_proxy"]
            .iter()
            .find_map(|k| std::env::var(k).ok())
        {
            options = options.with_proxy_excludes(excludes);
        }
    }
    options
}

/// A signed store for the URL's cloud, configured from the environment.
async fn store_for(spec: &str, url: &url::Url) -> Result<Reader> {
    store_signed(spec, url, true).await
}

/// Whether a store error is the absence of credentials, the one an unsigned
/// request to a public bucket gets past.
fn no_credentials(message: &str) -> bool {
    message.contains("No usable credentials")
}

/// A store as [`store_for`] builds it, or with `signed` false one that sends
/// its requests unsigned: a public bucket's, read with no credentials at all.
async fn store_signed(spec: &str, url: &url::Url, signed: bool) -> Result<Reader> {
    let (scheme, path) = object_store::ObjectStoreScheme::parse(url)
        .map_err(|e| Error::new(format!("{spec}: {e}")))?;
    let fail = |e: object_store::Error| Error::new(format!("{spec}: {}", explain(&e.to_string())));
    let store: Arc<dyn ObjectStore> = match scheme {
        object_store::ObjectStoreScheme::AmazonS3 => {
            let mut b = object_store::aws::AmazonS3Builder::from_env()
                .with_url(spec)
                .with_client_options(client_options());
            for (key, value) in aws_profile() {
                b = b.with_config(*key, value);
            }
            let endpoint = std::env::var("AWS_ENDPOINT_URL")
                .or_else(|_| std::env::var("AWS_ENDPOINT"))
                .ok();
            if endpoint
                .as_deref()
                .is_some_and(|e| e.starts_with("http://"))
                || allow_http("AWS", &[])
            {
                b = b.with_allow_http(true);
            }
            let has_region = ["AWS_REGION", "AWS_DEFAULT_REGION"]
                .iter()
                .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))
                || aws_profile()
                    .iter()
                    .any(|(k, _)| *k == object_store::aws::AmazonS3ConfigKey::Region);
            if !has_region
                && endpoint.is_none()
                && let Some(bucket) = url.host_str()
                && (scheme_is(url, "s3") || scheme_is(url, "s3a"))
            {
                // The bucket says where it is; a request to the wrong region
                // comes back as a redirect object_store does not follow.
                if let Ok(region) =
                    object_store::aws::resolve_bucket_region(bucket, &client_options()).await
                {
                    b = b.with_region(region);
                }
            }
            if !signed {
                b = b.with_skip_signature(true);
            }
            Arc::new(b.build().map_err(fail)?)
        }
        object_store::ObjectStoreScheme::GoogleCloudStorage => {
            let mut b = object_store::gcp::GoogleCloudStorageBuilder::from_env()
                .with_url(spec)
                .with_client_options(
                    client_options().with_allow_http(allow_http("GOOGLE", &["GOOGLE_BASE_URL"])),
                );
            if !signed {
                b = b.with_skip_signature(true);
            }
            Arc::new(b.build().map_err(fail)?)
        }
        object_store::ObjectStoreScheme::MicrosoftAzure => {
            let mut b = object_store::azure::MicrosoftAzureBuilder::from_env()
                .with_url(spec)
                .with_client_options(client_options().with_allow_http(allow_http(
                    "AZURE",
                    &[
                        "AZURE_STORAGE_ENDPOINT",
                        "AZURE_ENDPOINT",
                        "AZURE_STORAGE_EMULATOR_HOST",
                    ],
                )));
            if !signed {
                b = b.with_skip_signature(true);
            } else if azure_cli_only() {
                // Signed in with `az login` and nothing else: the CLI's token,
                // as the Azure tools use it, rather than a managed identity
                // this machine does not have.
                b = b.with_use_azure_cli(true);
            }
            Arc::new(b.build().map_err(fail)?)
        }
        other => {
            return Err(Error::new(format!(
                "{spec}: {other:?} URLs are not read here"
            )));
        }
    };
    Ok(Reader::Store(store, path))
}

/// Where each cloud's credentials would come from here, by what is set and
/// on disk -- nothing is fetched or run, and no secret is shown: for `compara
/// env`, to say why a pod can or cannot read a bucket.
pub(super) fn credential_sources() -> Vec<(&'static str, String)> {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let aws = if set("AWS_ACCESS_KEY_ID") {
        "keys in the environment".to_string()
    } else if set("AWS_WEB_IDENTITY_TOKEN_FILE") {
        "web identity (an EKS service account's role)".to_string()
    } else if set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        || set("AWS_CONTAINER_CREDENTIALS_FULL_URI")
    {
        "the container's role (ECS, EKS Pod Identity)".to_string()
    } else {
        let profile = aws_profile_name();
        let files = ["credentials", "config"].iter().any(|f| {
            home.as_ref()
                .is_some_and(|h| h.join(".aws").join(f).is_file())
        }) || set("AWS_SHARED_CREDENTIALS_FILE")
            || set("AWS_CONFIG_FILE");
        if files {
            format!("the profile `{profile}` in ~/.aws, or instance metadata")
        } else {
            "instance metadata, if this is an EC2 instance; else none (public buckets \
             are read unsigned)"
                .to_string()
        }
    };
    let gcs = if set("GOOGLE_APPLICATION_CREDENTIALS") || set("GOOGLE_SERVICE_ACCOUNT") {
        "a service account file".to_string()
    } else if home.as_ref().is_some_and(|h| {
        h.join(".config/gcloud/application_default_credentials.json")
            .is_file()
    }) {
        "application default credentials (gcloud auth application-default login)".to_string()
    } else {
        "the metadata server, if this runs on Google Cloud; else none".to_string()
    };
    let azure = if set("AZURE_STORAGE_ACCOUNT_KEY") || set("AZURE_STORAGE_ACCESS_KEY") {
        "an account key".to_string()
    } else if set("AZURE_STORAGE_SAS_KEY") || set("AZURE_STORAGE_SAS_TOKEN") {
        "a SAS token".to_string()
    } else if set("AZURE_CLIENT_SECRET") {
        "a service principal's secret".to_string()
    } else if set("AZURE_FEDERATED_TOKEN_FILE") {
        "workload identity".to_string()
    } else if set("AZURE_STORAGE_USE_EMULATOR") {
        "the emulator (Azurite)".to_string()
    } else if azure_cli_only() {
        "the Azure CLI's sign-in (az login)".to_string()
    } else {
        "managed identity, if this runs on Azure; else none".to_string()
    };
    vec![("s3", aws), ("gs", gcs), ("az", azure)]
}

/// Whether the only Azure credentials here are an Azure CLI sign-in: no key,
/// SAS, secret, token or identity endpoint in the environment, and `az` on
/// the PATH with a profile in `~/.azure`.
fn azure_cli_only() -> bool {
    let configured = [
        "AZURE_STORAGE_ACCOUNT_KEY",
        "AZURE_STORAGE_ACCESS_KEY",
        "AZURE_STORAGE_KEY",
        "AZURE_STORAGE_SAS_KEY",
        "AZURE_STORAGE_SAS_TOKEN",
        "AZURE_STORAGE_TOKEN",
        "AZURE_CLIENT_SECRET",
        "AZURE_FEDERATED_TOKEN_FILE",
        "AZURE_STORAGE_USE_EMULATOR",
        "AZURE_USE_AZURE_CLI",
        "AZURE_SKIP_SIGNATURE",
        "IDENTITY_ENDPOINT",
        "MSI_ENDPOINT",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some());
    if configured {
        return false;
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let signed_in = std::env::var_os("AZURE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".azure")))
        .is_some_and(|d| d.join("azureProfile.json").is_file());
    let names: &[&str] = if cfg!(windows) {
        &["az.cmd", "az.exe"]
    } else {
        &["az"]
    };
    signed_in
        && std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|d| names.iter().any(|n| d.join(n).is_file()))
        })
}

fn scheme_is(url: &url::Url, scheme: &str) -> bool {
    url.scheme().eq_ignore_ascii_case(scheme)
}

/// Keys, token and region from the shared files, as the AWS CLI reads them,
/// when the environment has no keys of its own. `AWS_PROFILE` (or
/// `AWS_DEFAULT_PROFILE`) names the profile, `default` by default. Static keys
/// are read from `~/.aws/credentials`; a profile that only points elsewhere
/// -- `credential_process`, SSO, a role to assume -- is asked for keys the way
/// the SDKs ask: its `credential_process` run, or else the AWS CLI's
/// `aws configure export-credentials`, which resolves all of them. Read once.
fn aws_profile() -> &'static [(object_store::aws::AmazonS3ConfigKey, String)] {
    static PROFILE: std::sync::OnceLock<Vec<(object_store::aws::AmazonS3ConfigKey, String)>> =
        std::sync::OnceLock::new();
    PROFILE.get_or_init(read_aws_profile)
}

fn aws_profile_name() -> String {
    std::env::var("AWS_PROFILE")
        .or_else(|_| std::env::var("AWS_DEFAULT_PROFILE"))
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "default".into())
}

fn read_aws_profile() -> Vec<(object_store::aws::AmazonS3ConfigKey, String)> {
    use object_store::aws::AmazonS3ConfigKey as K;
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        return Vec::new();
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let profile = aws_profile_name();
    let file = |var: &str, name: &str| -> Option<String> {
        let path = std::env::var_os(var).map(PathBuf::from).or_else(|| {
            home.as_ref()
                .map(|h| PathBuf::from(h).join(".aws").join(name))
        })?;
        std::fs::read_to_string(path).ok()
    };
    let credentials = file("AWS_SHARED_CREDENTIALS_FILE", "credentials")
        .map(|t| ini_section(&t, &profile))
        .unwrap_or_default();
    let config = file("AWS_CONFIG_FILE", "config")
        .map(|t| {
            let name = if profile == "default" {
                "default".to_string()
            } else {
                format!("profile {profile}")
            };
            ini_section(&t, &name)
        })
        .unwrap_or_default();
    let get = |k: &str| {
        credentials
            .iter()
            .chain(&config)
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for (name, key) in [
        ("aws_access_key_id", K::AccessKeyId),
        ("aws_secret_access_key", K::SecretAccessKey),
        ("aws_session_token", K::Token),
    ] {
        if let Some(v) = get(name) {
            out.push((key, v));
        }
    }
    let delegated = [
        "credential_process",
        "sso_session",
        "sso_start_url",
        "role_arn",
    ]
    .iter()
    .any(|k| get(k).is_some());
    if out.is_empty() && delegated {
        let process = get("credential_process");
        let output = match &process {
            Some(command) if cfg!(windows) => std::process::Command::new("cmd")
                .args(["/C", command])
                .output(),
            Some(command) => std::process::Command::new("sh")
                .args(["-c", command])
                .output(),
            None => std::process::Command::new("aws")
                .args(["configure", "export-credentials", "--profile", &profile])
                .args(["--format", "process"])
                .output(),
        };
        match output {
            Ok(o) if o.status.success() => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    for (name, key) in [
                        ("AccessKeyId", K::AccessKeyId),
                        ("SecretAccessKey", K::SecretAccessKey),
                        ("SessionToken", K::Token),
                    ] {
                        if let Some(text) = v[name].as_str() {
                            out.push((key, text.to_string()));
                        }
                    }
                }
            }
            Ok(o) => {
                let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
                let _ = PROFILE_TROUBLE.set(format!(
                    "the AWS profile `{profile}` gave no credentials ({}){}",
                    if why.is_empty() {
                        format!("exit {}", o.status)
                    } else {
                        why
                    },
                    if get("sso_session").is_some() || get("sso_start_url").is_some() {
                        format!("; `aws sso login --profile {profile}` first")
                    } else {
                        String::new()
                    }
                ));
            }
            Err(e) => {
                let _ = PROFILE_TROUBLE.set(format!(
                    "the AWS profile `{profile}` needs {} to give credentials, which could \
                     not be run ({e}); or export them: `aws configure export-credentials \
                     --profile {profile} --format env`",
                    if process.is_some() {
                        "its credential_process"
                    } else {
                        "the AWS CLI"
                    }
                ));
            }
        }
    }
    if let Some(v) = get("region") {
        out.push((K::Region, v));
    }
    out
}

/// Why the AWS profile gave no credentials, for the error that follows.
static PROFILE_TROUBLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn ini_section(text: &str, name: &str) -> Vec<(String, String)> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = header.trim() == name;
            continue;
        }
        if inside && let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_addresses_are_link_local_ones() {
        for ip in [
            "169.254.169.254",
            "169.254.0.1",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
        ] {
            assert!(metadata_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "8.8.8.8",
            "::1",
            "fd00::1",
            "2001:db8::1",
        ] {
            assert!(!metadata_address(ip.parse().unwrap()), "{ip}");
        }
        let url = |u: &str| url::Url::parse(u).unwrap();
        assert!(metadata_url(&url("http://169.254.169.254/latest/meta-data/")).is_some());
        assert!(metadata_url(&url("http://[fd00:ec2::254]/")).is_some());
        assert!(metadata_url(&url("https://example.com/a.csv")).is_none());
        assert!(metadata_url(&url("http://127.0.0.1:8080/a.csv")).is_none());
    }

    #[test]
    fn ranges_merge_when_close_and_stop_at_the_tail() {
        let got = coalesce(
            vec![
                10..20,
                0..4,
                21..30,
                2_000_000..2_000_100,
                5_000_000..6_000_000,
            ],
            5_500_000,
        );
        assert_eq!(got, vec![0..30, 2_000_000..2_000_100, 5_000_000..5_500_000]);
        assert!(coalesce(vec![10..10, 600..700], 500).is_empty());
    }

    #[test]
    fn a_profile_section_is_read_by_name() {
        let text = "[default]\naws_access_key_id = A\n\n[profile dev]\nregion = eu-west-1\n# c\n";
        assert_eq!(
            ini_section(text, "default"),
            vec![("aws_access_key_id".into(), "A".into())]
        );
        assert_eq!(
            ini_section(text, "profile dev"),
            vec![("region".into(), "eu-west-1".into())]
        );
        assert!(ini_section(text, "other").is_empty());
    }

    #[test]
    fn only_cloud_hosts_without_a_query_are_signed() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(signable(&u("https://b.s3.us-east-1.amazonaws.com/k.csv")));
        assert!(signable(&u("https://acct.blob.core.windows.net/c/k.csv")));
        assert!(!signable(&u(
            "https://b.s3.amazonaws.com/k.csv?X-Amz-Signature=x"
        )));
        assert!(!signable(&u("https://example.com/k.csv")));
    }
}
