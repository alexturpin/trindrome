# Builds the zip to hand to friends: release binaries with the C runtime linked
# statically (no Visual C++ redistributable needed), the installer, and a
# configuration pointing at the coordinator.
#
#   powershell -File packaging\package.ps1 -CoordinatorUrl https://trindrome-coordinator.example.workers.dev
param([Parameter(Mandatory)][string]$CoordinatorUrl)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
if ($CoordinatorUrl -notmatch '^https://') { throw 'the coordinator URL must start with https://' }

$env:RUSTFLAGS = '-C target-feature=+crt-static'
$target = Join-Path $root 'target\package'
# Cargo reports progress on stderr, which Windows PowerShell would treat as an error.
$ErrorActionPreference = 'Continue'
$log = cargo build --release --locked --target-dir $target -p trindrome-worker -p trindrome-settings 2>&1 | ForEach-Object { "$_" }
$ErrorActionPreference = 'Stop'
$log | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
if ($log -match 'building without CUDA kernels') { throw 'nvcc was not found, so the build has no GPU kernels' }

$exes = 'trindrome.exe', 'trindrome-agent.exe', 'trindrome-settings.exe'
$release = Join-Path $target 'release'
$version = (& (Join-Path $release 'trindrome.exe') --version).Split(' ')[-1]
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist "trindrome-$version"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null

foreach ($exe in $exes) { Copy-Item (Join-Path $release $exe) $stage }
foreach ($file in 'install.ps1', 'install.cmd', 'README.txt') { Copy-Item (Join-Path $PSScriptRoot $file) $stage }
@"
# Written by the installer on first install; edit with trindrome-settings.
[coordinator]
url = "$($CoordinatorUrl.TrimEnd('/'))"
"@ | Set-Content -Encoding utf8 (Join-Path $stage 'trindrome.toml')

$zip = "$stage.zip"
Remove-Item $zip -Force -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Write-Host "Packaged $zip"
