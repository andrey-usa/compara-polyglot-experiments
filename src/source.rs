//! Where a side's bytes come from.
//!
//! The engines read a plain local file: they map it, and everything they do
//! after that is pointer arithmetic over the mapping. So every other kind of
//! input is turned into one first, and the engines never know:
//!
//! - a local file is read where it is;
//! - `-` is standard input, spooled to a temporary file;
//! - a gzip or zstd file (`.csv.gz`, `.ndjson.zst`, whatever its name) is
//!   decompressed to a temporary file, by its first bytes rather than its name;
//! - with the `cloud` feature, an `s3://`, `gs://`, `az://` or `https://` URL
//!   is downloaded in parallel ranges, and a Parquet object only as far as the
//!   comparison reads it: its footer and the column chunks of the columns it
//!   compares (see [`remote`]).
//!
//! Temporary files live in the system's temporary directory (`TMPDIR`) and go
//! when the [`Source`] does. In a container that is the writable scratch
//! space -- an `emptyDir` under a read-only root -- and a page cache the
//! kernel can reclaim, where holding the bytes in memory would count against
//! the container's limit until the end.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::options::Options;

mod delta;
mod many;
#[cfg(feature = "cloud")]
mod remote;

/// One side of a comparison, as a local file the engines can map.
pub struct Source {
    spec: String,
    path: PathBuf,
    /// The bytes the input holds where it is stored: the object's size, the
    /// compressed file's, what came in on stdin.
    stored: u64,
    temps: Vec<PathBuf>,
    steps: Vec<String>,
}

impl Drop for Source {
    fn drop(&mut self) {
        for path in &self.temps {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Which columns a comparison reads, so a remote Parquet object is fetched
/// only as far as that.
#[derive(Clone, Debug, Default)]
pub struct Want {
    key: Vec<String>,
    compare: Option<Vec<String>>,
    ignore: Vec<String>,
    /// Only the footer: `columns` asks for nothing else.
    footer_only: bool,
}

impl Want {
    /// Every column.
    pub fn all() -> Self {
        Want::default()
    }

    /// The schema and nothing else.
    pub fn footer_only() -> Self {
        Want {
            footer_only: true,
            ..Want::default()
        }
    }

    /// What a comparison with these options reads: the key, and the compared
    /// columns -- all but the ignored ones, or those `--compare` names. A
    /// superset of what the engines resolve, never less: a column present on
    /// one side only is fetched and then not compared.
    pub fn for_options(opt: &Options) -> Self {
        Want {
            key: opt.key.clone(),
            compare: opt.compare.clone(),
            ignore: opt.ignore.clone(),
            footer_only: false,
        }
    }

    /// Whether the column called `name` is read.
    pub fn needs(&self, name: &str) -> bool {
        if self.footer_only {
            return false;
        }
        if self.key.iter().any(|k| k == name) {
            return true;
        }
        if self.ignore.iter().any(|i| i == name) {
            return false;
        }
        match &self.compare {
            Some(list) => list.iter().any(|c| c == name),
            None => true,
        }
    }
}

/// Whether `spec` names something to download rather than a local path.
pub fn is_remote(spec: &str) -> bool {
    let Some((scheme, rest)) = spec.split_once("://") else {
        return false;
    };
    !rest.is_empty()
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "s3" | "s3a" | "gs" | "az" | "adl" | "azure" | "abfs" | "abfss" | "http" | "https"
        )
}

impl Source {
    /// Opens `spec` -- a path, `-` or a URL -- as a local file.
    pub fn open(spec: &str, want: &Want) -> Result<Source> {
        let mut source = if spec == "-" {
            Source::stdin()?
        } else if is_remote(spec) {
            Source::remote(spec, want)?
        } else if let Some((files, note)) = many::local_files(spec)? {
            Source::local_many(spec, files, &note)?
        } else {
            let path = PathBuf::from(spec);
            let meta = std::fs::metadata(&path)
                .map_err(|_| Error::new(format!("file not found: {spec}")))?;
            if !meta.is_file() {
                return Err(Error::new(format!("not a file: {spec}")));
            }
            Source {
                spec: spec.to_string(),
                path,
                stored: meta.len(),
                temps: Vec::new(),
                steps: Vec::new(),
            }
        };
        source.decompress()?;
        Ok(source)
    }

    /// What the caller named: the path, `-`, or the URL.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// The local file to read.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The input's size where it is stored, before decompression.
    pub fn stored_size(&self) -> u64 {
        self.stored
    }

    /// What was done to get the local file, one line a step (`downloaded
    /// 3.4 GB in 2.1s`), for a note on stderr; empty for a local file read
    /// where it is.
    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    /// The last segment of the spec, as a report names a file.
    pub fn name(&self) -> String {
        if self.spec == "-" {
            return "stdin".to_string();
        }
        let trimmed = self.spec.split(['?', '#']).next().unwrap_or(&self.spec);
        trimmed
            .rsplit(['/', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or(trimmed)
            .to_string()
    }

    /// The files of a local directory or glob, in path order, as one.
    fn local_many(spec: &str, files: Vec<(String, PathBuf)>, note: &str) -> Result<Source> {
        let started = Instant::now();
        let mut combined = many::Combined::new()?;
        let relative: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
        let partitions = many::table_partitions(&relative);
        if let Some((names, _)) = &partitions {
            combined.set_partitions(names.clone());
        }
        let mut source = Source {
            spec: spec.to_string(),
            path: combined.path.clone(),
            stored: 0,
            temps: vec![combined.path.clone()],
            steps: Vec::new(),
        };
        let total: u64 = files
            .iter()
            .map(|f| std::fs::metadata(&f.1).map(|m| m.len()).unwrap_or(0))
            .sum();
        room_for(total, spec)?;
        for (i, (_, file)) in files.iter().enumerate() {
            source.stored += std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            let values = partitions.as_ref().map_or(&[][..], |p| &p.1[i][..]);
            combined.append_file(&file.display().to_string(), file, values)?;
        }
        combined.finish()?;
        source.steps.push(format!(
            "{}{note} in {:.1}s",
            combined.note(),
            started.elapsed().as_secs_f64()
        ));
        Ok(source)
    }

    fn stdin() -> Result<Source> {
        let started = Instant::now();
        let (path, file) = temp_file("stdin")?;
        let mut source = Source {
            spec: "-".to_string(),
            path: path.clone(),
            stored: 0,
            temps: vec![path],
            steps: Vec::new(),
        };
        let mut out = BufWriter::with_capacity(1 << 20, crate::memory::WriteBehind::new(file));
        let n = std::io::copy(&mut std::io::stdin().lock(), &mut out)
            .and_then(|n| out.flush().map(|_| n))
            .map_err(|e| Error::new(format!("cannot read standard input: {e}")))?;
        source.stored = n;
        source.steps.push(format!(
            "read {} from standard input in {:.1}s",
            human(n),
            started.elapsed().as_secs_f64()
        ));
        Ok(source)
    }

    #[cfg(feature = "cloud")]
    fn remote(spec: &str, want: &Want) -> Result<Source> {
        remote::fetch(spec, want)
    }

    #[cfg(not(feature = "cloud"))]
    fn remote(spec: &str, _want: &Want) -> Result<Source> {
        Err(Error::new(format!(
            "{spec}: this build reads local files only (it was built without the `cloud` \
             feature); download the file first, or use a release build"
        )))
    }

    /// Decompresses a gzip or zstd file to a temporary one, and refuses the
    /// compressed formats it cannot read with what they are, rather than letting
    /// a CSV reader report the binary as a missing key column.
    fn decompress(&mut self) -> Result<()> {
        let mut head = [0u8; 6];
        let n = {
            let mut f = File::open(&self.path)
                .map_err(|e| Error::new(format!("cannot read {}: {e}", self.spec)))?;
            read_up_to(&mut f, &mut head)?
        };
        let head = &head[..n];
        let kind = if head.starts_with(&[0x1f, 0x8b]) {
            "gzip"
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            "zstd"
        } else {
            let other = if head.starts_with(b"BZh") {
                Some("bzip2")
            } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
                Some("xz")
            } else if head.starts_with(b"PK\x03\x04") {
                Some("zip")
            } else {
                None
            };
            if let Some(other) = other {
                return Err(Error::new(format!(
                    "{} is {other}-compressed, which is not read here; gzip and zstd are \
                     (or decompress it first)",
                    self.spec
                )));
            }
            return Ok(());
        };
        let started = Instant::now();
        let (path, file) = temp_file("plain")?;
        self.temps.push(path.clone());
        let input = BufReader::with_capacity(
            1 << 20,
            File::open(&self.path)
                .map_err(|e| Error::new(format!("cannot read {}: {e}", self.spec)))?,
        );
        let mut out = BufWriter::with_capacity(1 << 20, crate::memory::WriteBehind::new(file));
        let read = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let input = Counted(input, read.clone());
        let bad = |e: std::io::Error| {
            Error::new(format!("{} does not {kind}-decompress: {e}", self.spec))
        };
        let mut decoder: Box<dyn Read> = match kind {
            "gzip" => Box::new(flate2::read::MultiGzDecoder::new(input)),
            _ => Box::new(zstd::stream::read::Decoder::new(input).map_err(bad)?),
        };
        let ratio = max_expansion();
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            let k = match decoder.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(bad(e)),
            };
            out.write_all(&buf[..k]).map_err(bad)?;
            written += k as u64;
            if ratio > 0 && written > EXPANSION_FLOOR && written / read.get().max(1) > ratio {
                return Err(Error::new(format!(
                    "{} expands to more than {ratio} times its size ({} from {} so far): a \
                     decompression bomb? COMPARA_MAX_EXPANSION=<times> (0: no limit) lets it",
                    self.spec,
                    human(written),
                    human(read.get())
                )));
            }
        }
        out.flush().map_err(bad)?;
        // A downloaded or spooled compressed file is not needed once it is
        // expanded; a local one is the caller's and stays.
        if let Some(i) = self.temps.iter().position(|t| *t == self.path) {
            let _ = std::fs::remove_file(self.temps.remove(i));
        }
        self.path = path;
        self.steps.push(format!(
            "{kind}-decompressed {} to {} in {:.1}s",
            human(self.stored),
            human(written),
            started.elapsed().as_secs_f64()
        ));
        Ok(())
    }
}

/// Tables expand 3 to 30 times their gzip or zstd size; a decompression
/// bomb thousands of times. Past [`EXPANSION_FLOOR`] of output, more than
/// this many times the input read so far is refused (`COMPARA_MAX_EXPANSION`,
/// 0 for no limit), so a small file of one repeated value still opens.
const MAX_EXPANSION: u64 = 1000;
const EXPANSION_FLOOR: u64 = 1 << 30;

fn max_expansion() -> u64 {
    std::env::var("COMPARA_MAX_EXPANSION")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(MAX_EXPANSION)
}

/// A reader that counts what it has read into the cell.
struct Counted<R>(R, std::rc::Rc<std::cell::Cell<u64>>);

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1.set(self.1.get() + n as u64);
        Ok(n)
    }
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

/// A new file in the temporary directory, named for this process so a crash
/// leaves something recognisable behind.
pub(crate) fn temp_file(tag: &str) -> Result<(PathBuf, File)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir();
    for _ in 0..16 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("compara-{}-{n}-{tag}", std::process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Error::new(format!(
                    "cannot create a temporary file in {} ({e}); set TMPDIR to a writable \
                     directory",
                    dir.display()
                )));
            }
        }
    }
    Err(Error::new("cannot find a free temporary file name"))
}

/// Where each cloud's credentials would come from here (scheme, source), by
/// what is set and on disk; empty without the `cloud` feature.
pub fn credential_sources() -> Vec<(&'static str, String)> {
    #[cfg(feature = "cloud")]
    {
        remote::credential_sources()
    }
    #[cfg(not(feature = "cloud"))]
    {
        Vec::new()
    }
}

/// Writes the local file `from` to `dest`, an object storage or HTTP URL;
/// the bytes written.
#[cfg(feature = "cloud")]
pub fn upload(from: &Path, dest: &str) -> Result<u64> {
    remote::upload(from, dest)
}

/// Writes the local file `from` to `dest`, an object storage or HTTP URL.
#[cfg(not(feature = "cloud"))]
pub fn upload(_from: &Path, dest: &str) -> Result<u64> {
    Err(Error::new(format!(
        "{dest}: this build writes local files only (it was built without the `cloud` feature)"
    )))
}

/// A directory for outputs that go somewhere else afterwards, removed with
/// what is in it when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Result<TempDir> {
        let (path, file) = temp_file(tag)?;
        drop(file);
        std::fs::remove_file(&path)?;
        std::fs::create_dir(&path)?;
        Ok(TempDir(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether a temporary file can be made: stdin, compressed and remote inputs
/// need one, and so does the engine that spills to disk.
pub fn temp_dir_writable() -> bool {
    match temp_file("probe") {
        Ok((path, _)) => {
            let _ = std::fs::remove_file(path);
            true
        }
        Err(_) => false,
    }
}

/// The bytes free to this process on the filesystem holding `dir`, where the
/// platform says.
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn statvfs(path: *const std::ffi::c_char, buf: *mut u64) -> std::ffi::c_int;
        }
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // struct statvfs on 64-bit Linux: f_bsize, f_frsize, f_blocks,
        // f_bfree, f_bavail, ... -- all 64-bit, well inside 32 words.
        let mut buf = [0u64; 32];
        // SAFETY: a NUL-terminated path and a buffer larger than the struct.
        if unsafe { statvfs(path.as_ptr(), buf.as_mut_ptr()) } != 0 {
            return None;
        }
        Some(buf[4].saturating_mul(buf[1]))
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    {
        let _ = dir;
        None
    }
}

/// Refuses, before anything is written, a temporary file the temporary
/// directory has no room for: an error up front that says how much is needed
/// and where, rather than "No space left on device" partway through a
/// download.
pub(crate) fn room_for(bytes: u64, what: &str) -> Result<()> {
    let dir = std::env::temp_dir();
    match free_space(&dir) {
        Some(free) if bytes > free => Err(Error::new(format!(
            "{what} needs {} in {}, which has {} free; set TMPDIR to a larger disk (in \
             Kubernetes, an emptyDir with room for it)",
            human(bytes),
            dir.display(),
            human(free)
        ))),
        _ => Ok(()),
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_file_with_no_room_is_refused_up_front() {
        assert!(room_for(0, "nothing").is_ok());
        if free_space(&std::env::temp_dir()).is_some() {
            let e = room_for(u64::MAX, "s3://b/huge.csv")
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("s3://b/huge.csv needs") && e.contains("TMPDIR"),
                "{e}"
            );
        }
    }

    #[test]
    fn urls_are_remote_and_paths_are_not() {
        for spec in [
            "s3://bucket/a.csv",
            "S3://bucket/a.csv",
            "gs://b/x.parquet",
            "az://c/d.ndjson",
            "abfss://c@acct.dfs.core.windows.net/x",
            "https://example.com/a.csv",
            "http://127.0.0.1:9000/b/a.csv",
        ] {
            assert!(is_remote(spec), "{spec}");
        }
        for spec in [
            "a.csv",
            "/tmp/a.csv",
            "-",
            "C:\\data\\a.csv",
            "s3:/x",
            "ftp://h/a.csv",
            "s3://",
        ] {
            assert!(!is_remote(spec), "{spec}");
        }
    }

    #[test]
    fn a_want_reads_the_key_and_what_is_compared() {
        let mut opt = Options::with_key(["id"]);
        opt.ignore = vec!["updated_at".into(), "id".into()];
        let w = Want::for_options(&opt);
        assert!(w.needs("id"), "a key column is read even when ignored");
        assert!(!w.needs("updated_at"));
        assert!(w.needs("amount"));
        opt.compare = Some(vec!["amount".into()]);
        let w = Want::for_options(&opt);
        assert!(w.needs("amount") && w.needs("id") && !w.needs("note"));
        assert!(!Want::footer_only().needs("id"));
        assert!(Want::all().needs("anything"));
    }

    #[test]
    fn a_name_is_the_last_segment() {
        let s = |spec: &str| Source {
            spec: spec.into(),
            path: PathBuf::new(),
            stored: 0,
            temps: Vec::new(),
            steps: Vec::new(),
        };
        assert_eq!(s("s3://b/dir/a.csv").name(), "a.csv");
        assert_eq!(
            s("https://h/x/y.parquet?X-Amz-Signature=1").name(),
            "y.parquet"
        );
        assert_eq!(s("data/b.csv").name(), "b.csv");
        assert_eq!(s("-").name(), "stdin");
    }

    #[test]
    fn gzip_and_zstd_inputs_read_as_their_contents() {
        let text = b"id,v\n1,a\n2,b\n".repeat(1000);
        let dir = std::env::temp_dir();
        let gz = dir.join(format!("compara-test-{}-a.csv.gz", std::process::id()));
        let zs = dir.join(format!("compara-test-{}-a.csv.zst", std::process::id()));
        {
            let mut e = flate2::write::GzEncoder::new(
                File::create(&gz).unwrap(),
                flate2::Compression::fast(),
            );
            e.write_all(&text).unwrap();
            e.finish().unwrap();
        }
        std::fs::write(&zs, zstd::stream::encode_all(&text[..], 3).unwrap()).unwrap();
        for p in [&gz, &zs] {
            let s = Source::open(p.to_str().unwrap(), &Want::all()).unwrap();
            assert_ne!(s.path(), p.as_path());
            assert_eq!(std::fs::read(s.path()).unwrap(), text);
            assert_eq!(s.steps().len(), 1, "{:?}", s.steps());
            let expanded = s.path().to_path_buf();
            drop(s);
            assert!(!expanded.exists(), "the expanded copy goes with the source");
            assert!(p.exists(), "the caller's file stays");
            std::fs::remove_file(p).unwrap();
        }
    }

    #[test]
    fn other_compressions_are_named_not_misread() {
        let p = std::env::temp_dir().join(format!("compara-test-{}-a.csv.bz2", std::process::id()));
        std::fs::write(&p, b"BZh91AY&SY").unwrap();
        let e = Source::open(p.to_str().unwrap(), &Want::all())
            .err()
            .unwrap();
        assert!(e.to_string().contains("bzip2"), "{e}");
        std::fs::remove_file(&p).unwrap();
    }
}
