# Unit tests + clipboard interop smoke. The smoke OVERWRITES the Windows clipboard;
# plain text on it is saved and restored afterwards (other formats are not).
param([switch]$NoBuild)
# cargo writes progress to stderr; failures are checked through exit codes.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot
# The folder-local toolchain (.tools, offline) when present, else the Rust on PATH.
$cargoArgs = @()
if (Test-Path (Join-Path $root '.tools\cargo\bin')) {
    $env:RUSTUP_HOME = Join-Path $root '.tools\rustup'
    $env:CARGO_HOME = Join-Path $root '.tools\cargo'
    $env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
    $cargoArgs = @('--offline')
}
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
if (-not $NoBuild) {
    & cargo build --release @cargoArgs --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
}
& cargo test --release @cargoArgs --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw 'Unit tests failed' }

Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'data') -Directory -Filter 'smoke-*' -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
$data = Join-Path $root ('app\data\smoke-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss'))
foreach ($d in 'left', 'left\Folder A\sub', 'right', 'right\Folder A', 'explorer-src', 'explorer-dst', 'explorer-dst2') {
    New-Item -ItemType Directory -Path (Join-Path $data $d) -Force | Out-Null
}
$utf8 = New-Object System.Text.UTF8Encoding $false
# Enough files for several pages.
New-Item -ItemType Directory -Path (Join-Path $data 'many') -Force | Out-Null
1..150 | ForEach-Object { [IO.File]::WriteAllText((Join-Path $data ('many\file{0:D3}.txt' -f $_)), "$_", (New-Object System.Text.UTF8Encoding $false)) }
# Folder conflict: merge (nested inner.txt conflict), cancel, replace.
[IO.File]::WriteAllText((Join-Path $data 'left\Folder A\inner.txt'), 'new', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'left\Folder A\sub\deep.txt'), 'deep', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'right\Folder A\inner.txt'), 'old', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'right\Folder A\keep.txt'), 'keep', $utf8)
# This run's own settings: never the developer's app\data\config.json.
$config = @{ theme = 'dark'; places = @{ desktop = (Join-Path $data 'left\Folder A') } } | ConvertTo-Json
[IO.File]::WriteAllText((Join-Path $data 'config.json'), $config, $utf8)
[IO.File]::WriteAllText((Join-Path $data 'left\a.txt'), 'alpha', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'left\日本語 ファイル.txt'), 'ユニコード', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'left\b.txt'), 'bravo', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'explorer-src\ex_copy.txt'), 'from shell copy', $utf8)
[IO.File]::WriteAllText((Join-Path $data 'explorer-src\ex_cut.txt'), 'from shell cut', $utf8)

Add-Type -AssemblyName System.Windows.Forms
$savedText = $null
try { if ([Windows.Forms.Clipboard]::ContainsText()) { $savedText = [Windows.Forms.Clipboard]::GetText() } } catch {}
$log = Join-Path $root 'results\app-smoke.txt'
New-Item -ItemType Directory -Path (Split-Path $log) -Force | Out-Null
# CARGO_TARGET_DIR lets checks run while the usual build of the app is open.
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
$exe = Join-Path $target 'release\enyfile.exe'
try {
    $p = Start-Process -FilePath $exe -ArgumentList @('--smoke', "`"$data`"", "`"$log`"") -PassThru
    if (-not $p.WaitForExit(120000)) { $p.Kill(); throw 'Smoke timeout' }
} finally {
    try { if ($null -ne $savedText) { [Windows.Forms.Clipboard]::SetText($savedText) } else { [Windows.Forms.Clipboard]::Clear() } } catch {}
}
Get-Content -LiteralPath $log -Encoding UTF8
if (-not (Select-String -LiteralPath $log -Pattern '^PASS all$' -Quiet)) { throw 'Smoke failed' }
