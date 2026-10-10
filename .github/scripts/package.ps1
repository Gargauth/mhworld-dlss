<#
.SYNOPSIS
    Assemble the release archive from a finished build.

.DESCRIPTION
    Writes mhworld-dlss-<tag>.zip and SHA256SUMS.txt into -OutDir. The
    archive holds one folder, to be extracted into the game folder:

        mhw-dlss/mhw-dlss-launcher.exe
        mhw-dlss/mhw-dlss.dll
        mhw-dlss/fetch-nvidia.cmd
        mhw-dlss/fetch-nvidia.ps1
        mhw-dlss/mhw-dlss.default.ini
        mhw-dlss/README.md
        mhw-dlss/LICENSE.txt
        mhw-dlss/THIRD-PARTY-NOTICES.md

    install.ps1 depends on this layout and on both file names; the folder
    name is $ModFolder below and in install.ps1.

    No settings file is included: the mod writes mhw-dlss.ini and the
    launcher writes mhw-dlss-loader.ini on first start, and shipping either
    would overwrite a user's own on every update. mhw-dlss.default.ini is
    not one: it is the text the mod writes, taken from src/config.rs at
    packaging time, for the installer to start a settings file from when it
    has to set a key before the mod has ever run.

    No NVIDIA file is ever included, and the script fails if the archive
    holds anything but the eight files above.

    Used by .github/workflows/release.yml, and runnable by hand:

        powershell -NoProfile -ExecutionPolicy Bypass -File .github\scripts\package.ps1 -Tag v0.0.0-local
#>
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$BinDir,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $BinDir) { $BinDir = Join-Path $root 'target\release' }
if (-not $OutDir) { $OutDir = Join-Path $root 'target\dist' }
# Relative to PowerShell's location, which is not always the process's.
$BinDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($BinDir)
$OutDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutDir)

# The one folder a release holds. The same name as $ModFolder in install.ps1.
$ModFolder = 'mhw-dlss'

# The settings file as the mod writes it: the SHIPPED string of
# src/config.rs, which is a plain string literal opened with `"\` and closed
# by a line holding `";`. Anything that would need unescaping is refused, so
# that this cannot quietly produce a different text from the mod's.
$source = [System.IO.File]::ReadAllText((Join-Path $root 'src\config.rs')).Replace("`r`n", "`n")
$open = 'const SHIPPED: &str = "\' + "`n"
$from = $source.IndexOf($open)
if ($from -lt 0) { throw 'src/config.rs has no SHIPPED string where this script expects it.' }
$from += $open.Length
$to = $source.IndexOf("`n" + '";' + "`n", $from)
if ($to -lt 0) { throw 'the SHIPPED string in src/config.rs does not end where this script expects it to.' }
$shipped = $source.Substring($from, $to - $from) + "`n"
if ($shipped.Contains('\') -or $shipped.Contains('"')) {
    throw 'the SHIPPED string in src/config.rs now uses an escape or a quote; package.ps1 has to learn to read it.'
}
foreach ($key in 'render_scale = ', 'fg = 0', 'fg_multiplier = ') {
    if (-not ($shipped -split "`n" | Where-Object { $_.StartsWith($key) })) { throw "the text taken from src/config.rs has no '$key' line." }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$defaultIni = Join-Path $OutDir 'mhw-dlss.default.ini'
[System.IO.File]::WriteAllText($defaultIni, $shipped, (New-Object System.Text.ASCIIEncoding))

# Archive entry -> the file it is made from.
$layout = [ordered]@{
    "$ModFolder/mhw-dlss-launcher.exe"   = Join-Path $BinDir 'launcher.exe'
    "$ModFolder/mhw-dlss.dll"            = Join-Path $BinDir 'mhw_dlss.dll'
    "$ModFolder/fetch-nvidia.cmd"        = Join-Path $root 'fetch-nvidia.cmd'
    "$ModFolder/fetch-nvidia.ps1"        = Join-Path $root 'fetch-nvidia.ps1'
    "$ModFolder/mhw-dlss.default.ini"    = $defaultIni
    "$ModFolder/README.md"               = Join-Path $root 'README.md'
    "$ModFolder/LICENSE.txt"             = Join-Path $root 'LICENSE'
    "$ModFolder/THIRD-PARTY-NOTICES.md"  = Join-Path $root 'THIRD-PARTY-NOTICES.md'
}
foreach ($entry in $layout.Keys) {
    if (-not (Test-Path -LiteralPath $layout[$entry])) {
        throw "$($layout[$entry]) does not exist, so $entry cannot be packaged."
    }
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zipName = "mhworld-dlss-$Tag.zip"
$zip = Join-Path $OutDir $zipName
$sums = Join-Path $OutDir 'SHA256SUMS.txt'
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

# Entries are written one by one under names spelt here, with forward
# slashes, rather than by zipping a folder: Windows PowerShell 5.1's own
# Compress-Archive writes backslashes, which other tools read as file names.
$archive = [System.IO.Compression.ZipFile]::Open($zip, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($entry in $layout.Keys) {
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, $layout[$entry], $entry, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
    }
} finally {
    $archive.Dispose()
}

# Read it back: exactly these entries and no other, and nothing of NVIDIA's.
$archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
try {
    $found = @($archive.Entries | ForEach-Object { $_.FullName })
} finally {
    $archive.Dispose()
}
$expected = @($layout.Keys)
$difference = @(Compare-Object -ReferenceObject $expected -DifferenceObject $found)
if ($difference.Count -ne 0) {
    throw "the archive does not hold exactly the expected files: $($found -join ', ')"
}
foreach ($name in $found) {
    if (($name -split '/')[-1] -match '^(nvngx_.*|sl\..*)\.dll$') {
        throw "the archive contains $name. NVIDIA's libraries are never packaged."
    }
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# The archive's own line is the one install.ps1 looks up, by file name. The
# two programs are listed as well, under their names inside the archive, so
# that a file somebody already extracted can be checked against the release.
$lines = @(
    "$(Get-Sha256 $zip)  $zipName"
    "$(Get-Sha256 $layout["$ModFolder/mhw-dlss-launcher.exe"])  $ModFolder/mhw-dlss-launcher.exe"
    "$(Get-Sha256 $layout["$ModFolder/mhw-dlss.dll"])  $ModFolder/mhw-dlss.dll"
)
[System.IO.File]::WriteAllText($sums, (($lines -join "`n") + "`n"), (New-Object System.Text.ASCIIEncoding))

Remove-Item -LiteralPath $defaultIni -Force

Write-Host "wrote $zip"
foreach ($name in $found) { Write-Host "  $name" }
Write-Host "wrote $sums"
foreach ($line in $lines) { Write-Host "  $line" }
