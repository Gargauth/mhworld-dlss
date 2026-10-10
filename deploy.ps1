# Developer convenience: build the mod and its launcher and copy both into
# the mod's folder inside the game folder, under the names they are released
# with.
#
#   target\release\mhw_dlss.dll  ->  <game>\mhw-dlss\mhw-dlss.dll
#   target\release\launcher.exe   ->  <game>\mhw-dlss\mhw-dlss-launcher.exe
#   fetch-nvidia.ps1, .cmd        ->  <game>\mhw-dlss\
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
# Usage:
#
#   .\deploy.ps1
#   .\deploy.ps1 -Game "D:\Games\Monster Hunter World"
param(
    [string]$Game = "C:\Program Files (x86)\Steam\steamapps\common\Monster Hunter World"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath (Join-Path $Game "MonsterHunterWorld.exe"))) {
    throw "MonsterHunterWorld.exe is not in '$Game'. Pass the game folder with -Game."
}
if (Get-Process MonsterHunterWorld -ErrorAction SilentlyContinue) {
    throw "Monster Hunter: World is running. Close it first."
}

Push-Location $PSScriptRoot
cargo build --release
$built = $LASTEXITCODE
Pop-Location
if ($built -ne 0) {
    throw "cargo build failed; nothing was installed."
}

$folder = Join-Path $Game "mhw-dlss"
if (-not (Test-Path -LiteralPath $folder)) {
    New-Item -ItemType Directory -Path $folder | Out-Null
}
Copy-Item "$PSScriptRoot\target\release\mhw_dlss.dll" "$folder\mhw-dlss.dll" -Force
Write-Host "installed -> $folder\mhw-dlss.dll"

Copy-Item "$PSScriptRoot\target\release\launcher.exe" "$folder\mhw-dlss-launcher.exe" -Force
Write-Host "installed -> $folder\mhw-dlss-launcher.exe"

Copy-Item "$PSScriptRoot\fetch-nvidia.ps1" "$folder\fetch-nvidia.ps1" -Force
Copy-Item "$PSScriptRoot\fetch-nvidia.cmd" "$folder\fetch-nvidia.cmd" -Force
Write-Host "installed -> $folder\fetch-nvidia.ps1, fetch-nvidia.cmd"

Write-Host "Steam launch option: `"$folder\mhw-dlss-launcher.exe`" %command%"
