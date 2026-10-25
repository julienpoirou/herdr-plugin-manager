# Produces bin\herdr-pm.exe for the plugin on Windows. See build.sh.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$want = (Select-String -Path herdr-plugin.toml -Pattern '^version\s*=\s*"(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force -Path bin | Out-Null

if ($env:HERDR_PM_BUILD_FROM_SOURCE -ne "1" -and (Test-Path bin\herdr-pm.exe)) {
    $have = ((& .\bin\herdr-pm.exe --version) -split " ")[1]
    if ($have -eq $want) {
        Write-Host "GitBridge: using prebuilt bin\herdr-pm.exe $have"
        exit 0
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "GitBridge: building needs Rust (https://rustup.rs), or a checkout that already contains bin\herdr-pm.exe $want."
    exit 1
}

if (-not $env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA "gitbridge\target"
}
Write-Host "GitBridge: building $want"
cargo build --release --locked --quiet
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\herdr-pm.exe") bin\herdr-pm.exe
Write-Host "GitBridge: built bin\herdr-pm.exe"
