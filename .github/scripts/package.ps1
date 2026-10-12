<#
.SYNOPSIS
    Assemble a release or dev archive from a finished build.

.DESCRIPTION
    Writes into -OutDir, for -Flavour release:

        mhworld-dlss-<tag>.zip        SHA256SUMS.txt

    and for -Flavour dev:

        mhworld-dlss-<tag>-dev.zip    SHA256SUMS-dev.txt

    Each archive holds one folder, to be extracted into the game folder:

        mhw-dlss/mhw-dlss-launcher.exe
        mhw-dlss/mhw-dlss.dll
        mhw-dlss/fetch-nvidia.cmd
        mhw-dlss/fetch-nvidia.ps1
        mhw-dlss/mhw-dlss.default.ini
        mhw-dlss/README.md
        mhw-dlss/LICENSE.txt
        mhw-dlss/THIRD-PARTY-NOTICES.md

    and the dev archive also

        mhw-dlss/DEV-BUILD.txt        what a dev build is, and how to reach it
        mhw-dlss/devctl/*.py          the dev-protocol client (tools/devctl)
        mhw-dlss/devctl/README.md

    install.ps1 depends on the release layout and file names; the folder
    name is $ModFolder below and in install.ps1. A dev archive is never a
    release asset: release.yml keeps it with the workflow run only.

    No settings file is included: the mod writes mhw-dlss.ini and the
    launcher mhw-dlss-loader.ini on first start, and shipping either would
    overwrite a user's own on every update. mhw-dlss.default.ini is the text
    the mod writes (Settings::write_default, printed by the launcher crate's
    default-settings example), for install.ps1 to start a settings file from.

    No NVIDIA file is ever included, the script fails if an archive holds
    anything but the files above, and a release archive is refused if its
    DLL carries the dev-build marker MHW_DLSS_DEV_BUILD.

    Runs on Windows PowerShell 5.1 and on PowerShell 7 on any OS:

        pwsh .github/scripts/package.ps1 -Tag v0.0.0-local
        pwsh .github/scripts/package.ps1 -Tag v0.0.0-local -Flavour dev -BinDir target/x86_64-pc-windows-gnu/release
#>
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [ValidateSet('release', 'dev')][string]$Flavour = 'release',
    # Where mhw_dlss.dll and mhw-dlss-launcher.exe are. Default: the
    # cross-build folder if it has them, else target/release.
    [string]$BinDir,
    # The settings file to ship as mhw-dlss.default.ini. Default: printed by
    # `cargo run -p mhw-dlss-launcher --example default-settings`.
    [string]$DefaultIni,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
function Get-RootPath([string]$Relative) { [System.IO.Path]::Combine($root, $Relative) }
function Resolve-Given([string]$Path) {
    # Relative to PowerShell's location, which is not always the process's.
    $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

if (-not $BinDir) {
    $BinDir = Get-RootPath 'target/x86_64-pc-windows-gnu/release'
    if (-not (Test-Path -LiteralPath ([System.IO.Path]::Combine($BinDir, 'mhw_dlss.dll')))) {
        $BinDir = Get-RootPath 'target/release'
    }
}
if (-not $OutDir) { $OutDir = Get-RootPath 'target/dist' }
$BinDir = Resolve-Given $BinDir
$OutDir = Resolve-Given $OutDir
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# The one folder an archive holds. The same name as $ModFolder in install.ps1.
$ModFolder = 'mhw-dlss'
$DevMarker = 'MHW_DLSS_DEV_BUILD'
$suffix = ''
if ($Flavour -eq 'dev') { $suffix = '-dev' }

# --- the default settings file ---------------------------------------------
$generatedIni = $false
if ($DefaultIni) {
    $DefaultIni = Resolve-Given $DefaultIni
} else {
    $DefaultIni = [System.IO.Path]::Combine($OutDir, 'mhw-dlss.default.ini')
    $cargoArgs = @('run', '--quiet', '--locked', '-p', 'mhw-dlss-launcher', '--example', 'default-settings')
    if ($Flavour -eq 'dev') { $cargoArgs += @('--features', 'dev') }
    Push-Location $root
    try {
        $text = (& cargo @cargoArgs) -join "`n"
        if ($LASTEXITCODE -ne 0) { throw "cargo $($cargoArgs -join ' ') failed" }
    } finally {
        Pop-Location
    }
    [System.IO.File]::WriteAllText($DefaultIni, ($text.TrimEnd() + "`n"), (New-Object System.Text.ASCIIEncoding))
    $generatedIni = $true
}
$iniText = [System.IO.File]::ReadAllText($DefaultIni)
foreach ($needle in '[General]', '[Upscaling]', '[FrameGeneration]') {
    if (-not $iniText.Contains($needle)) { throw "$DefaultIni has no $needle section; it is not the mod's settings file." }
}
if ($iniText -match '[^\x00-\x7F]') { throw "$DefaultIni is not plain ASCII." }
$hasDevSections = $iniText.Contains('[Dev]')
if ($hasDevSections -ne ($Flavour -eq 'dev')) {
    throw "$DefaultIni does not match the $Flavour flavour ([Dev] section present: $hasDevSections)."
}

# --- the layout: archive entry -> the file it is made from -----------------
$layout = [ordered]@{
    "$ModFolder/mhw-dlss-launcher.exe"  = [System.IO.Path]::Combine($BinDir, 'mhw-dlss-launcher.exe')
    "$ModFolder/mhw-dlss.dll"           = [System.IO.Path]::Combine($BinDir, 'mhw_dlss.dll')
    "$ModFolder/fetch-nvidia.cmd"       = Get-RootPath 'fetch-nvidia.cmd'
    "$ModFolder/fetch-nvidia.ps1"       = Get-RootPath 'fetch-nvidia.ps1'
    "$ModFolder/mhw-dlss.default.ini"   = $DefaultIni
    "$ModFolder/README.md"              = Get-RootPath 'README.md'
    "$ModFolder/LICENSE.txt"            = Get-RootPath 'LICENSE'
    "$ModFolder/THIRD-PARTY-NOTICES.md" = Get-RootPath 'THIRD-PARTY-NOTICES.md'
}
$devNote = $null
if ($Flavour -eq 'dev') {
    $devNote = [System.IO.Path]::Combine($OutDir, 'DEV-BUILD.txt')
    $note = @(
        "mhworld-dlss $Tag DEV BUILD - not a release."
        ''
        'This build contains the dev server (docs/DEV-PROTOCOL.md): a TCP listener'
        'that lets a client read and write game memory, change settings and'
        'capture frames. Use it for development only, never for normal play.'
        ''
        'Settings: mhw-dlss.ini gains the [Dev] and [Experimental] sections.'
        '  [Dev] Listen    = 127.0.0.1:47800   (0.0.0.0:47800 to reach it from the LAN)'
        '  [Dev] TokenFile = dev.token         (created on first start, in this folder)'
        ''
        'Client (Python 3.10+, in devctl/):'
        '  python devctl/devctl.py --host 127.0.0.1 --token-file dev.token hello'
        ''
        'From another machine: copy dev.token there, set Listen = 0.0.0.0:47800,'
        'allow the port in the Windows firewall, and pass --host <this PC>.'
        ''
        'See docs/DEVELOPMENT.md in the repository.'
    ) -join "`r`n"
    [System.IO.File]::WriteAllText($devNote, $note + "`r`n", (New-Object System.Text.ASCIIEncoding))
    $layout["$ModFolder/DEV-BUILD.txt"] = $devNote
    foreach ($file in 'devctl.py', 'devclient.py', 'capture.py', 'README.md') {
        $layout["$ModFolder/devctl/$file"] = Get-RootPath "tools/devctl/$file"
    }
}
foreach ($entry in $layout.Keys) {
    if (-not (Test-Path -LiteralPath $layout[$entry])) {
        throw "$($layout[$entry]) does not exist, so $entry cannot be packaged."
    }
}

# A release DLL must not be a dev build. The full string check is CI's
# (.github/scripts/check-dev-strings.sh); this guards the archive itself.
$dllText = [System.Text.Encoding]::GetEncoding(28591).GetString([System.IO.File]::ReadAllBytes($layout["$ModFolder/mhw-dlss.dll"]))
if ($Flavour -eq 'release' -and $dllText.Contains($DevMarker)) {
    throw "mhw_dlss.dll carries ${DevMarker}: it is a dev build and cannot go into a release archive."
}

# --- the archive -------------------------------------------------------------
$zipName = "mhworld-dlss-$Tag$suffix.zip"
$zip = [System.IO.Path]::Combine($OutDir, $zipName)
$sums = [System.IO.Path]::Combine($OutDir, "SHA256SUMS$suffix.txt")
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
$difference = @(Compare-Object -ReferenceObject @($layout.Keys) -DifferenceObject $found)
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
# that files somebody already extracted can be checked against the release.
$lines = @(
    "$(Get-Sha256 $zip)  $zipName"
    "$(Get-Sha256 $layout["$ModFolder/mhw-dlss-launcher.exe"])  $ModFolder/mhw-dlss-launcher.exe"
    "$(Get-Sha256 $layout["$ModFolder/mhw-dlss.dll"])  $ModFolder/mhw-dlss.dll"
)
[System.IO.File]::WriteAllText($sums, (($lines -join "`n") + "`n"), (New-Object System.Text.ASCIIEncoding))

if ($generatedIni) { Remove-Item -LiteralPath $DefaultIni -Force }
if ($devNote) { Remove-Item -LiteralPath $devNote -Force }

Write-Host "wrote $zip"
foreach ($name in $found) { Write-Host "  $name" }
Write-Host "wrote $sums"
foreach ($line in $lines) { Write-Host "  $line" }
