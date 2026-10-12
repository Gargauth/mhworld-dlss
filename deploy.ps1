# Developer convenience: build the mod and its launcher and copy both into
# the mod's folder inside the game folder, under the names they are released
# with.
#
#   <build>\mhw_dlss.dll           ->  <game>\mhw-dlss\mhw-dlss.dll
#   <build>\mhw-dlss-launcher.exe  ->  <game>\mhw-dlss\mhw-dlss-launcher.exe
#   fetch-nvidia.ps1, .cmd         ->  <game>\mhw-dlss\
#
# <build> is target\release, or target\dev\dev-release with -Dev: the dev build
# has its own target folder so that a dev DLL is never taken for a release,
# and its own profile (dev-release: panics unwind for the dev server).
#
# Everything the mod installs or writes lives in <game>\mhw-dlss: its
# library, the launcher, the NVIDIA libraries, the settings file, the chain
# file and the logs. None of the last three is touched here.
#
# The NVIDIA files are not built here and not shipped: run fetch-nvidia.cmd in
# that folder. The Steam launch option is
#
#   "<game>\mhw-dlss\mhw-dlss-launcher.exe" %command%
#
# -Dev deploys the dev build (docs/DEVELOPMENT.md): the dev server, probes
# and the [Dev]/[Experimental] settings. It prints how to reach it with
# tools/devctl. Deploy without -Dev to go back to the release build.
#
# From Linux, build with tools/xbuild.sh [release|dev] and copy the files by
# hand; this script is for a Windows checkout.
#
# Usage:
#
#   .\deploy.ps1
#   .\deploy.ps1 -Dev
#   .\deploy.ps1 -Game "D:\Games\Monster Hunter World"
param(
    [string]$Game = "C:\Program Files (x86)\Steam\steamapps\common\Monster Hunter World",
    [switch]$Dev
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath (Join-Path $Game "MonsterHunterWorld.exe"))) {
    throw "MonsterHunterWorld.exe is not in '$Game'. Pass the game folder with -Game."
}
if (Get-Process MonsterHunterWorld -ErrorAction SilentlyContinue) {
    throw "Monster Hunter: World is running. Close it first."
}

$cargoArgs = @('build', '-p', 'mhw-dlss', '-p', 'mhw-dlss-launcher')
$built = Join-Path $PSScriptRoot 'target\release'
if ($Dev) {
    $cargoArgs += @('--profile', 'dev-release', '--features', 'dev', '--target-dir', 'target\dev')
    $built = Join-Path $PSScriptRoot 'target\dev\dev-release'
} else {
    $cargoArgs += @('--release')
}
Push-Location $PSScriptRoot
try {
    & cargo @cargoArgs
    $status = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($status -ne 0) {
    throw "cargo build failed; nothing was installed."
}

$folder = Join-Path $Game "mhw-dlss"
if (-not (Test-Path -LiteralPath $folder)) {
    New-Item -ItemType Directory -Path $folder | Out-Null
}
Copy-Item (Join-Path $built 'mhw_dlss.dll') (Join-Path $folder 'mhw-dlss.dll') -Force
Write-Host "installed -> $folder\mhw-dlss.dll"

Copy-Item (Join-Path $built 'mhw-dlss-launcher.exe') (Join-Path $folder 'mhw-dlss-launcher.exe') -Force
Write-Host "installed -> $folder\mhw-dlss-launcher.exe"

Copy-Item "$PSScriptRoot\fetch-nvidia.ps1" "$folder\fetch-nvidia.ps1" -Force
Copy-Item "$PSScriptRoot\fetch-nvidia.cmd" "$folder\fetch-nvidia.cmd" -Force
Write-Host "installed -> $folder\fetch-nvidia.ps1, fetch-nvidia.cmd"

Write-Host "Steam launch option: `"$folder\mhw-dlss-launcher.exe`" %command%"

if ($Dev) {
    # Where the dev server listens and where its token is: [Dev] Listen and
    # [Dev] TokenFile in mhw-dlss.ini, with the mod's defaults when the file
    # or the key is not there yet. TokenFile is relative to the mod folder.
    $listen = '127.0.0.1:47800'
    $tokenFile = 'dev.token'
    $ini = Join-Path $folder 'mhw-dlss.ini'
    if (Test-Path -LiteralPath $ini) {
        $section = ''
        foreach ($line in Get-Content -LiteralPath $ini) {
            $trimmed = $line.Trim()
            if ($trimmed -match '^\[\s*([^\]]*?)\s*\]') { $section = $Matches[1]; continue }
            if ($section -ine 'Dev' -or $trimmed -notmatch '^([A-Za-z]+)\s*=\s*(.*?)\s*(\s[;#].*)?$') { continue }
            $value = $Matches[2].Trim('"')
            if ($Matches[1] -ieq 'Listen' -and $value) { $listen = $value }
            if ($Matches[1] -ieq 'TokenFile' -and $value) { $tokenFile = $value }
        }
    }
    if (-not [System.IO.Path]::IsPathRooted($tokenFile)) { $tokenFile = Join-Path $folder $tokenFile }
    $hostPart, $port = $listen -split ':(?=[0-9]+$)'
    if (-not $port) { $port = '47800' }
    $devctl = Join-Path $PSScriptRoot 'tools\devctl\devctl.py'

    Write-Host ""
    Write-Host "DEV BUILD deployed. The dev server starts with the game and listens on $listen."
    Write-Host "Its token file is written on first start: $tokenFile"
    Write-Host ""
    Write-Host "Connect from this machine:"
    Write-Host ("    python `"{0}`" --host 127.0.0.1 --port {1} --token-file `"{2}`" hello" -f $devctl, $port, $tokenFile) -ForegroundColor Cyan
    if ($hostPart -eq '0.0.0.0') {
        $addresses = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
            Where-Object { $_.IPAddress -notmatch '^(127\.|169\.254\.)' } |
            ForEach-Object { $_.IPAddress })
        Write-Host ""
        Write-Host "From the LAN (copy the token file to that machine; allow TCP $port in the firewall):"
        foreach ($address in $addresses) {
            Write-Host ("    python devctl.py --host {0} --port {1} --token-file dev.token hello" -f $address, $port) -ForegroundColor Cyan
        }
    } else {
        Write-Host ""
        Write-Host "For the LAN: set [Dev] Listen = 0.0.0.0:$port in $ini, restart the game,"
        Write-Host "and run this again to see the addresses to connect to."
    }
}
