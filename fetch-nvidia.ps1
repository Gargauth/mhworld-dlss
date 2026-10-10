<#
.SYNOPSIS
    Fetch the NVIDIA runtime libraries mhw-dlss needs, from NVIDIA, and put
    them where the mod looks for them.

.DESCRIPTION
    mhw-dlss is GPL-3.0 and does not bundle any NVIDIA binary. This script
    downloads one file, the Streamline SDK release archive that NVIDIA publish
    on GitHub, takes seven libraries out of it, checks each one, and copies
    them into the mod's folder, <game>\mhw-dlss, where this script itself
    lives with the rest of the mod. Nothing is downloaded from anywhere else
    and nothing is modified: the files are byte copies of what NVIDIA signed.

    The easiest way to run it is to double-click fetch-nvidia.cmd, which sits
    beside it and keeps the window open.

    WHAT GOES WHERE

      <game>\mhw-dlss\   sl.interposer.dll  sl.common.dll   sl.dlss_g.dll
                          sl.reflex.dll      sl.pcl.dll
                          nvngx_dlssg.dll    nvngx_dlss.dll

    Nothing is put in the game folder itself. NGX looks for a snippet in the
    directory of the module that created the NGX context. Streamline's context
    is created by sl.common.dll in <game>\mhw-dlss, and the mod's own by the
    mod's DLL, which is in that folder too (<game>\mhw-dlss\mhw-dlss.dll),
    so both find the copies there.

    A <game>\nvngx_dlssg.dll that is already in the game folder is left alone.
    Earlier versions of this script put a second copy there, for a mod DLL
    that sat in the game folder; with the DLL in mhw-dlss the test program in
    tools\standin loads the frame generation snippet without it. If frame
    generation ever fails with nothing but a line in mhw-dlss\nvngx.log,
    copying mhw-dlss\nvngx_dlssg.dll into the game folder is the thing to try.

    nvngx_dlisp.dll must NOT be in <game>\mhw-dlss. The game's own 2019 copy
    in the game root is left alone. A copy in the plugin folder is initialised
    by NGX on Streamline's behalf, faults on current GPUs and takes Streamline's
    start-up down with it. If one is found there it is renamed to
    nvngx_dlisp.dll.disabled and the script says so.

    The game's own <game>\nvngx_dlss.dll (DLSS 1.1) is never touched.

    HOW THE FILES ARE INSTALLED

    All of them or none. Every file is first copied into its destination
    folder under a temporary name and checked there. Only when all of them are
    in place are they given their real names. If anything fails on the way,
    what was done is undone and the folder is as it was.

    Inside <game>\mhw-dlss, which is this mod's own folder, an older file is
    replaced. Nothing outside that folder is written, renamed or deleted.

    WHAT IT LEAVES ON DISK

    The downloaded archive (about 276 MB) is kept in
    %LOCALAPPDATA%\mhw-dlss\cache so a second run does not download it
    again. It can be deleted at any time.

    WHY THE STREAMLINE ARCHIVE AND NOT THE NVIDIA/DLSS REPOSITORY

    nvngx_dlss.dll is published in two places: lib\Windows_x86_64\rel in
    github.com/NVIDIA/DLSS, and bin\x64 of the Streamline SDK archive. For the
    pinned pair (DLSS 310.9.1, Streamline 2.14.1) the two files were compared
    and are byte-identical (same SHA-256, below). The archive is needed anyway
    for the six Streamline files, so taking the snippet from it means one
    download, one pinned version, one integrity check, and a super resolution
    snippet NVIDIA shipped together with that frame generation snippet. The
    cost is size: the archive is about 276 MB and 68 MB of it is used.

    LICENCE TERMS (read from the archive itself; not legal advice)

    The archive carries three licence texts that apply to what is copied. The
    script copies all three next to the DLLs so you have them.

    * license.txt (archive root): the MIT licence, "Copyright (c) 2023 NVIDIA
      CORPORATION". It covers the Streamline framework, with one stated
      exception (sl_nvperf, under the Nsight Perf SDK licence, not used here).
      sl.interposer.dll, sl.common.dll, sl.reflex.dll and sl.pcl.dll are
      NVIDIA's signed builds of that source. MIT permits use, copying and
      redistribution with the notice kept.
    * sl.dlss_g.dll is the one Streamline plugin with no source: the SDK's
      README says it "is provided as prebuilt DLLs only". No file in the
      archive says in so many words which licence governs that binary. The
      root MIT text does not exclude it, but this project does not rely on
      that reading and treats it like the snippets below.
    * bin\x64\nvngx_dlss.license.txt: the "NVIDIA RTX SDKs License"
      (v. March 14, 2024), byte-identical to LICENSE.txt in NVIDIA/DLSS and to
      external\ngx-sdk\license.txt in the archive. It governs nvngx_dlss.dll
      and nvngx_dlssg.dll. It grants the right to "Install and use the SDK"
      (1a) and to distribute it only "as incorporated in object code format
      into a software application" (1c) that has "material additional
      functionality" (2a), under terms "at least as protective" as NVIDIA's
      (2c). It forbids distributing the SDK "as a stand-alone product" (4b)
      and using it "in any manner that would cause it to become subject to an
      open source software license" (4e). Its supplement licenses the DLSS and
      NGX SDKs only "for their use in systems with NVIDIA GPUs" (supplement 1)
      and lets NVIDIA update the SDK over the air (supplement 6.1).
    * bin\x64\reflex.license.txt: NVIDIA's SDK licence with the Reflex
      supplement. Same shape: install and use, distribution only inside an
      application, clause 2.5 against open source licensing.

    What that means here: a GPL-3.0 project cannot put these files in its
    repository or its release archive without running into 4b, 4e and 2c. So it
    does not. You download them from NVIDIA yourself, with this script or by
    hand, which is the "install and use" the licences grant to you directly.
    By running this script you are accepting NVIDIA's terms for these files.
    Do not redistribute the game folder's mhw-dlss directory.

.PARAMETER GamePath
    The Monster Hunter: World folder. When omitted, the script looks at where
    it is: in <game>\mhw-dlss, which is where it is shipped, or in the game
    folder itself. Otherwise the game is found from Steam's library
    configuration.

.PARAMETER ZipPath
    Use a Streamline SDK archive you already downloaded instead of downloading
    it. It is checked exactly as a downloaded one would be.

.PARAMETER Force
    Replace a file in the game folder even when it is NEWER than the pinned
    one. Without this a newer file is kept and reported.

.PARAMETER DryRun
    Do everything except change the game folder: download, verify, and print
    what would be copied or renamed.

.PARAMETER KeepStaging
    Do not delete the temporary folder the files are extracted to.

.PARAMETER CachePath
    Where the downloaded archive is kept, instead of
    %LOCALAPPDATA%\mhw-dlss\cache. For testing this script.

.PARAMETER Url
    Where to download the archive from, instead of NVIDIA's GitHub release.
    For testing this script; the archive is checked against the pinned
    SHA-256 wherever it came from.

.EXAMPLE
    .\fetch-nvidia.ps1

.EXAMPLE
    .\fetch-nvidia.ps1 -GamePath "D:\SteamLibrary\steamapps\common\Monster Hunter World" -DryRun
#>
[CmdletBinding()]
param(
    [string]$GamePath,
    [string]$ZipPath,
    [switch]$Force,
    [switch]$DryRun,
    [switch]$KeepStaging,
    [string]$CachePath,
    [string]$Url
)

# ---------------------------------------------------------------------------
# THE PINNED VERSION. Change it here and nowhere else.
#
# 2.14.1 is not a preference. 2.14.0 fixed two sl.dlss_g bugs that are this
# game's swapchain exactly: a stall when the application owns the frame-latency
# waitable object, and a missing ALLOW_TEARING on the proxy swapchain. Older
# SDKs are expected to hang or show a black screen. Newer ones are untested.
# ---------------------------------------------------------------------------
$StreamlineVersion = '2.14.1'

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$StreamlineUrl = "https://github.com/NVIDIA-RTX/Streamline/releases/download/v$StreamlineVersion/streamline-sdk-v$StreamlineVersion.zip"
if ($Url) { $StreamlineUrl = $Url }
$SteamAppId    = '582010'   # Monster Hunter: World
$GameExe       = 'MonsterHunterWorld.exe'
$PluginFolder  = 'mhw-dlss'

# SHA-256 of the archive and of each file in it, for the versions this project
# has measured. A version that is not in this table is still checked for a
# valid NVIDIA signature, and the script says the hashes were not compared.
$Pinned = @{
    '2.14.1' = @{
        Zip   = '92C4D954631A1710DA86CA3FA8D5034F2B9503838C95FC4AE977AE149319781B'
        # In bytes. A cached file shorter than this is a download that was
        # interrupted and can be resumed; one this long or longer with the
        # wrong hash cannot, and is deleted.
        Size  = 275994000
        Files = @{
            'sl.interposer.dll' = '8C87C9499461DA561EDD529AA9BF7831D67D7B94EBB1C1A5ED54EF4934E1EA4C'
            'sl.common.dll'     = '82924A8954DD671E09351C5DE0EB87AD0EB25B944CC9F9AB955CA1D9950DE15D'
            'sl.dlss_g.dll'     = 'F4A6B2B14DCC0B1485989E430D3B4E3A44AC1800B92BA1AD74F476E64FB2B09C'
            'sl.reflex.dll'     = '0CE9725E3E03EA9E7F81D008B57F33EE365973D2E349131C8B1C3E3378FE2DB0'
            'sl.pcl.dll'        = 'F13D51CFA05F4CD514DF2026049E2DB8ADF359221713170AD386FD499915B582'
            'nvngx_dlssg.dll'   = 'FF6E90EB78B827927DFF5B4ECC6B1C870C2E9BCA29ED9F48C7D348CC9E170B82'
            # Identical to lib/Windows_x86_64/rel/nvngx_dlss.dll at tag
            # v310.9.1 of github.com/NVIDIA/DLSS.
            'nvngx_dlss.dll'    = '3975567B8943C53ACCE397F2B72380092F84F162D00B0D2C7D08A1025C563983'
        }
    }
}

# Archive entry -> where it goes, relative to the game folder. Everything goes
# into the mod's own folder ('.' would be the game folder itself, which nothing
# uses). Only production builds: bin/x64, never bin/x64/development.
$Wanted = @(
    @{ Entry = 'bin/x64/sl.interposer.dll'; Name = 'sl.interposer.dll'; To = @($PluginFolder) }
    @{ Entry = 'bin/x64/sl.common.dll';     Name = 'sl.common.dll';     To = @($PluginFolder) }
    @{ Entry = 'bin/x64/sl.dlss_g.dll';     Name = 'sl.dlss_g.dll';     To = @($PluginFolder) }
    @{ Entry = 'bin/x64/sl.reflex.dll';     Name = 'sl.reflex.dll';     To = @($PluginFolder) }
    @{ Entry = 'bin/x64/sl.pcl.dll';        Name = 'sl.pcl.dll';        To = @($PluginFolder) }
    @{ Entry = 'bin/x64/nvngx_dlssg.dll';   Name = 'nvngx_dlssg.dll';   To = @($PluginFolder) }
    @{ Entry = 'bin/x64/nvngx_dlss.dll';    Name = 'nvngx_dlss.dll';    To = @($PluginFolder) }
)

# Licence texts, copied beside the DLLs so the terms travel with the files.
$Licences = @(
    @{ Entry = 'license.txt';                    Name = 'streamline.license.txt' }
    @{ Entry = 'bin/x64/nvngx_dlss.license.txt'; Name = 'nvngx_dlss.license.txt' }
    @{ Entry = 'bin/x64/reflex.license.txt';     Name = 'reflex.license.txt' }
)

function Say([string]$Text)  { Write-Host $Text }
function Note([string]$Text) { Write-Host "  $Text" }
function Warn([string]$Text) { Write-Host "  ! $Text" -ForegroundColor Yellow }
function Refuse([string]$Text) {
    Write-Host ""
    Write-Host "REFUSED: $Text" -ForegroundColor Red
    Write-Host "Nothing in the game folder was changed by this step."
    exit 1
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Get-DllVersion([string]$Path) {
    $info = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
    New-Object System.Version($info.FileMajorPart, $info.FileMinorPart, $info.FileBuildPart, $info.FilePrivatePart)
}

# --- find the game ---------------------------------------------------------

function Get-SteamRoots {
    $roots = @()
    foreach ($key in @('HKCU:\Software\Valve\Steam', 'HKLM:\SOFTWARE\WOW6432Node\Valve\Steam', 'HKLM:\SOFTWARE\Valve\Steam')) {
        $item = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
        if ($null -eq $item) { continue }
        foreach ($name in @('SteamPath', 'InstallPath')) {
            $property = $item.PSObject.Properties[$name]
            if ($null -ne $property -and $property.Value) {
                $roots += ([string]$property.Value).Replace('/', '\').TrimEnd('\')
            }
        }
    }
    $roots | Sort-Object -Unique
}

function Get-SteamLibraries([string]$SteamRoot) {
    $libraries = @($SteamRoot)
    $vdf = Join-Path $SteamRoot 'steamapps\libraryfolders.vdf'
    if (Test-Path -LiteralPath $vdf) {
        foreach ($line in Get-Content -LiteralPath $vdf) {
            if ($line -match '^\s*"path"\s+"(.+)"\s*$') {
                $libraries += $Matches[1].Replace('\\', '\').TrimEnd('\')
            }
        }
    }
    $libraries | Sort-Object -Unique
}

function Find-Game {
    foreach ($root in Get-SteamRoots) {
        foreach ($library in Get-SteamLibraries $root) {
            $manifest = Join-Path $library "steamapps\appmanifest_$SteamAppId.acf"
            if (-not (Test-Path -LiteralPath $manifest)) { continue }
            $folder = 'Monster Hunter World'
            foreach ($line in Get-Content -LiteralPath $manifest) {
                if ($line -match '^\s*"installdir"\s+"(.+)"\s*$') { $folder = $Matches[1] }
            }
            $candidate = Join-Path $library "steamapps\common\$folder"
            if (Test-Path -LiteralPath (Join-Path $candidate $GameExe)) {
                return $candidate
            }
        }
    }
    return $null
}

Say "mhw-dlss: fetching the NVIDIA runtime (Streamline SDK $StreamlineVersion)"
Say ""

if ($GamePath) {
    $Game = $GamePath.TrimEnd('\')
    $how = 'given with -GamePath'
} elseif ($PSScriptRoot -and ((Split-Path -Leaf $PSScriptRoot) -ieq $PluginFolder) -and
          (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $PSScriptRoot) $GameExe))) {
    # Before asking Steam. The script is shipped in <game>\mhw-dlss, and
    # somebody with two installs means the one they put it in.
    $Game = (Split-Path -Parent $PSScriptRoot).TrimEnd('\')
    $how = "the folder above the one this script is in, which holds $GameExe"
} elseif ($PSScriptRoot -and (Test-Path -LiteralPath (Join-Path $PSScriptRoot $GameExe))) {
    $Game = $PSScriptRoot.TrimEnd('\')
    $how = "the folder this script is in, which holds $GameExe"
} else {
    $Game = Find-Game
    $how = "found from Steam's library configuration (app $SteamAppId)"
    if (-not $Game) {
        Refuse "could not find Monster Hunter: World in any Steam library. Run this script from the game's mhw-dlss folder, or pass the game folder with -GamePath."
    }
}
if (-not (Test-Path -LiteralPath (Join-Path $Game $GameExe))) {
    Refuse "$Game does not contain $GameExe, so it is not the game folder."
}
Say "game folder : $Game"
Note $how

# Only a game running out of THIS folder is in the way. A process whose path
# cannot be read is assumed to be it, because guessing wrong the other way
# means copying over libraries that are mapped into a running game.
if (-not $DryRun) {
    $expected = Join-Path $Game $GameExe
    foreach ($process in @(Get-Process -Name 'MonsterHunterWorld' -ErrorAction SilentlyContinue)) {
        $running = $null
        try { $running = $process.Path } catch { $running = $null }
        if (-not $running -or $running -ieq $expected) {
            Refuse "Monster Hunter: World is running from this folder (pid $($process.Id)). Close it first; its libraries are locked while it runs."
        }
        Note "a game is running from $running, which is a different folder; carrying on"
    }
}

# --- get the archive -------------------------------------------------------

$pin = $null
if ($Pinned.ContainsKey($StreamlineVersion)) { $pin = $Pinned[$StreamlineVersion] }

if ($ZipPath) {
    if (-not (Test-Path -LiteralPath $ZipPath)) { Refuse "-ZipPath $ZipPath does not exist." }
    $zip = (Resolve-Path -LiteralPath $ZipPath).Path
    Say "archive     : $zip"
    Note "given with -ZipPath; not downloaded"
} else {
    $cache = Join-Path $env:LOCALAPPDATA 'mhw-dlss\cache'
    if ($CachePath) { $cache = $CachePath }
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    $zip = Join-Path $cache "streamline-sdk-v$StreamlineVersion.zip"
    Say "archive     : $zip"


    $have = $false
    $resume = $false
    if (Test-Path -LiteralPath $zip) {
        $length = (Get-Item -LiteralPath $zip).Length
        if ($pin -and (Get-Sha256 $zip) -eq $pin.Zip) {
            Note "already downloaded, and its SHA-256 matches the pinned one"
            $have = $true
        } elseif ($pin -and $length -lt $pin.Size) {
            Note "a previous download stopped at $length of $($pin.Size) bytes; carrying on from there"
            $resume = $true
        } elseif ($pin) {
            # Complete and wrong. Asking the server to carry on from the end
            # of it gets nothing, so the same wrong file would be refused on
            # every run until somebody deleted it by hand.
            Remove-Item -LiteralPath $zip -Force
            Note "a previous download is $length bytes with the wrong SHA-256; deleted it and fetching again"
        } else {
            Note "already downloaded (no pinned hash for $StreamlineVersion to compare it with)"
            $have = $true
        }
    }
    if (-not $have) {
        Note "downloading $StreamlineUrl"
        Note "about 276 MB, from NVIDIA's GitHub release"
        $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
        if ($curl) {
            # --fail turns an HTTP error into an exit code. -C - carries on a
            # partial file, and is only given when there is one.
            $options = @('-L', '--fail', '--retry', '3')
            if ($resume) { $options += @('-C', '-') }
            & $curl.Source @options -o $zip $StreamlineUrl
            if ($LASTEXITCODE -ne 0) {
                Refuse "the download failed (curl exit code $LASTEXITCODE). Run this again to carry on from where it stopped."
            }
        } else {
            # No resume this way, so never on top of a partial file.
            if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
            $previous = $ProgressPreference
            $ProgressPreference = 'SilentlyContinue'   # the progress bar makes 5.1 crawl
            try {
                [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
                Invoke-WebRequest -Uri $StreamlineUrl -OutFile $zip -UseBasicParsing
            } finally {
                $ProgressPreference = $previous
            }
        }
    }
}

$zipHash = Get-Sha256 $zip
if ($pin) {
    if ($zipHash -ne $pin.Zip) {
        $what = "the archive's SHA-256 is $zipHash, and the one measured for $StreamlineVersion is $($pin.Zip). It is not the file this project was tested with."
        if ($ZipPath) { Refuse $what }
        # Ours to delete, and it has to go: left in the cache it would be
        # found again on the next run.
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
        Refuse "$what The downloaded file has been deleted from the cache; run this again to download it afresh."
    }
    Note "SHA-256 matches the pinned archive: $zipHash"
} else {
    Warn "no pinned hashes for Streamline $StreamlineVersion; only the signatures are checked. SHA-256 $zipHash"
}

# --- extract only what is needed, into a fresh folder ----------------------

Add-Type -AssemblyName System.IO.Compression.FileSystem
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("mhw-dlss-fetch-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null

$failed = $false
try {
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        foreach ($item in ($Wanted + $Licences)) {
            $entry = $archive.GetEntry($item.Entry)
            if ($null -eq $entry) {
                Refuse "the archive has no $($item.Entry). It is not a Streamline SDK release archive, or its layout changed."
            }
            # The destination name is ours, never the entry's, so a hostile
            # archive cannot write outside the staging folder.
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $staging $item.Name), $true)
        }
    } finally {
        $archive.Dispose()
    }

    # --- verify every DLL before anything is copied ------------------------

    Say ""
    Say "verifying   : Authenticode signature, signer, and pinned SHA-256"
    $problems = @()
    $sources = @{}
    foreach ($item in $Wanted) {
        $path = Join-Path $staging $item.Name
        $signature = Get-AuthenticodeSignature -LiteralPath $path
        $subject = ''
        if ($signature.SignerCertificate) { $subject = $signature.SignerCertificate.Subject }
        $hash = Get-Sha256 $path
        $version = Get-DllVersion $path
        $description = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($path).FileDescription

        $ok = $true
        if ($signature.Status -ne 'Valid') {
            $problems += "$($item.Name): signature status is '$($signature.Status)', not Valid"
            $ok = $false
        }
        # Both the common name and the organisation, as whole components.
        if ($subject -notmatch '(^|,\s*)CN=NVIDIA Corporation(,|$)' -or $subject -notmatch '(^|,\s*)O=NVIDIA Corporation(,|$)') {
            $problems += "$($item.Name): signed by '$subject', which is not NVIDIA Corporation"
            $ok = $false
        }
        if ($pin) {
            if (-not $pin.Files.ContainsKey($item.Name) -or $pin.Files[$item.Name] -ne $hash) {
                $problems += "$($item.Name): SHA-256 $hash is not the one measured for $StreamlineVersion"
                $ok = $false
            }
        }
        if ($item.Name -like 'sl.*' -and $description -notmatch 'PRODUCTION') {
            $problems += "$($item.Name): describes itself as '$description', which is not a production build"
            $ok = $false
        }
        $mark = 'ok    '
        if (-not $ok) { $mark = 'FAILED' }
        Note ("{0}  {1,-18} {2,-12} {3}" -f $mark, $item.Name, $version, $description)
        $sources[$item.Name] = @{ Path = $path; Hash = $hash; Version = $version }
    }
    if ($problems.Count -gt 0) {
        Say ""
        foreach ($problem in $problems) { Warn $problem }
        Refuse "one or more files failed verification, so none of them is copied."
    }
    Note "all $($Wanted.Count) are validly signed by NVIDIA Corporation"

    # --- the one file that must not be there -------------------------------

    $plugin = Join-Path $Game $PluginFolder
    $changed = 0
    $kept = 0
    $current = 0

    Say ""
    Say "plugin folder: $plugin"
    if (-not (Test-Path -LiteralPath $plugin)) {
        if ($DryRun) {
            Note "would create it"
        } else {
            New-Item -ItemType Directory -Path $plugin | Out-Null
            Note "created"
        }
    }
    $rootCopy = Join-Path $Game 'nvngx_dlssg.dll'
    if (Test-Path -LiteralPath $rootCopy) {
        Note "nvngx_dlssg.dll in the game folder itself is left alone; this script no longer installs or updates one there"
    }

    $hazard = Join-Path $plugin 'nvngx_dlisp.dll'
    if (Test-Path -LiteralPath $hazard) {
        $aside = "$hazard.disabled"
        if (Test-Path -LiteralPath $aside) {
            $aside = "$hazard.disabled-" + (Get-Date -Format 'yyyyMMdd-HHmmss')
        }
        if ($DryRun) {
            Warn "nvngx_dlisp.dll is in the plugin folder; it would be renamed to $(Split-Path $aside -Leaf)"
        } else {
            Rename-Item -LiteralPath $hazard -NewName (Split-Path $aside -Leaf)
            Warn "nvngx_dlisp.dll was in the plugin folder and has been renamed to $(Split-Path $aside -Leaf)."
            $changed++
        }
        Note "Why: NGX initialises every snippet in the folder Streamline is given, the game's"
        Note "2019 sharpening snippet faults on current GPUs, and that fault aborts Streamline's"
        Note "start-up, so frame generation never loads. The game's own copy in the game root"
        Note "is what the game needs, and it has not been touched."
    } else {
        Note "no nvngx_dlisp.dll in the plugin folder (good; the game root's copy is left alone)"
    }

    # --- install: all of it, or none of it ---------------------------------
    #
    # Three passes. Decide what each destination needs. Put every new file in
    # its destination folder under a temporary name and check it there. Then
    # give them their real names, moving aside whatever was in the way. A
    # failure in the second pass has changed nothing that is in use; a failure
    # in the third undoes what the third had done. Either way the folder never
    # ends up holding some files of one Streamline version and some of
    # another, which is what copying them one by one could leave.

    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $plan = @()
    Say ""
    Say "installing  :"
    foreach ($item in $Wanted) {
        $source = $sources[$item.Name]
        foreach ($place in $item.To) {
            $folder = $plugin
            $shown = "$PluginFolder\$($item.Name)"
            if ($place -eq '.') { $folder = $Game; $shown = $item.Name }
            $target = Join-Path $folder $item.Name
            $step = @{
                Source = $source; Target = $target; Shown = $shown; Name = $item.Name
                Temp = "$target.mhw-dlss-new"; Aside = $null; KeepAside = $false
                Why = $null; Staged = $false; MovedAside = $false; Installed = $false
            }

            if (-not (Test-Path -LiteralPath $target)) {
                $step.Why = 'add'
                $plan += $step
                continue
            }
            if ((Get-Sha256 $target) -eq $source.Hash) {
                Note ("up to date  {0,-34} {1}" -f $shown, $source.Version)
                $current++
                continue
            }

            $existing = Get-DllVersion $target
            if ($existing -gt $source.Version -and -not $Force) {
                Warn ("kept        {0,-34} {1} is NEWER than the pinned {2}; not replaced" -f $shown, $existing, $source.Version)
                Note "            The mod was measured with the pinned version. Pass -Force to replace it."
                $kept++
                continue
            }

            $step.Why = "replacing $existing with $($source.Version)"
            if ($existing -gt $source.Version) { $step.Why = "REPLACING THE NEWER $existing WITH $($source.Version), because -Force was given" }
            if ($existing -eq $source.Version) { $step.Why = "same version $existing but different contents (a development build?); replacing" }
            $step.Aside = "$target.before-mhw-dlss-$stamp"
            # In the game root the file may be somebody else's: another mod's,
            # or a different version put there on purpose. It is moved aside
            # and stays. In the plugin folder everything is this mod's own,
            # and the old file is deleted once the new set is in place.
            $step.KeepAside = ($place -eq '.')
            $plan += $step
        }
    }

    if ($DryRun) {
        foreach ($step in $plan) {
            if ($step.Why -eq 'add') {
                Note ("would add   {0,-34} {1}" -f $step.Shown, $step.Source.Version)
            } else {
                Note ("would write {0,-34} {1}" -f $step.Shown, $step.Why)
                if ($step.KeepAside) {
                    Note ("            the file there now would be kept as {0}" -f (Split-Path $step.Aside -Leaf))
                }
            }
        }
    } elseif ($plan.Count -gt 0) {
        try {
            # Second pass: every new file beside where it will live.
            foreach ($step in $plan) {
                if (Test-Path -LiteralPath $step.Temp) { Remove-Item -LiteralPath $step.Temp -Force }
                Copy-Item -LiteralPath $step.Source.Path -Destination $step.Temp
                $step.Staged = $true
                if ((Get-Sha256 $step.Temp) -ne $step.Source.Hash) {
                    throw "$($step.Shown): the copy in the game folder does not have the SHA-256 of the file it was copied from"
                }
            }
            # Third pass: the real names. Renames within one folder, so each
            # is a single step of the file system and none copies any data.
            foreach ($step in $plan) {
                if ($step.Aside) {
                    Rename-Item -LiteralPath $step.Target -NewName (Split-Path $step.Aside -Leaf)
                    $step.MovedAside = $true
                }
                Rename-Item -LiteralPath $step.Temp -NewName (Split-Path $step.Target -Leaf)
                $step.Staged = $false
                $step.Installed = $true
            }
        } catch {
            $reason = $_.Exception.Message
            Warn "installing failed: $reason"
            Warn "undoing what was done, so the game folder is as it was:"
            $undone = $true
            for ($index = $plan.Count - 1; $index -ge 0; $index--) {
                $step = $plan[$index]
                try {
                    if ($step.Installed) {
                        Remove-Item -LiteralPath $step.Target -Force
                        Note ("removed     {0}" -f $step.Shown)
                    }
                    if ($step.MovedAside) {
                        Rename-Item -LiteralPath $step.Aside -NewName (Split-Path $step.Target -Leaf)
                        Note ("put back    {0}" -f $step.Shown)
                    }
                    if ($step.Staged -and (Test-Path -LiteralPath $step.Temp)) {
                        Remove-Item -LiteralPath $step.Temp -Force
                    }
                } catch {
                    $undone = $false
                    Warn ("could not undo {0}: {1}" -f $step.Shown, $_.Exception.Message)
                }
            }
            Write-Host ""
            Write-Host "REFUSED: $reason" -ForegroundColor Red
            if ($undone) {
                Write-Host "Everything this run had done in the game folder was undone."
            } else {
                Write-Host "Not everything could be undone; the lines above say what is left. Files named"
                Write-Host "*.before-mhw-dlss-$stamp are the originals and *.mhw-dlss-new are unused copies."
            }
            Write-Host "A file that cannot be renamed is usually in use: close the game and anything that"
            Write-Host "has the game folder's libraries open, then run this again."
            exit 1
        }

        foreach ($step in $plan) {
            if ($step.Why -eq 'add') {
                Note ("added       {0,-34} {1}" -f $step.Shown, $step.Source.Version)
            } else {
                Note ("replaced    {0,-34} {1}" -f $step.Shown, $step.Why)
                if ($step.KeepAside) {
                    Warn ("            the file that was there is kept as {0}" -f (Split-Path $step.Aside -Leaf))
                    Note "            It was not put there by this script. Delete it if you do not need it."
                } else {
                    Remove-Item -LiteralPath $step.Aside -Force -ErrorAction SilentlyContinue
                }
            }
            $changed++
        }
    }

    foreach ($item in $Licences) {
        $target = Join-Path $plugin $item.Name
        $source = Join-Path $staging $item.Name
        if ((Test-Path -LiteralPath $target) -and (Get-Sha256 $target) -eq (Get-Sha256 $source)) { continue }
        if ($DryRun) {
            Note ("would add   {0}" -f "$PluginFolder\$($item.Name)")
        } else {
            Copy-Item -LiteralPath $source -Destination $target -Force
            Note ("added       {0}" -f "$PluginFolder\$($item.Name)")
        }
    }

    Say ""
    if ($DryRun) {
        Say "Dry run: the game folder was not changed."
    } else {
        Say "Done: $changed changed, $current already up to date, $kept kept because newer."
    }
    Say "NVIDIA's licence texts for these files are in $plugin"
    Say "(streamline.license.txt, nvngx_dlss.license.txt, reflex.license.txt)."
} catch {
    $failed = $true
    throw
} finally {
    if ($KeepStaging -and -not $failed) {
        Say "staging kept: $staging"
    } else {
        Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
    }
}
