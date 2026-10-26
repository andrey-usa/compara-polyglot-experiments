#!/usr/bin/env python3
"""Repackage release archives for package managers.

Takes the archives the release workflow builds (`<name>-<target>.tar.gz` /
`.zip`, each with a `.sha256`) and writes:

  npm       <out>/npm/<name>/ plus one package per platform, the layout esbuild
            and Biome use: the main package's launcher runs the binary from
            the optional dependency npm installed for this platform
  wheel     <out>/wheels/*.whl: one py3-none-<platform> wheel per target with
            the binary in its scripts, the layout ruff and uv use
  homebrew  <out>/<name>.rb
  scoop     <out>/<name>.json

Standard library only, so it runs on any CI image:

  package.py npm --name navigera --version 0.4.0 --dist dist --out pkg \\
      --repo andrey-usa/navigera --description "..." --license MIT
"""
import argparse
import base64
import hashlib
import io
import json
import stat
import tarfile
import zipfile
from pathlib import Path

# Rust target -> (npm os, npm cpu, wheel platform tag, Homebrew os/arch). The
# musl builds are static and run on any Linux; the gnu ones need glibc 2.28.
TARGETS = {
    "x86_64-unknown-linux-musl": (
        "linux", "x64", "manylinux_2_17_x86_64.manylinux2014_x86_64.musllinux_1_1_x86_64", ("linux", "intel")),
    "aarch64-unknown-linux-musl": (
        "linux", "arm64", "manylinux_2_17_aarch64.manylinux2014_aarch64.musllinux_1_1_aarch64", ("linux", "arm")),
    "x86_64-unknown-linux-gnu": ("linux", "x64", "manylinux_2_28_x86_64", ("linux", "intel")),
    "aarch64-unknown-linux-gnu": ("linux", "arm64", "manylinux_2_28_aarch64", ("linux", "arm")),
    "x86_64-apple-darwin": ("darwin", "x64", "macosx_10_12_x86_64", ("macos", "intel")),
    "aarch64-apple-darwin": ("darwin", "arm64", "macosx_11_0_arm64", ("macos", "arm")),
    "x86_64-pc-windows-msvc": ("win32", "x64", "win_amd64", None),
}


def archives(dist: Path, name: str):
    """(target, archive path, sha256) for every target present in `dist`."""
    for target in TARGETS:
        for ext in (".tar.gz", ".zip"):
            path = dist / f"{name}-{target}{ext}"
            if path.exists():
                yield target, path, hashlib.sha256(path.read_bytes()).hexdigest()


def binary(archive: Path, name: str) -> tuple[str, bytes]:
    """The executable inside a release archive: (file name, contents)."""
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as z:
            member = next(m for m in z.namelist() if Path(m).name == f"{name}.exe")
            return f"{name}.exe", z.read(member)
    with tarfile.open(archive) as t:
        member = next(m for m in t.getmembers() if Path(m.name).name == name and m.isfile())
        return name, t.extractfile(member).read()


def write_exe(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


LAUNCHER = """#!/usr/bin/env node
"use strict";
// Runs the {name} binary from the platform package npm installed alongside.
const {{ spawnSync }} = require("child_process");
const pkg = `{name}-${{process.platform}}-${{process.arch}}`;
const exe = process.platform === "win32" ? "{name}.exe" : "{name}";
let bin;
try {{
  bin = require.resolve(`${{pkg}}/bin/${{exe}}`);
}} catch {{
  console.error(`{name}: no prebuilt binary for ${{process.platform}}-${{process.arch}} (package ${{pkg}}).`);
  console.error("Install from source instead: cargo install {name}");
  process.exit(1);
}}
const result = spawnSync(bin, process.argv.slice(2), {{ stdio: "inherit" }});
if (result.error) {{
  console.error(`{name}: ${{result.error.message}}`);
  process.exit(1);
}}
process.exit(result.status ?? 1);
"""


def npm(args) -> None:
    root = Path(args.out) / "npm"
    common = {
        "version": args.version,
        "license": args.license,
        "repository": {"type": "git", "url": f"git+https://github.com/{args.repo}.git"},
        "homepage": f"https://github.com/{args.repo}#readme",
    }
    optional = {}
    for target, archive, _ in archives(Path(args.dist), args.name):
        os_, cpu = TARGETS[target][:2]
        pkg = f"{args.name}-{os_}-{cpu}"
        if pkg in optional:
            raise SystemExit(f"two archives map to the npm package {pkg}; ship one libc per platform")
        exe, data = binary(archive, args.name)
        write_exe(root / pkg / "bin" / exe, data)
        meta = {"name": pkg, **common, "description": f"The {os_}-{cpu} binary for {args.name}.",
                "os": [os_], "cpu": [cpu], "files": ["bin"], "preferUnplugged": True}
        if target.endswith("-linux-gnu"):
            meta["libc"] = ["glibc"]
        (root / pkg / "package.json").write_text(json.dumps(meta, indent=2) + "\n")
        (root / pkg / "README.md").write_text(
            f"# {pkg}\n\nThe {os_}-{cpu} binary for [{args.name}](https://github.com/{args.repo}). "
            f"Install `{args.name}` instead; npm picks this package for you.\n")
        optional[pkg] = args.version
    if not optional:
        raise SystemExit(f"no {args.name}-<target> archives in {args.dist}")
    main = root / args.name
    write_exe(main / "bin" / f"{args.name}.js", LAUNCHER.format(name=args.name).encode())
    meta = {"name": args.name, **common, "description": args.description,
            "keywords": args.keywords.split(",") if args.keywords else [],
            "bin": {args.name: f"bin/{args.name}.js"}, "files": ["bin"],
            "engines": {"node": ">=16"}, "optionalDependencies": optional}
    if args.mcp_name:
        # The MCP Registry checks that the npm package claims the server name.
        meta["mcpName"] = args.mcp_name
    (main / "package.json").write_text(json.dumps(meta, indent=2) + "\n")
    readme = Path(args.readme) if args.readme else None
    (main / "README.md").write_text(readme.read_text() if readme and readme.exists()
                                    else f"# {args.name}\n\n{args.description}\n")
    if args.license_file and Path(args.license_file).exists():
        for pkg_dir in root.iterdir():
            (pkg_dir / "LICENSE").write_text(Path(args.license_file).read_text())
    print("\n".join(sorted(p.name for p in root.iterdir())))


def wheel(args) -> None:
    out = Path(args.out) / "wheels"
    out.mkdir(parents=True, exist_ok=True)
    dist_name = args.name.replace("-", "_")
    readme = Path(args.readme) if args.readme else None
    long_description = readme.read_text() if readme and readme.exists() else args.description
    metadata = "\n".join([
        "Metadata-Version: 2.1",
        f"Name: {args.name}",
        f"Version: {args.version}",
        f"Summary: {args.description}",
        f"Home-page: https://github.com/{args.repo}",
        f"Project-URL: Source, https://github.com/{args.repo}",
        f"Project-URL: Changelog, https://github.com/{args.repo}/blob/HEAD/CHANGELOG.md",
        f"License: {args.license}",
        *[f"Keywords: {args.keywords}"] * bool(args.keywords),
        "Classifier: Programming Language :: Rust",
        "Classifier: Environment :: Console",
        f"Classifier: License :: OSI Approved :: {args.license} License",
        "Requires-Python: >=3.8",
        "Description-Content-Type: text/markdown",
        "",
        long_description,
    ])
    for target, archive, _ in archives(Path(args.dist), args.name):
        tag = f"py3-none-{TARGETS[target][2]}"
        exe, data = binary(archive, args.name)
        info = f"{dist_name}-{args.version}.dist-info"
        files = {
            f"{dist_name}-{args.version}.data/scripts/{exe}": (data, 0o755),
            f"{info}/METADATA": (metadata.encode(), 0o644),
            f"{info}/WHEEL": (("Wheel-Version: 1.0\nGenerator: package.py\nRoot-Is-Purelib: false\n"
                               + "".join(f"Tag: py3-none-{t}\n" for t in TARGETS[target][2].split("."))).encode(),
                              0o644),
        }
        if args.license_file and Path(args.license_file).exists():
            files[f"{info}/licenses/LICENSE"] = (Path(args.license_file).read_bytes(), 0o644)
        record = io.StringIO()
        for path, (content, _) in files.items():
            digest = base64.urlsafe_b64encode(hashlib.sha256(content).digest()).rstrip(b"=").decode()
            record.write(f"{path},sha256={digest},{len(content)}\n")
        record.write(f"{info}/RECORD,,\n")
        files[f"{info}/RECORD"] = (record.getvalue().encode(), 0o644)
        path = out / f"{dist_name}-{args.version}-{tag}.whl"
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
            for name, (content, mode) in files.items():
                entry = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
                entry.external_attr = (stat.S_IFREG | mode) << 16
                entry.compress_type = zipfile.ZIP_DEFLATED
                z.writestr(entry, content)
        print(path.name)


def homebrew(args) -> None:
    by_os: dict[str, list[str]] = {"macos": [], "linux": []}
    for target, archive, sha in archives(Path(args.dist), args.name):
        brew = TARGETS[target][3]
        if not brew:
            continue
        os_, arch = brew
        url = f"https://github.com/{args.repo}/releases/download/v{args.version}/{archive.name}"
        by_os[os_].append(f"    on_{arch} do\n      url \"{url}\"\n      sha256 \"{sha}\"\n    end\n")
    klass = "".join(part.capitalize() for part in args.name.split("-"))
    blocks = "".join(f"  on_{os_} do\n{''.join(arms)}  end\n\n" for os_, arms in by_os.items() if arms)
    formula = (f"class {klass} < Formula\n"
               f"  desc \"{args.description}\"\n"
               f"  homepage \"https://github.com/{args.repo}\"\n"
               f"  version \"{args.version}\"\n"
               f"  license \"{args.license}\"\n\n"
               f"{blocks}"
               f"  def install\n    bin.install \"{args.name}\"\n  end\n\n"
               f"  test do\n    assert_match version.to_s, shell_output(\"#{{bin}}/{args.name} --version\")\n  end\nend\n")
    path = Path(args.out) / f"{args.name}.rb"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(formula)
    print(path)


def scoop(args) -> None:
    found = {t: (a, s) for t, a, s in archives(Path(args.dist), args.name)}
    archive, sha = found["x86_64-pc-windows-msvc"]
    url = f"https://github.com/{args.repo}/releases/download/v{args.version}/{archive.name}"
    manifest = {
        "version": args.version,
        "description": args.description,
        "homepage": f"https://github.com/{args.repo}",
        "license": args.license,
        "architecture": {"64bit": {"url": url, "hash": sha}},
        "bin": f"{args.name}.exe",
        "checkver": {"github": f"https://github.com/{args.repo}"},
        "autoupdate": {"architecture": {"64bit": {
            "url": f"https://github.com/{args.repo}/releases/download/v$version/{args.name}-x86_64-pc-windows-msvc.zip"}}},
    }
    path = Path(args.out) / f"{args.name}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(path)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=["npm", "wheel", "homebrew", "scoop"])
    ap.add_argument("--name", required=True)
    ap.add_argument("--version", required=True)
    ap.add_argument("--dist", required=True, help="directory with the release archives")
    ap.add_argument("--out", required=True)
    ap.add_argument("--repo", required=True, help="owner/name on GitHub")
    ap.add_argument("--description", required=True)
    ap.add_argument("--license", default="MIT")
    ap.add_argument("--license-file", default="LICENSE")
    ap.add_argument("--readme", default="README.md")
    ap.add_argument("--keywords", default="")
    ap.add_argument("--mcp-name", default="", help="npm: the server's MCP Registry name (io.github.<owner>/<name>)")
    args = ap.parse_args()
    args.version = args.version.removeprefix("v")
    {"npm": npm, "wheel": wheel, "homebrew": homebrew, "scoop": scoop}[args.kind](args)


if __name__ == "__main__":
    main()
