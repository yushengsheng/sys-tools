$ErrorActionPreference = 'Stop'
$taskRoot = $PSScriptRoot
$taskExe = Join-Path $taskRoot 'target\debug\systools.exe'
$taskExisting = Get-Process -Name systools -ErrorAction SilentlyContinue | Where-Object { $_.Path -and [StringComparer]::OrdinalIgnoreCase.Equals($_.Path, $taskExe) }
if ($taskExisting) {
    Write-Host 'sys tools is already running. Please use its existing window.'
    exit 0
}
try {
    Write-Host 'Building local test version...'
    & (Join-Path $taskRoot 'build.ps1')
    if (-not (Test-Path -LiteralPath $taskExe)) { throw 'Build did not produce target/debug/systools.exe.' }
    # This is the interactive test app, so its window is intentionally visible.
    Start-Process -FilePath $taskExe -WorkingDirectory $taskRoot | Out-Null
} catch {
    Write-Error $_
    exit 1
}

