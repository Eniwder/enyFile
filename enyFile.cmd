@echo off
rem Builds the app when sources changed (cargo skips it otherwise), then starts it.
setlocal
set "ROOT=%~dp0"
set "EXE=%ROOT%app\target\release\enyfile.exe"
set "OFFLINE="

rem The folder-local toolchain (.tools) when present, else the Rust on PATH.
if exist "%ROOT%.tools\cargo\bin\cargo.exe" (
    set "RUSTUP_HOME=%ROOT%.tools\rustup"
    set "CARGO_HOME=%ROOT%.tools\cargo"
    set "PATH=%ROOT%.tools\cargo\bin;%PATH%"
    set "OFFLINE=--offline"
)

rem A running copy of this exe locks it, so a rebuild would fail: just start another one.
rem (Only this build counts: an installed enyfile.exe elsewhere does not block it.)
powershell -NoProfile -Command "if (Get-Process enyfile -ErrorAction SilentlyContinue | Where-Object Path -eq $env:EXE) { exit 0 } else { exit 1 }" >nul 2>nul && goto run

where cargo >nul 2>nul || goto run
echo Building enyFile...
cargo build --release %OFFLINE% --manifest-path "%ROOT%app\Cargo.toml"
if errorlevel 1 (
    echo.
    echo Build failed.
    pause
    exit /b 1
)

:run
if not exist "%EXE%" (
    echo enyfile.exe not found. Install Rust: https://rustup.rs/
    pause
    exit /b 1
)
start "" "%EXE%" %*
