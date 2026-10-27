param([switch]$Check, [switch]$Test)
$ErrorActionPreference = 'Stop'
$taskRoot = $PSScriptRoot
Push-Location -LiteralPath $taskRoot
try {
    $taskCargo = Get-Command cargo.exe -ErrorAction SilentlyContinue
    $bundledToolchain = Get-ChildItem -LiteralPath (Join-Path $taskRoot '.tools\rustup\toolchains') -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^stable-.*-windows-msvc$' -and (Test-Path -LiteralPath (Join-Path $_.FullName 'bin\cargo.exe')) } | Select-Object -First 1
    if ($bundledToolchain -and (Test-Path -LiteralPath (Join-Path $bundledToolchain.FullName 'bin\cargo.exe'))) {
        $env:RUSTUP_HOME = Join-Path $taskRoot '.tools\rustup'
        $taskCargoPath = Join-Path $bundledToolchain.FullName 'bin\cargo.exe'
    } elseif (Test-Path -LiteralPath (Join-Path $taskRoot '.tools\cargo\bin\cargo.exe')) {
        $env:CARGO_HOME = Join-Path $taskRoot '.tools\cargo'
        $env:RUSTUP_HOME = Join-Path $taskRoot '.tools\rustup'
        $taskCargoPath = Join-Path $env:CARGO_HOME 'bin\cargo.exe'
    } elseif ($taskCargo) {
        $taskCargoPath = $taskCargo.Source
    } else {
        throw 'Rust MSVC and Visual Studio C++ Build Tools are required.'
    }
    if ($Check) { & $taskCargoPath check --locked }
    elseif ($Test) { & $taskCargoPath test --locked --lib }
    else { & $taskCargoPath build --locked }
    if ($LASTEXITCODE -ne 0) { throw "Build or verification failed. Exit code: $LASTEXITCODE" }
} finally { Pop-Location }
