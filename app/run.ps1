# Builds release binaries, starts erebus-core if it isn't running, then starts erebus-face.
# The core keeps running after the face closes. Stop it with: Stop-Process -Name erebus-core

param([switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$bin = Join-Path $root 'target\release'

if (-not $NoBuild) {
    cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p erebus-core -p erebus-face
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

if (-not (Get-Process -Name 'erebus-core' -ErrorAction SilentlyContinue)) {
    Start-Process -FilePath (Join-Path $bin 'erebus-core.exe') -WindowStyle Hidden
}

Start-Process -FilePath (Join-Path $bin 'erebus-face.exe')
