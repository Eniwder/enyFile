# Records the README animation: builds, runs `enyfile --demo` (which plays the script in
# src\demo.rs and writes one frame per state), then encodes docs\demo.webp with ffmpeg.
# Needs ffmpeg with libwebp on PATH. The window opens for about a minute; leave it alone
# (keys typed into it would end up in the recording).
param([switch]$NoBuild, [string]$Out)
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot
if (-not $Out) { $Out = Join-Path $root 'docs\demo.webp' }
$cargoArgs = @()
if (Test-Path (Join-Path $root '.tools\cargo\bin')) {
    $env:RUSTUP_HOME = Join-Path $root '.tools\rustup'
    $env:CARGO_HOME = Join-Path $root '.tools\cargo'
    $env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
    $cargoArgs = @('--offline')
}
if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) { throw 'ffmpeg is not on PATH' }
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
if (-not $NoBuild) {
    & cargo build --release @cargoArgs --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
}
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
$exe = Join-Path $target 'release\enyfile.exe'
# The folders shown (rebuilt each run) and the frames; both under app\data (not in git).
$demo = Join-Path $PSScriptRoot 'data\demo'
$frames = Join-Path $PSScriptRoot 'data\demo-frames'
# Same pixel size on any display scaling.
$env:SLINT_SCALE_FACTOR = '1'
$p = Start-Process -FilePath $exe -ArgumentList @('--demo', "`"$demo`"", "`"$frames`"") -PassThru
if (-not $p.WaitForExit(240000)) { $p.Kill(); throw 'Demo timeout' }
$errorFile = Join-Path $frames 'demo-error.txt'
if (Test-Path $errorFile) { throw ('Demo failed: ' + (Get-Content -Raw $errorFile)) }
$list = Join-Path $frames 'frames.ffconcat'
if (-not (Test-Path $list)) { throw 'No frames were recorded' }

New-Item -ItemType Directory -Path (Split-Path $Out) -Force | Out-Null
# Frame durations come from the list (variable frame rate); lossless keeps text crisp and
# stays small because consecutive UI frames differ in small areas.
& ffmpeg -hide_banner -loglevel error -y -f concat -safe 0 -i $list -fps_mode vfr `
    -c:v libwebp_anim -lossless 1 -compression_level 6 -loop 0 $Out
if ($LASTEXITCODE -ne 0) { throw 'ffmpeg failed' }
$count = (Select-String -Path $list -Pattern '^duration').Count
'{0}: {1} frames, {2:N1} MB' -f $Out, $count, ((Get-Item $Out).Length / 1MB)
