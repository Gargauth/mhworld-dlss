<#
.SYNOPSIS
    Run fetch-nvidia.ps1 against scratch folders and check what it did.

.DESCRIPTION
    Never touches a real game folder: every run is given -GamePath into a
    temporary folder holding an empty MonsterHunterWorld.exe, and the download
    is replaced by a file:// URL pointing at an archive you already have.

    It needs the real Streamline SDK archive, because the script under test
    checks signatures and pinned hashes and nothing else gets past them.

.PARAMETER ZipPath
    streamline-sdk-v2.14.1.zip, already downloaded.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File tools\test-fetch-nvidia.ps1 -ZipPath C:\dl\streamline-sdk-v2.14.1.zip
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ZipPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$script  = Join-Path (Split-Path $PSScriptRoot -Parent) 'fetch-nvidia.ps1'
$zip     = (Resolve-Path -LiteralPath $ZipPath).Path
$zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
$zipSize = (Get-Item -LiteralPath $zip).Length
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("mhw-dlss-fetch-test-" + [Guid]::NewGuid().ToString('N'))
$failures = 0

function Invoke-Fetch([string]$Script, [string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    $output = & powershell -NoProfile -ExecutionPolicy Bypass -File $Script @Arguments 2>&1 | ForEach-Object { "$_" }
    @{ Code = $LASTEXITCODE; Text = ($output -join "`n") }
}

function Check([string]$What, [bool]$Holds) {
    if ($Holds) {
        Write-Host "  ok      $What"
    } else {
        Write-Host "  FAILED  $What" -ForegroundColor Red
        $script:failures++
    }
}

function Get-Fingerprint([string]$Folder) {
    Get-ChildItem -LiteralPath $Folder -Recurse -File | Sort-Object FullName |
        ForEach-Object { "$($_.FullName)=$((Get-FileHash -LiteralPath $_.FullName).Hash)" }
}

function New-Game([string]$Name) {
    $folder = Join-Path $scratch $Name
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
    Set-Content -LiteralPath (Join-Path $folder 'MonsterHunterWorld.exe') -Value ''
    $folder
}

try {
    New-Item -ItemType Directory -Path $scratch | Out-Null
    Write-Host "scratch: $scratch"

    Write-Host "1. a first install"
    $game = New-Game 'first'
    $run = Invoke-Fetch $script @('-GamePath', $game, '-ZipPath', $zip)
    Check 'exits 0' ($run.Code -eq 0)
    Check 'seven libraries are in place, all in the mod''s folder' (
        @(Get-ChildItem -LiteralPath $game -Recurse -Filter *.dll).Count -eq 7 -and
        @(Get-ChildItem -LiteralPath (Join-Path $game 'mhw-dlss') -Filter *.dll).Count -eq 7)
    Check 'no temporary or moved-aside file is left' (
        @(Get-ChildItem -LiteralPath $game -Recurse -File | Where-Object { $_.Name -match 'mhw-dlss-new|before-mhw-dlss' }).Count -eq 0)

    Write-Host "2. a second run changes nothing"
    $before = Get-Fingerprint $game
    $run = Invoke-Fetch $script @('-GamePath', $game, '-ZipPath', $zip)
    Check 'exits 0' ($run.Code -eq 0)
    Check 'says 0 changed' ($run.Text -match 'Done: 0 changed, 7 already up to date')
    Check 'the folder is identical' (-not (Compare-Object $before (Get-Fingerprint $game)))

    Write-Host "3. a file in the game folder itself is left alone"
    $foreign = Join-Path $game 'nvngx_dlssg.dll'
    Set-Content -LiteralPath $foreign -Value 'not ours'
    $foreignHash = (Get-FileHash -LiteralPath $foreign).Hash
    Add-Content -LiteralPath (Join-Path $game 'mhw-dlss\sl.common.dll') -Value 'x'
    $run = Invoke-Fetch $script @('-GamePath', $game, '-ZipPath', $zip)
    Check 'exits 0' ($run.Code -eq 0)
    Check 'the file in the game folder is untouched' ((Get-FileHash -LiteralPath $foreign).Hash -eq $foreignHash)
    Check 'says it left it alone' ($run.Text -match 'in the game folder itself is left alone')
    Check 'nothing was moved aside anywhere' (
        @(Get-ChildItem -LiteralPath $game -Recurse -File | Where-Object { $_.Name -match 'before-mhw-dlss' }).Count -eq 0)
    Check 'the changed file in the mod''s folder was replaced' ($run.Text -match 'Done: 1 changed')
    [System.IO.File]::Delete($foreign)

    Write-Host "4. a failure half-way leaves the folder as it was"
    Add-Content -LiteralPath (Join-Path $game 'mhw-dlss\sl.interposer.dll') -Value 'x'
    Add-Content -LiteralPath (Join-Path $game 'mhw-dlss\sl.pcl.dll') -Value 'x'
    $before = Get-Fingerprint $game
    $held = [System.IO.File]::Open((Join-Path $game 'mhw-dlss\sl.pcl.dll'), 'Open', 'Read', 'Read')
    try {
        $run = Invoke-Fetch $script @('-GamePath', $game, '-ZipPath', $zip)
    } finally {
        $held.Close()
    }
    Check 'exits 1' ($run.Code -eq 1)
    Check 'says it undid everything' ($run.Text -match 'Everything this run had done in the game folder was undone')
    Check 'the folder is identical to before the run' (-not (Compare-Object $before (Get-Fingerprint $game)))
    $run = Invoke-Fetch $script @('-GamePath', $game, '-ZipPath', $zip)
    Check 'and installs once the file is free' ($run.Code -eq 0 -and $run.Text -match 'Done: 2 changed')

    Write-Host "5. the script finds the game from the mod's folder, where it is shipped"
    $game = New-Game 'shipped'
    $folder = Join-Path $game 'mhw-dlss'
    New-Item -ItemType Directory -Path $folder | Out-Null
    Copy-Item -LiteralPath $script -Destination (Join-Path $folder 'fetch-nvidia.ps1')
    $run = Invoke-Fetch (Join-Path $folder 'fetch-nvidia.ps1') @('-ZipPath', $zip, '-DryRun')
    Check 'exits 0' ($run.Code -eq 0)
    Check 'names the folder above it' ($run.Text -match 'the folder above the one this script is in')
    Check 'a dry run adds nothing' (@(Get-ChildItem -LiteralPath $game -Recurse -File).Count -eq 2)

    Write-Host "5b. and from the game folder itself"
    $game = New-Game 'beside'
    Copy-Item -LiteralPath $script -Destination (Join-Path $game 'fetch-nvidia.ps1')
    $run = Invoke-Fetch (Join-Path $game 'fetch-nvidia.ps1') @('-ZipPath', $zip, '-DryRun')
    Check 'exits 0' ($run.Code -eq 0)
    Check 'names the folder it is in' ($run.Text -match 'the folder this script is in')
    Check 'a dry run adds nothing' (@(Get-ChildItem -LiteralPath $game -Recurse -File).Count -eq 2)

    Write-Host "6. a complete but wrong file in the cache is deleted and fetched again"
    $cache = Join-Path $scratch 'cache'
    New-Item -ItemType Directory -Path $cache | Out-Null
    $cached = Join-Path $cache 'streamline-sdk-v2.14.1.zip'
    $stream = [System.IO.File]::Create($cached); $stream.SetLength($zipSize); $stream.Close()
    $url = ([System.Uri]$zip).AbsoluteUri
    $run = Invoke-Fetch $script @('-GamePath', $game, '-CachePath', $cache, '-Url', $url, '-DryRun')
    Check 'exits 0' ($run.Code -eq 0)
    Check 'says it deleted the wrong file' ($run.Text -match 'deleted it and fetching again')
    Check 'the cache now holds the right archive' ((Get-FileHash -LiteralPath $cached).Hash -eq $zipHash)

    Write-Host "7. an interrupted download is carried on"
    $stream = [System.IO.File]::Open($cached, 'Open', 'ReadWrite'); $stream.SetLength(100000000); $stream.Close()
    $run = Invoke-Fetch $script @('-GamePath', $game, '-CachePath', $cache, '-Url', $url, '-DryRun')
    Check 'exits 0' ($run.Code -eq 0)
    Check 'says it carried on' ($run.Text -match 'carrying on from there')
    Check 'the cache holds the right archive' ((Get-FileHash -LiteralPath $cached).Hash -eq $zipHash)

    Write-Host "8. a download that arrives wrong is refused and not kept"
    [System.IO.File]::Delete($cached)
    $wrong = Join-Path $scratch 'wrong.zip'
    Set-Content -LiteralPath $wrong -Value 'not an archive'
    $run = Invoke-Fetch $script @('-GamePath', $game, '-CachePath', $cache, '-Url', ([System.Uri]$wrong).AbsoluteUri, '-DryRun')
    Check 'exits 1' ($run.Code -eq 1)
    Check 'the cache is empty afterwards' (-not (Test-Path -LiteralPath $cached))
} finally {
    if (Test-Path -LiteralPath $scratch) {
        [System.IO.Directory]::Delete($scratch, $true)
    }
}

Write-Host ""
if ($failures -gt 0) {
    Write-Host "$failures check(s) FAILED" -ForegroundColor Red
    exit 1
}
Write-Host "all checks passed"
