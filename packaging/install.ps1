# Installs or upgrades trindrome for the current Windows user, from the folder
# this script is in. No administrator rights needed. Run it through install.cmd.
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$data = Join-Path $env:LOCALAPPDATA 'Trindrome'
$bin = Join-Path $data 'bin'
$exes = 'trindrome.exe', 'trindrome-agent.exe', 'trindrome-settings.exe'

# An upgrade: let the running agent checkpoint and exit first.
if (Get-Process -Name 'trindrome-agent' -ErrorAction SilentlyContinue) {
    Write-Host 'Stopping the running agent...'
    & (Join-Path $bin 'trindrome.exe') stop | Out-Null
    $deadline = (Get-Date).AddSeconds(90)
    while ((Get-Process -Name 'trindrome-agent' -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 1
    }
    if (Get-Process -Name 'trindrome-agent' -ErrorAction SilentlyContinue) {
        throw 'the agent did not stop within 90 seconds; try again, or stop it from the tray icon'
    }
}

New-Item -ItemType Directory -Force $bin | Out-Null
foreach ($exe in $exes) {
    $target = Join-Path $bin $exe
    Copy-Item (Join-Path $here $exe) $target -Force
    Unblock-File $target
}

# Keep an existing configuration; a new install gets the coordinator's address.
$config = Join-Path $data 'trindrome.toml'
if (-not (Test-Path $config)) {
    Copy-Item (Join-Path $here 'trindrome.toml') $config
} elseif (-not (Select-String -Path $config -Pattern '^\s*\[coordinator\]' -Quiet)) {
    Write-Warning "$config has no [coordinator] section, so this machine searches on its own. See README.txt."
}

$cli = Join-Path $bin 'trindrome.exe'
& $cli startup enable
Start-Process (Join-Path $bin 'trindrome-agent.exe') -WindowStyle Hidden
Start-Sleep -Seconds 3
Write-Host ''
& $cli status
Write-Host ''
Write-Host 'Installed. trindrome computes only while this computer is idle and on AC power.'
Write-Host "Check on it any time with:  & `"$cli`" status"
