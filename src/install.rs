//! `navigera install-browser`: Chrome from Google's own channel for automated
//! use, [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/).
//! chrome-headless-shell by default -- the same Chrome without its browser UI
//! layer, about three times faster to start -- or the full Chrome. It goes in
//! navigera's own cache (`navigera install-browser --where` prints it) and is
//! found there on every launch after; nothing outside that directory changes.
//!
//! The index and the archive come through `curl`, as the decider's requests
//! do, so the binary carries no TLS stack; the archive is unpacked here, every
//! entry checked against its CRC and kept inside the directory it goes to.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Chrome for Testing's index of the latest version on each channel, with the
/// download of each product for each platform.
const INDEX: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

/// The products Chrome for Testing publishes that navigera can drive.
pub const PRODUCTS: [&str; 2] = ["chrome-headless-shell", "chrome"];

/// Chrome for Testing's name for this machine, where it publishes one.
pub fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux64"),
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("windows", "x86_64") => Some("win64"),
        ("windows", "x86") => Some("win32"),
        _ => None,
    }
}

/// Where installed browsers go: `$NAVIGERA_BROWSERS`, else the user's cache
/// directory (`~/.cache`, `~/Library/Caches`, `%LOCALAPPDATA%`) + `navigera/browsers`.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("NAVIGERA_BROWSERS").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let cache = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).or_else(|| home().map(|h| h.join("AppData/Local")))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home().map(|h| h.join(".cache")))
    }?;
    Some(cache.join("navigera").join("browsers"))
}

/// The executable inside an unpacked `product` for `platform`.
fn executable(product: &str, platform: &str) -> PathBuf {
    let dir = format!("{product}-{platform}");
    let rel = match (product, platform) {
        ("chrome", p) if p.starts_with("mac") => {
            "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing".to_string()
        }
        ("chrome", p) if p.starts_with("win") => "chrome.exe".to_string(),
        (other, p) if p.starts_with("win") => format!("{other}.exe"),
        (other, _) => other.to_string(),
    };
    Path::new(&dir).join(rel)
}

/// Versions sort as numbers, part by part: 147.0.7727.0 after 99.0.4844.51.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The executables of `product` installed here, newest version first.
pub fn installed(product: &str) -> Vec<String> {
    let (Some(root), Some(platform)) = (root(), platform()) else { return Vec::new() };
    let dir = root.join(product);
    let mut versions: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    versions.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
    versions
        .iter()
        .map(|v| dir.join(v).join(executable(product, platform)))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect()
}

/// `curl` a URL to stdout (`None`) or to a file.
fn curl(url: &str, to: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["--fail", "--location", "--silent", "--show-error", "--retry", "3", "--proto", "=https"]);
    if let Some(path) = to {
        cmd.arg("--output").arg(path);
    }
    let out = cmd.arg(url).output().context("run curl (install-browser downloads with curl; put it on PATH)")?;
    if !out.status.success() {
        bail!("curl {url}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The version and download URL of `product` on `channel` for `platform`.
fn pick(index: &Value, product: &str, channel: &str, platform: &str) -> Result<(String, String)> {
    let name = match channel.to_ascii_lowercase().as_str() {
        "stable" => "Stable",
        "beta" => "Beta",
        "dev" => "Dev",
        "canary" => "Canary",
        other => bail!("unknown channel {other:?}: stable, beta, dev or canary"),
    };
    let entry = &index["channels"][name];
    let version = entry["version"].as_str().context("the Chrome for Testing index has no version for that channel")?;
    let url = entry["downloads"][product]
        .as_array()
        .and_then(|list| list.iter().find(|d| d["platform"] == platform))
        .and_then(|d| d["url"].as_str())
        .with_context(|| format!("Chrome for Testing publishes no {product} {version} for {platform}"))?;
    Ok((version.to_string(), url.to_string()))
}

/// `navigera install-browser [product] [--channel <c>]`: download and unpack,
/// unless that version is installed already; the executable's path.
pub fn install(product: &str, channel: &str) -> Result<Value> {
    if !PRODUCTS.contains(&product) {
        bail!("unknown browser {product:?}: chrome-headless-shell (default) or chrome");
    }
    let Some(platform) = platform() else {
        bail!(
            "Chrome for Testing has no build for {}-{}; install Chromium from your system's packages and pass it with --chromium or $CHROME_BIN",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let root = root().context("no cache directory to install into: set $NAVIGERA_BROWSERS")?;
    let index: Value = serde_json::from_slice(&curl(INDEX, None)?).context("the Chrome for Testing index")?;
    let (version, url) = pick(&index, product, channel, platform)?;
    let dest = root.join(product).join(&version);
    let exe = dest.join(executable(product, platform));
    if exe.is_file() {
        return Ok(json!({ "installed": exe.display().to_string(), "version": version, "already": true }));
    }
    std::fs::create_dir_all(root.join(product)).with_context(|| format!("create {}", root.display()))?;
    let pid = std::process::id();
    let zip = root.join(product).join(format!(".{version}-{pid}.zip"));
    let partial = root.join(product).join(format!(".{version}-{pid}"));
    let result = (|| -> Result<()> {
        curl(&url, Some(&zip))?;
        let bytes = std::fs::read(&zip).with_context(|| format!("read {}", zip.display()))?;
        unzip(&bytes, &partial)?;
        std::fs::rename(&partial, &dest).with_context(|| format!("move into {}", dest.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&zip);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    result?;
    if !exe.is_file() {
        bail!("the archive from {url} has no {}", executable(product, platform).display());
    }
    Ok(json!({ "installed": exe.display().to_string(), "version": version, "from": url }))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    let s = b.get(at..at + 2).context("the archive ends early")?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).context("the archive ends early")?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    let s = b.get(at..at + 8).context("the archive ends early")?;
    Ok(u64::from_le_bytes(s.try_into().expect("eight bytes")))
}

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

/// `name` as a path under the directory being unpacked into, refused if it
/// is absolute or climbs out of it.
fn inside(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => bail!("the archive names a path outside its directory: {name:?}"),
        }
    }
    if name.contains('\\') || out.as_os_str().is_empty() {
        bail!("the archive names a path it can't hold: {name:?}");
    }
    Ok(out)
}

/// Unpacks a zip archive (stored and deflated entries, zip64 sizes) into `to`.
pub fn unzip(b: &[u8], to: &Path) -> Result<()> {
    // The end of central directory record: within the last 64 KB + 22 bytes.
    let from = b.len().saturating_sub(22 + 0xFFFF);
    let eocd = (from..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .context("not a zip archive")?;
    let mut count = u16_at(b, eocd + 10)? as u64;
    let mut cd = u32_at(b, eocd + 16)? as u64;
    if (count == 0xFFFF || cd == 0xFFFF_FFFF) && eocd >= 20 && b[eocd - 20..eocd - 16] == [0x50, 0x4b, 0x06, 0x07] {
        let z64 = u64_at(b, eocd - 12)? as usize;
        if u32_at(b, z64)? != 0x0606_4b50 {
            bail!("the archive's zip64 record is damaged");
        }
        count = u64_at(b, z64 + 32)?;
        cd = u64_at(b, z64 + 48)?;
    }
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    let mut at = cd as usize;
    let mut links = Vec::new();
    for _ in 0..count {
        if u32_at(b, at)? != 0x0201_4b50 {
            bail!("the archive's directory is damaged");
        }
        let made_by = u16_at(b, at + 4)?;
        let method = u16_at(b, at + 10)?;
        let crc = u32_at(b, at + 16)?;
        let mut packed = u32_at(b, at + 20)? as u64;
        let mut size = u32_at(b, at + 24)? as u64;
        let name_len = u16_at(b, at + 28)? as usize;
        let extra_len = u16_at(b, at + 30)? as usize;
        let comment_len = u16_at(b, at + 32)? as usize;
        let attrs = u32_at(b, at + 38)?;
        let mut local = u32_at(b, at + 42)? as u64;
        let name =
            String::from_utf8_lossy(b.get(at + 46..at + 46 + name_len).context("the archive ends early")?).into_owned();
        // Zip64: the sizes and offset that didn't fit, in that order.
        let extra = b.get(at + 46 + name_len..at + 46 + name_len + extra_len).context("the archive ends early")?;
        let mut e = 0;
        while e + 4 <= extra.len() {
            let (id, len) = (u16_at(extra, e)?, u16_at(extra, e + 2)? as usize);
            if id == 0x0001 {
                let mut f = e + 4;
                for field in [&mut size, &mut packed, &mut local] {
                    if *field == 0xFFFF_FFFF {
                        *field = u64_at(extra, f)?;
                        f += 8;
                    }
                }
            }
            e += 4 + len;
        }
        at += 46 + name_len + extra_len + comment_len;

        let rel = inside(&name)?;
        let path = to.join(&rel);
        let mode = if made_by >> 8 == 3 { attrs >> 16 } else { 0 };
        if name.ends_with('/') {
            std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
            continue;
        }
        let lh = local as usize;
        if u32_at(b, lh)? != 0x0403_4b50 {
            bail!("the archive's entry {name:?} is damaged");
        }
        let start = lh + 30 + u16_at(b, lh + 26)? as usize + u16_at(b, lh + 28)? as usize;
        let raw = b.get(start..start + packed as usize).context("the archive ends early")?;
        let data = match method {
            0 => raw.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, size as usize)
                .map_err(|e| anyhow::anyhow!("{name}: does not inflate ({e:?})"))?,
            other => bail!("{name}: compression method {other} is not read here"),
        };
        if data.len() as u64 != size || crc32(&data) != crc {
            bail!("{name}: damaged in the download (size or checksum differs)");
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        if mode & 0o170000 == 0o120000 {
            links.push((rel, String::from_utf8_lossy(&data).into_owned()));
            continue;
        }
        std::fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
        #[cfg(unix)]
        if mode & 0o777 != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o755))?;
        }
    }
    // Links last, and only ones that stay inside (a macOS app's framework
    // versions): one that leads out is refused like a path that does.
    for (rel, target) in links {
        let resolved = rel.parent().unwrap_or(Path::new("")).join(&target);
        let mut depth = 0i32;
        for c in resolved.components() {
            match c {
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                Component::ParentDir => depth -= 1,
                _ => depth = -1,
            }
            if depth < 0 {
                bail!("the archive links {} outside its directory ({target:?})", rel.display());
            }
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, to.join(&rel)).with_context(|| format!("link {}", rel.display()))?;
        #[cfg(not(unix))]
        let _ = target;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip of `(name, bytes, mode, deflate)` entries, as `zip` writes one.
    fn zip(entries: &[(&str, &[u8], u32, bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for &(name, data, mode, deflate) in entries {
            let packed = if deflate { miniz_oxide::deflate::compress_to_vec(data, 6) } else { data.to_vec() };
            let offset = out.len() as u32;
            let head = |sig: u32, extra: &[u8]| {
                let mut h = sig.to_le_bytes().to_vec();
                h.extend(extra);
                h
            };
            let mut common = Vec::new();
            common.extend(20u16.to_le_bytes()); // version needed
            common.extend(0u16.to_le_bytes()); // flags
            common.extend((if deflate { 8u16 } else { 0 }).to_le_bytes());
            common.extend([0, 0, 0, 0]); // time, date
            common.extend(crc32(data).to_le_bytes());
            common.extend((packed.len() as u32).to_le_bytes());
            common.extend((data.len() as u32).to_le_bytes());
            common.extend((name.len() as u16).to_le_bytes());
            common.extend(0u16.to_le_bytes()); // extra
            out.extend(head(0x0403_4b50, &common));
            out.extend(name.as_bytes());
            out.extend(&packed);
            let mut c = head(0x0201_4b50, &((3u16 << 8) | 20).to_le_bytes());
            c.extend(&common);
            c.extend(0u16.to_le_bytes()); // comment
            c.extend(0u16.to_le_bytes()); // disk
            c.extend(0u16.to_le_bytes()); // internal attrs
            c.extend((mode << 16).to_le_bytes());
            c.extend(offset.to_le_bytes());
            c.extend(name.as_bytes());
            cd.extend(c);
        }
        let at = out.len() as u32;
        out.extend(&cd);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((cd.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn an_archive_unpacks_with_its_modes_and_nothing_outside() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; 100_000];
        let archive = zip(&[
            ("shell-linux64/", b"", 0o40755, false),
            ("shell-linux64/chrome-headless-shell", &big, 0o100755, true),
            ("shell-linux64/resources.pak", b"pak", 0o100644, false),
        ]);
        unzip(&archive, dir.path()).unwrap();
        let exe = dir.path().join("shell-linux64/chrome-headless-shell");
        assert_eq!(std::fs::read(&exe).unwrap(), big);
        assert_eq!(std::fs::read(dir.path().join("shell-linux64/resources.pak")).unwrap(), b"pak");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
        for bad in ["../escape", "/abs/path", "a/../../b"] {
            let err = unzip(&zip(&[(bad, b"no", 0o100644, false)]), dir.path()).unwrap_err();
            assert!(format!("{err:#}").contains("outside"), "{bad}: {err:#}");
        }
        #[cfg(unix)]
        {
            let link = zip(&[("app/Current", b"../../etc", 0o120777, false)]);
            assert!(unzip(&link, dir.path()).is_err());
        }
        let mut damaged = zip(&[("f", b"hello world", 0o100644, false)]);
        let at = damaged.windows(11).position(|w| w == b"hello world").unwrap();
        damaged[at] = b'H';
        assert!(format!("{:#}", unzip(&damaged, dir.path()).unwrap_err()).contains("damaged"));
    }

    #[test]
    fn the_index_names_a_version_and_a_download() {
        let index = json!({"channels": {"Stable": {"version": "147.0.7727.0", "downloads": {
            "chrome-headless-shell": [{"platform": "linux64", "url": "https://x/linux64.zip"}, {"platform": "win64", "url": "https://x/win64.zip"}]
        }}}});
        assert_eq!(
            pick(&index, "chrome-headless-shell", "stable", "win64").unwrap(),
            ("147.0.7727.0".to_string(), "https://x/win64.zip".to_string())
        );
        assert!(pick(&index, "chrome-headless-shell", "stable", "mac-arm64").is_err());
        assert!(pick(&index, "chrome", "nightly", "linux64").is_err());
        assert_eq!(
            executable("chrome-headless-shell", "win64"),
            Path::new("chrome-headless-shell-win64").join("chrome-headless-shell.exe")
        );
        let mut v = vec!["99.0.4844.51", "147.0.7727.0", "147.0.700.1"];
        v.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
        assert_eq!(v, ["147.0.7727.0", "147.0.700.1", "99.0.4844.51"]);
    }
}
