# Builds release binaries, starts pinnos-core if it isn't running, then starts pinnos-face.
# The core keeps running after the face closes. Stop it with: Stop-Process -Name pinnos-core

param([switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$bin = Join-Path $root 'target\release'

if (-not $NoBuild) {
    cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p pinnos-core -p pinnos-face
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

if (-not (Get-Process -Name 'pinnos-core' -ErrorAction SilentlyContinue)) {
    Start-Process -FilePath (Join-Path $bin 'pinnos-core.exe') -WindowStyle Hidden
}

Start-Process -FilePath (Join-Path $bin 'pinnos-face.exe')
