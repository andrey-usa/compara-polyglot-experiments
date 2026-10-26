# Install navigera on Windows: the prebuilt binary from the latest GitHub
# release, or a source build with cargo when no binary is published.
#
#   irm https://raw.githubusercontent.com/andrey-usa/navigera/master/install.ps1 | iex
#
# Env: NAVIGERA_INSTALL_DIR (default %LOCALAPPDATA%\navigera\bin),
# NAVIGERA_VERSION (a tag such as v0.3.0; default: latest release),
# NAVIGERA_NO_MODIFY_PATH=1 (don't add the directory to the user PATH),
# NAVIGERA_GIT_REV (commit for the source build; default: master).
#
# The archive is checked against the SHA-256 the release publishes next to
# it; a mismatch or a missing checksum stops the install
# (NAVIGERA_NO_VERIFY=1 skips the check).
#
# Runs in the caller's session (`| iex`), so everything stays inside one
# script block and errors are thrown, never `exit`.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is slow with it
    # Windows PowerShell 5.1 may still default to TLS 1.0; GitHub needs 1.2.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $repo = 'andrey-usa/navigera'
    $dir = if ($env:NAVIGERA_INSTALL_DIR) { $env:NAVIGERA_INSTALL_DIR } `
           else { Join-Path $env:LOCALAPPDATA 'navigera\bin' }
    $version = if ($env:NAVIGERA_VERSION) { $env:NAVIGERA_VERSION } else { 'latest' }
    # x64 binary; Windows on Arm runs it under emulation.
    $target = 'x86_64-pc-windows-msvc'
    $asset = "navigera-$target.zip"
    $url = if ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download/$asset" } `
           else { "https://github.com/$repo/releases/download/$version/$asset" }

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("navigera-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $exe = Join-Path $dir 'navigera.exe'
    try {
        $downloaded = $false
        try {
            Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp $asset) -UseBasicParsing
            $downloaded = $true
        } catch { }
        if ($downloaded -and $env:NAVIGERA_NO_VERIFY -ne '1') {
            try {
                $sums = (Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing).Content
            } catch {
                throw "no checksum published at $url.sha256 (set NAVIGERA_NO_VERIFY=1 to install anyway)"
            }
            if ($sums -is [byte[]]) { $sums = [Text.Encoding]::ASCII.GetString($sums) }
            $want = ($sums.Trim() -split '\s+')[0].ToLower()
            $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLower()
            if ($want -ne $got) { throw "checksum mismatch for $url (want $want, got $got)" }
        }
        if ($downloaded) {
            Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
            Copy-Item (Join-Path $tmp 'navigera.exe') $exe -Force
            Write-Host "installed $exe ($target, $version)"
        } elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
            Write-Host "no prebuilt binary at $url; building from source with cargo (a few minutes)..."
            # The repo also holds Rust benchmark contenders, so name the package.
            # cargo reports progress on stderr: not an error (PowerShell 5.1
            # would turn redirected stderr into terminating errors under Stop).
            $ErrorActionPreference = 'Continue'
            $rev = if ($env:NAVIGERA_GIT_REV) { @('--rev', $env:NAVIGERA_GIT_REV) } else { @() }
            cargo install --locked --git "https://github.com/$repo" @rev navigera --root (Join-Path $tmp 'root') 2>&1 |
                ForEach-Object { "$_" }
            $ErrorActionPreference = 'Stop'
            if ($LASTEXITCODE -ne 0) { throw "cargo install failed (exit $LASTEXITCODE)" }
            Copy-Item (Join-Path $tmp 'root\bin\navigera.exe') $exe -Force
            Write-Host "installed $exe (built from source)"
        } else {
            throw "no prebuilt binary at $url and no cargo; install Rust (https://rustup.rs) and rerun"
        }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    # On PATH for this session, and (unless opted out) for new ones.
    if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$dir;$env:Path" }
    if ($env:NAVIGERA_NO_MODIFY_PATH -ne '1') {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (-not (($userPath -split ';') -contains $dir)) {
            [Environment]::SetEnvironmentVariable('Path', ($(if ($userPath) { "$dir;$userPath" } else { $dir })), 'User')
            Write-Host "added $dir to your user PATH (new terminals pick it up)"
        }
    }
    & $exe --version
    Write-Host "next: navigera install-skill   # usage guide for your agent (./.agents/skills; --claude for ./.claude/skills)"
}
