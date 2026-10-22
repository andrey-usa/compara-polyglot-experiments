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

use super::{Source, Want, human, temp_file};
use crate::error::{Error, Result};

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
    let mut reader = if scheme == "http" || scheme == "https" {
        Reader::Http(http_client()?, spec.to_string())
    } else {
        store_for(spec, &url).await?
    };

    // The end of the object first: its size comes with it, and for Parquet
    // the footer.
    let (size, tail) = match tail(&reader, spec).await {
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
        Err(e) => return Err(e.into_error(spec)),
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
    fetch_ranges(&reader, &file, ranges, spec).await?;
    file.sync_data().ok();
    let secs = started.elapsed().as_secs_f64();
    source.steps.push(format!(
        "downloaded {} of {}{note} in {secs:.1}s ({}/s)",
        human(fetched),
        human(size),
        human((fetched as f64 / secs.max(1e-3)) as u64)
    ));
    Ok(source)
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
            .map_err(|e| fail(e.to_string()))?;
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
enum FetchError {
    Refused(u16),
    Other(String),
}
use FetchError::Refused;

impl FetchError {
    fn into_error(self, spec: &str) -> Error {
        match self {
            Refused(404) => Error::new(format!("not found: {spec}")),
            Refused(status) => Error::new(format!("{spec}: HTTP {status}")),
            FetchError::Other(e) => Error::new(format!("{spec}: {e}")),
        }
    }
}

impl From<object_store::Error> for FetchError {
    fn from(e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => Refused(404),
            e => FetchError::Other(explain(&e.to_string())),
        }
    }
}

/// A store error, with what to do about the common one: no credentials.
fn explain(e: &str) -> String {
    let lower = e.to_ascii_lowercase();
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

/// The object's size and its last [`TAIL`] bytes (all of it, if smaller).
async fn tail(reader: &Reader, spec: &str) -> std::result::Result<(u64, Vec<u8>), FetchError> {
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
                // An empty object has no suffix to give.
                Err(e) => {
                    let meta = store.head(path).await;
                    match meta {
                        Ok(m) if m.size == 0 => return Ok((0, Vec::new())),
                        _ => return Err(e.into()),
                    }
                }
            };
            let size = got.meta.size;
            let bytes = got.bytes().await?;
            Ok((size, bytes.to_vec()))
        }
        Reader::Http(client, url) => {
            let response = client
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes=-{TAIL}"))
                .send()
                .await
                .map_err(|e| FetchError::Other(e.to_string()))?;
            let status = response.status();
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                // An empty body: nothing to take the end of.
                return Ok((0, Vec::new()));
            }
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
                return Ok((total, bytes.to_vec()));
            }
            // The server sent the whole object: it does not do ranges, so this
            // response is the download.
            let bytes = response
                .bytes()
                .await
                .map_err(|e| FetchError::Other(e.to_string()))?;
            Ok((bytes.len() as u64, bytes.to_vec()))
        }
    }
}

/// Fetches `ranges` into `file` at their offsets, in parts, several at a time.
async fn fetch_ranges(
    reader: &Reader,
    file: &File,
    ranges: Vec<Range<u64>>,
    spec: &str,
) -> Result<()> {
    let parts: Vec<Range<u64>> = ranges
        .into_iter()
        .flat_map(|r| {
            let mut out = Vec::new();
            let mut at = r.start;
            while at < r.end {
                let end = (at + PART).min(r.end);
                out.push(at..end);
                at = end;
            }
            out
        })
        .collect();
    let concurrency = std::env::var("COMPARA_FETCH_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(CONCURRENCY);
    futures::stream::iter(parts)
        .map(|part| fetch_part(reader, file, part))
        .buffer_unordered(concurrency)
        .try_collect::<Vec<()>>()
        .await
        .map_err(|e| e.into_error(spec))?;
    Ok(())
}

async fn fetch_part(
    reader: &Reader,
    file: &File,
    part: Range<u64>,
) -> std::result::Result<(), FetchError> {
    let mut at = part.start;
    match reader {
        Reader::Store(store, path) => {
            let got = store
                .get_opts(
                    path,
                    GetOptions {
                        range: Some(GetRange::Bounded(part.clone())),
                        ..Default::default()
                    },
                )
                .await?;
            let mut stream = got.into_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                write_at(file, &chunk, at).map_err(|e| FetchError::Other(e.to_string()))?;
                at += chunk.len() as u64;
            }
        }
        Reader::Http(client, url) => {
            let response = client
                .get(url)
                .header(
                    reqwest::header::RANGE,
                    format!("bytes={}-{}", part.start, part.end - 1),
                )
                .send()
                .await
                .map_err(|e| FetchError::Other(e.to_string()))?;
            let status = response.status();
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
                let chunk = chunk.map_err(|e| FetchError::Other(e.to_string()))?;
                write_at(file, &chunk, at).map_err(|e| FetchError::Other(e.to_string()))?;
                at += chunk.len() as u64;
            }
        }
    }
    if at != part.end {
        return Err(FetchError::Other(format!(
            "a range ended early: {} of {} bytes",
            at - part.start,
            part.end - part.start
        )));
    }
    Ok(())
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

fn http_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("compara/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30));
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
    let (scheme, path) = object_store::ObjectStoreScheme::parse(url)
        .map_err(|e| Error::new(format!("{spec}: {e}")))?;
    let fail = |e: object_store::Error| Error::new(format!("{spec}: {}", explain(&e.to_string())));
    let store: Arc<dyn ObjectStore> = match scheme {
        object_store::ObjectStoreScheme::AmazonS3 => {
            let mut b = object_store::aws::AmazonS3Builder::from_env()
                .with_url(spec)
                .with_client_options(client_options());
            for (key, value) in aws_profile() {
                b = b.with_config(key, value);
            }
            let endpoint = std::env::var("AWS_ENDPOINT_URL")
                .or_else(|_| std::env::var("AWS_ENDPOINT"))
                .ok();
            if endpoint
                .as_deref()
                .is_some_and(|e| e.starts_with("http://"))
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
            Arc::new(b.build().map_err(fail)?)
        }
        object_store::ObjectStoreScheme::GoogleCloudStorage => Arc::new(
            object_store::gcp::GoogleCloudStorageBuilder::from_env()
                .with_url(spec)
                .with_client_options(client_options())
                .build()
                .map_err(fail)?,
        ),
        object_store::ObjectStoreScheme::MicrosoftAzure => Arc::new(
            object_store::azure::MicrosoftAzureBuilder::from_env()
                .with_url(spec)
                .with_client_options(client_options())
                .build()
                .map_err(fail)?,
        ),
        other => {
            return Err(Error::new(format!(
                "{spec}: {other:?} URLs are not read here"
            )));
        }
    };
    Ok(Reader::Store(store, path))
}

fn scheme_is(url: &url::Url, scheme: &str) -> bool {
    url.scheme().eq_ignore_ascii_case(scheme)
}

/// Keys, token and region from a shared credentials file, as the AWS CLI reads
/// them, when the environment has no keys of its own. `AWS_PROFILE` names the
/// profile (default `default`). A profile that only points elsewhere -- SSO, a
/// role to assume, a credential process -- has no keys here; the environment
/// (`aws configure export-credentials --format env`) or an attached role is
/// the way to give those.
fn aws_profile() -> Vec<(object_store::aws::AmazonS3ConfigKey, String)> {
    use object_store::aws::AmazonS3ConfigKey as K;
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        return Vec::new();
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
    let file = |var: &str, name: &str| -> Option<String> {
        let path = std::env::var_os(var).map(PathBuf::from).or_else(|| {
            home.as_ref()
                .map(|h| PathBuf::from(h).join(".aws").join(name))
        })?;
        std::fs::read_to_string(path).ok()
    };
    let mut out = Vec::new();
    if let Some(text) = file("AWS_SHARED_CREDENTIALS_FILE", "credentials") {
        let section = ini_section(&text, &profile);
        for (name, key) in [
            ("aws_access_key_id", K::AccessKeyId),
            ("aws_secret_access_key", K::SecretAccessKey),
            ("aws_session_token", K::Token),
        ] {
            if let Some(v) = section
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
            {
                out.push((key, v));
            }
        }
    }
    if let Some(text) = file("AWS_CONFIG_FILE", "config") {
        let name = if profile == "default" {
            "default".to_string()
        } else {
            format!("profile {profile}")
        };
        if let Some(v) = ini_section(&text, &name)
            .into_iter()
            .find(|(k, _)| k == "region")
            .map(|(_, v)| v)
        {
            out.push((K::Region, v));
        }
    }
    out
}

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

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], at: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, at)
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, at)?;
        buf = &buf[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], at: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, at)
}

#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
