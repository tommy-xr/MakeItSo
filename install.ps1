# Install mis, mis-worker and mis-orchestrator from a GitHub release (Windows x64).
#   irm https://raw.githubusercontent.com/tommy-xr/MakeItSo/main/install.ps1 | iex
# Env: MIS_VERSION (tag, default latest), MIS_INSTALL_DIR (default %LOCALAPPDATA%\mis\bin),
#      MIS_TARBALL (local .tar.gz; skips download).
$ErrorActionPreference = 'Stop'

$repo = 'tommy-xr/MakeItSo'
$target = 'x86_64-pc-windows-msvc'
$dir = if ($env:MIS_INSTALL_DIR) { $env:MIS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'mis\bin' }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'mis install: only 64-bit Windows is supported' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    if ($env:MIS_TARBALL) {
        $tarball = $env:MIS_TARBALL
    } else {
        $tag = $env:MIS_VERSION
        if (-not $tag) {
            $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
        }
        $url = "https://github.com/$repo/releases/download/$tag/mis-$target.tar.gz"
        Write-Host "Downloading $url"
        $tarball = Join-Path $tmp 'mis.tar.gz'
        try { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $tarball }
        catch { throw "mis install: download failed: release $tag or asset mis-$target.tar.gz not found" }
    }

    # tar.exe ships with Windows 10 1803+.
    tar -xzf $tarball -C $tmp
    if ($LASTEXITCODE -ne 0) { throw "mis install: could not extract $tarball" }

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    foreach ($bin in 'mis', 'mis-worker', 'mis-orchestrator') {
        Copy-Item -Force (Join-Path $tmp "$bin.exe") $dir
    }
} finally {
    Remove-Item -Recurse -Force $tmp
}

Write-Host "Installed mis, mis-worker, mis-orchestrator to $dir"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
    [Environment]::SetEnvironmentVariable('Path', (@($userPath, $dir) | Where-Object { $_ }) -join ';', 'User')
    $env:Path += ";$dir"
    Write-Host "Added $dir to user PATH (restart your terminal)."
}
