<#
.SYNOPSIS
    Install, update or remove mhworld-dlss in the Monster Hunter: World folder.

.DESCRIPTION
    Meant to be run in one line, from a PowerShell window:

        irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1 | iex

    With options:

        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1))) -Uninstall

    What it does, in this order:

      1. Finds the game through Steam's library configuration, and stops if
         it is not found or is running.
      2. Asks a few questions: whether to turn frame generation on, and
         whether to install any of three optional tools. Enter takes the
         default, which is always "no". Each question has a switch that
         answers it in advance, and with no console to ask on the defaults
         are taken.
      3. Downloads the release archive and its SHA256SUMS.txt from this
         project's GitHub releases, and checks the archive against the sums
         before anything else happens.
      4. Copies the archive's one folder, mhw-dlss, into the game folder. An
         existing mhw-dlss.ini or mhw-dlss-loader.ini in it is never
         overwritten, with one exception: a settings file of version 0.2
         (keys such as render_scale and fg, no [Sections]) means nothing to
         0.3, so it is kept as mhw-dlss.0.2.ini and a new mhw-dlss.ini with
         the defaults takes its place. The README maps the old keys to the
         new ones.
      5. Runs the fetch-nvidia.ps1 that came in the archive, which downloads
         NVIDIA's libraries from NVIDIA and checks them itself.
      6. Installs the optional tools that were asked for, each from its own
         official source, and adds each to the chain file.
      7. Prints the Steam launch option, which is the one step left to do by
         hand. Steam's own configuration files are never edited.

    It does not need administrator rights and never asks for them. Files are
    first copied beside where they belong and checked, and only then given
    their names, so a failure leaves the mod's folder as it was. Running it
    again changes nothing that is already in place.

    Nothing here uses the folder the script is in, so it works the same
    piped from the web and run from a file. It does not end the PowerShell
    session it is run in.

    WHERE IT WRITES

    <game>\mhw-dlss and the temp folder. One exception, only if the RenoDX
    add-on is asked for: that one file goes beside the game's executable,
    because that is where ReShade looks for add-ons.

    THE OPTIONAL TOOLS

    None is part of this project. Each is downloaded from its authors, in a
    version this mod was verified with, and checked against a SHA-256 pinned
    in this script before it is used. If a check fails that tool is left out
    and the mod itself stays installed.

      ReShade     6.8.0 with add-on support, from reshade.me. ReShade64.dll
                  is taken out of the official setup program, which is not
                  run. By crosire, BSD 3-Clause licence.
      RenoDX      the HDR add-on for this game, from the RenoDX project's
                  snapshot release on GitHub. That is a rolling build with no
                  fixed version, so it CANNOT be pinned: it is downloaded
                  over HTTPS from the project's own release and that is all
                  the checking there is. By clshortfuse, MIT licence. Needs
                  ReShade, so asking for it asks for ReShade.
      OptiScaler  0.9.4, from github.com/optiscaler/OptiScaler. Not needed
                  for anything this mod does. Only OptiScaler.dll and
                  OptiScaler.ini are installed, the ini with the seven
                  settings this mod needs; its setup script is not run. That
                  is enough for its overlay and for DLSS through it. Its
                  FSR and XeSS upscalers need further libraries from its
                  release archive, which you would copy yourself. The archive
                  is a .7z, so 7-Zip's official 7zr.exe is downloaded into
                  the temp folder, checked, and used to open it. GPL-3.0.

    WHAT -Uninstall REMOVES

    What this script installed, which it records in
    mhw-dlss\mhw-dlss-installed.txt: the mod, NVIDIA's libraries, and any
    optional tool it installed, with the chain-file lines it added. Also the
    files a release archive and fetch-nvidia.ps1 are known to install, for
    an install made by hand, and the logs. Your settings files are kept
    (mhw-dlss.ini, mhw-dlss-loader.ini, and mhw-dlss.0.2.ini if an update
    set one aside), and so is anything else in the folder that this script
    did not put there. What is kept is listed. The folder itself is removed
    only if it ends up empty. -Purge removes the whole folder, whatever is
    in it.

.PARAMETER GameDir
    The Monster Hunter: World folder, when Steam's configuration does not
    lead to it.

.PARAMETER Version
    A release tag to install, such as v0.1.0, instead of the latest release.

.PARAMETER ZipPath
    A release archive you already downloaded. Nothing of the mod is then
    downloaded. It is checked against -SumsPath, or against a SHA256SUMS.txt
    lying beside it; with neither it is used unchecked, and the script says
    so.

.PARAMETER SumsPath
    The SHA256SUMS.txt that belongs to -ZipPath.

.PARAMETER NvidiaZipPath
    A Streamline SDK archive you already downloaded, handed to
    fetch-nvidia.ps1 as its -ZipPath so that it downloads nothing.

.PARAMETER SkipNvidia
    Do not fetch NVIDIA's libraries. The mod then does nothing in the game
    until fetch-nvidia.cmd has been run.

.PARAMETER FrameGeneration
    Turn frame generation on ([FrameGeneration] Enabled = true in
    mhw-dlss.ini) without being asked. Needs an RTX 40 or 50 series card.

.PARAMETER NoFrameGeneration
    Turn frame generation off ([FrameGeneration] Enabled = false) without
    being asked.

.PARAMETER ReShade
    Install ReShade without being asked.

.PARAMETER RenoDX
    Install the RenoDX HDR add-on for this game, and ReShade with it,
    without being asked.

.PARAMETER OptiScaler
    Install OptiScaler without being asked.

.PARAMETER Companions
    "none": ask no questions; install only the tools named by a switch.

.PARAMETER NonInteractive
    Ask no questions; take the default for anything no switch answers.

.PARAMETER Uninstall
    Remove the mod, NVIDIA's libraries and the optional tools this script
    installed. See WHAT -Uninstall REMOVES.

.PARAMETER Purge
    With -Uninstall: remove the whole <game>\mhw-dlss folder, settings files
    and anything else in it included.
#>
param(
    [string]$GameDir,
    [string]$Version,
    [string]$ZipPath,
    [string]$SumsPath,
    [string]$NvidiaZipPath,
    [switch]$SkipNvidia,
    [switch]$FrameGeneration,
    [switch]$NoFrameGeneration,
    [switch]$ReShade,
    [switch]$RenoDX,
    [switch]$OptiScaler,
    [string]$Companions,
    [switch]$NonInteractive,
    [switch]$Uninstall,
    [switch]$Purge
)

# Everything runs in a scope of its own. Piped into Invoke-Expression, a
# script runs in the caller's session: a preference set or a function defined
# at the top level would stay behind in it, and `exit` would close the window.
$mhwDlssState = @{ Failed = $false }
$mhwDlssArguments = @{
    State             = $mhwDlssState
    GameDir           = $GameDir
    Version           = $Version
    ZipPath           = $ZipPath
    SumsPath          = $SumsPath
    NvidiaZipPath     = $NvidiaZipPath
    SkipNvidia        = [bool]$SkipNvidia
    FrameGeneration   = [bool]$FrameGeneration
    NoFrameGeneration = [bool]$NoFrameGeneration
    ReShade           = [bool]$ReShade
    RenoDX            = [bool]$RenoDX
    OptiScaler        = [bool]$OptiScaler
    Companions        = $Companions
    NonInteractive    = [bool]$NonInteractive
    Uninstall         = [bool]$Uninstall
    Purge             = [bool]$Purge
}

& {
    param(
        [hashtable]$State,
        [string]$GameDir,
        [string]$Version,
        [string]$ZipPath,
        [string]$SumsPath,
        [string]$NvidiaZipPath,
        [bool]$SkipNvidia,
        [bool]$FrameGeneration,
        [bool]$NoFrameGeneration,
        [bool]$ReShade,
        [bool]$RenoDX,
        [bool]$OptiScaler,
        [string]$Companions,
        [bool]$NonInteractive,
        [bool]$Uninstall,
        [bool]$Purge
    )

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # the progress bar makes 5.1 crawl
    Set-StrictMode -Version 2

    $Repository   = 'Gargauth/mhworld-dlss'
    $SumsName     = 'SHA256SUMS.txt'
    $SteamAppId   = '582010'   # Monster Hunter: World
    $GameExe      = 'MonsterHunterWorld.exe'

    # -----------------------------------------------------------------------
    # THE LAYOUT. Every path below is relative to the game folder, and every
    # path this script uses is built from these. A release archive holds the
    # one folder $ModFolder; .github/scripts/package.ps1 names the same.
    # -----------------------------------------------------------------------
    $ModFolder       = 'mhw-dlss'
    # Where fetch-nvidia.ps1 puts NVIDIA's libraries, nvngx_dlss.dll among them.
    $NvidiaFolder    = $ModFolder
    # Where ReShade64.dll and OptiScaler's two files go. The chain file names
    # them relative to its own folder.
    $CompanionFolder = $ModFolder
    # Where ReShade looks for add-ons: beside the game's executable, which is
    # the game folder itself. Not a choice of this script's.
    $AddonFolder     = ''
    $Launcher        = "$ModFolder\mhw-dlss-launcher.exe"
    $Payload         = "$ModFolder\mhw-dlss.dll"
    $FetchScript     = "$ModFolder\fetch-nvidia.ps1"
    $SettingsFile    = "$ModFolder\mhw-dlss.ini"
    $ChainFile       = "$ModFolder\mhw-dlss-loader.ini"
    # The settings file as the mod itself would write it, shipped in the
    # archive so that this script can start one from it. See Set-FrameGeneration.
    $DefaultSettings = "$ModFolder\mhw-dlss.default.ini"
    # Where a 0.2 settings file is set aside. See Update-OldSettings.
    $OldSettings     = "$ModFolder\mhw-dlss.0.2.ini"
    # What this script installed, for -Uninstall.
    $ManifestFile    = "$ModFolder\mhw-dlss-installed.txt"
    $LicenceNote     = "$CompanionFolder\COMPANION-LICENCES.txt"

    # The user's own files. Never overwritten, and removed only by -Purge.
    $Settings     = @($SettingsFile, $ChainFile, $OldSettings)
    # What a release archive and fetch-nvidia.ps1 are known to install, and
    # what the mod, the launcher and NVIDIA's libraries write while they run.
    # -Uninstall removes these by name as well as what the manifest lists, so
    # that an install made by hand can be removed too.
    $FromArchive  = @($Launcher, $Payload, $FetchScript, "$ModFolder\fetch-nvidia.cmd", $DefaultSettings,
                      "$ModFolder\README.md", "$ModFolder\LICENSE.txt", "$ModFolder\THIRD-PARTY-NOTICES.md")
    $FromNvidia   = @('sl.interposer.dll', 'sl.common.dll', 'sl.dlss_g.dll', 'sl.reflex.dll',
                      'sl.pcl.dll', 'nvngx_dlssg.dll', 'nvngx_dlss.dll',
                      'streamline.license.txt', 'nvngx_dlss.license.txt', 'reflex.license.txt') |
                    ForEach-Object { "$NvidiaFolder\$_" }
    # dev.token: written by a dev build only (docs/DEVELOPMENT.md).
    $RunFiles     = @("$ModFolder\mhw-dlss.log", "$ModFolder\mhw-dlss.previous.log",
                      "$ModFolder\mhw-dlss-launcher.log", "$ModFolder\mhw-dlss-launcher.previous.log",
                      "$SettingsFile.writing", "$NvidiaFolder\sl.log", "$NvidiaFolder\nvngx.log",
                      "$ModFolder\dev.token")

    # -----------------------------------------------------------------------
    # THE OPTIONAL TOOLS. Versions this mod was verified with, each with the
    # SHA-256 of the download and of the file taken out of it.
    # -----------------------------------------------------------------------
    $ReShadeSetup = @{
        Version = '6.8.0'
        Url     = 'https://reshade.me/downloads/ReShade_Setup_6.8.0_Addon.exe'
        Sha256  = 'AFE4C8F13048306307983B8B3D41D5BF00A86820440B0E57DEA10950E1176445'
        Entry   = 'ReShade64.dll'
        Dll     = '0CEE63F9C9F13F3AC909C5B4903F4DBB4B719A7AB3B4F13B0DEAF83C814B94F7'
        Licence = 'https://github.com/crosire/reshade/blob/main/LICENSE.md'
    }
    $RenoDXAddon = @{
        Name    = 'renodx-mhworld.addon64'
        Url     = 'https://github.com/clshortfuse/renodx/releases/download/snapshot/renodx-mhworld.addon64'
        Licence = 'https://github.com/clshortfuse/renodx/blob/main/LICENSE'
    }
    $OptiScalerRelease = @{
        Version = '0.9.4'
        Url     = 'https://github.com/optiscaler/OptiScaler/releases/download/v0.9.4/Optiscaler_0.9.4-final.20260718._MM.7z'
        Sha256  = '575CB4DF866116093DF75AF607E37FD70E10F5163E0F23FD5C804142E80EF0AD'
        Dll     = 'FBFB6676B829DAD7E020FB830586A16AA0EC6ADD78016DB48EF12E2AE1803231'
        Licence = 'https://github.com/optiscaler/OptiScaler/blob/master/LICENSE'
    }
    # 7-Zip's own small extractor, from 7-Zip's own repository. Only used to
    # open OptiScaler's .7z, from the temp folder, and deleted afterwards.
    $SevenZip = @{
        Version = '26.04'
        Url     = 'https://github.com/ip7z/7zip/releases/download/26.04/7zr.exe'
        Sha256  = '256FECA8E274E5DA655E2A284FABAFD9F554365EB164862089DACD4E8276D282'
    }
    # Where downloads may come from. Nothing else is ever asked for.
    $AllowedSources = '^https://((api\.)?github\.com|reshade\.me)/'

    $TempRoot     = [System.IO.Path]::GetTempPath().TrimEnd('\')
    # Where fetch-nvidia.ps1 is told to keep NVIDIA's archive (about 276 MB),
    # so that a second run does not download it again.
    $NvidiaCache  = Join-Path $TempRoot 'mhw-dlss\cache'

    function Say([string]$Text)  { Write-Host $Text }
    function Note([string]$Text) { Write-Host "  $Text" }
    function Warn([string]$Text) { Write-Host "  ! $Text" -ForegroundColor Yellow }
    function Stop-Here([string]$Text) { throw [System.InvalidOperationException]$Text }

    function Get-Sha256([string]$Path) {
        (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
    }

    # --- find the game -----------------------------------------------------
    # The same lookup as fetch-nvidia.ps1: Steam's registry keys, then every
    # library folder Steam lists, then the game's manifest in each.

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
        @($roots | Sort-Object -Unique)
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
        @($libraries | Sort-Object -Unique)
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

    function Resolve-Game {
        if ($GameDir) {
            $game = $GameDir.TrimEnd('\')
            $how = 'given with -GameDir'
            if (-not (Test-Path -LiteralPath (Join-Path $game $GameExe))) {
                Stop-Here "$game does not contain $GameExe, so it is not the game folder."
            }
            $game = (Resolve-Path -LiteralPath $game).Path.TrimEnd('\')
        } else {
            $game = Find-Game
            $how = "found from Steam's library configuration (app $SteamAppId)"
            if (-not $game) {
                Stop-Here "Monster Hunter: World was not found in any Steam library. Pass its folder with -GameDir."
            }
        }
        Say "game folder : $game"
        Note $how

        # Only a game running out of THIS folder is in the way. A process
        # whose path cannot be read is assumed to be it.
        $expected = Join-Path $game $GameExe
        foreach ($process in @(Get-Process -Name 'MonsterHunterWorld' -ErrorAction SilentlyContinue)) {
            $running = $null
            try { $running = $process.Path } catch { $running = $null }
            if (-not $running -or $running -ieq $expected) {
                Stop-Here "Monster Hunter: World is running from this folder (pid $($process.Id)). Close it and run this again."
            }
        }
        return $game
    }

    function Assert-Writable([string]$Folder) {
        $probe = Join-Path $Folder ("mhw-dlss-write-test-" + [Guid]::NewGuid().ToString('N') + ".tmp")
        try {
            [System.IO.File]::WriteAllText($probe, 'x')
            Remove-Item -LiteralPath $probe -Force
        } catch {
            Stop-Here "$Folder cannot be written to by your account, and this script does not ask for administrator rights. Install by hand instead: the README's second route says how."
        }
    }

    # --- the questions -----------------------------------------------------

    # Whether there is somebody to ask. Piped into Invoke-Expression from a
    # PowerShell window there is: the pipe is inside PowerShell and the
    # console is still the keyboard. Run by another program with its input
    # redirected, or with -NonInteractive, there is not.
    function Test-CanAsk {
        if ($NonInteractive -or $Companions -ieq 'none') { return $false }
        try {
            return [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
        } catch {
            return $false
        }
    }

    # A yes/no question. Enter, anything unclear, or no answer at all is no.
    function Read-YesNo([string]$Question) {
        try {
            $answer = Read-Host "$Question [y/N]"
        } catch {
            return $false
        }
        return ([string]$answer).Trim() -match '^(y|yes)$'
    }

    # What is wanted: FrameGeneration is $true, $false, or $null for "leave
    # the settings file as it is".
    function Get-Choices {
        if ($Companions -and $Companions -ine 'none') {
            Stop-Here "-Companions takes 'none'; the tools themselves are asked for with -ReShade, -RenoDX and -OptiScaler."
        }
        if ($FrameGeneration -and $NoFrameGeneration) {
            Stop-Here "-FrameGeneration and -NoFrameGeneration cannot both be given."
        }
        $choices = @{ FrameGeneration = $null; ReShade = $ReShade; RenoDX = $RenoDX; OptiScaler = $OptiScaler }
        if ($FrameGeneration) { $choices.FrameGeneration = $true }
        if ($NoFrameGeneration) { $choices.FrameGeneration = $false }

        if (Test-CanAsk) {
            Note "Enter answers no to each of these."
            if ($null -eq $choices.FrameGeneration) {
                if (Read-YesNo "Enable frame generation? (needs an RTX 40 or 50 series card)") { $choices.FrameGeneration = $true }
            }
            if (-not $choices.RenoDX -and -not $choices.ReShade) {
                $choices.ReShade = Read-YesNo "Install ReShade? (needed for the in-game settings page and for RenoDX)"
            }
            if (-not $choices.RenoDX) {
                $choices.RenoDX = Read-YesNo "Install the RenoDX HDR add-on for this game? (needs ReShade and an HDR display)"
            }
            if (-not $choices.OptiScaler) {
                Note "OptiScaler adds its own overlay and upscaler options. Nothing this mod does needs it."
                $choices.OptiScaler = Read-YesNo "Install OptiScaler?"
            }
        } else {
            Note "No questions are asked (no console to ask on, -NonInteractive, or -Companions none):"
            Note "frame generation is left as it is and no optional tool is installed, unless a switch says otherwise."
        }
        if ($choices.RenoDX) { $choices.ReShade = $true }
        return $choices
    }

    # --- downloads ---------------------------------------------------------

    function Get-FromWeb([string]$Url, [string]$OutFile) {
        # HTTPS, and only the hosts named at the top. GitHub itself redirects
        # a release download to its own storage; nothing else is followed to.
        if ($Url -notmatch $AllowedSources) {
            Stop-Here "refusing to download from $Url, which is not one of the sources this script uses."
        }
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $headers = @{ 'User-Agent' = 'mhworld-dlss-install' }
        if ($OutFile) {
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -Headers $headers
        } else {
            Invoke-RestMethod -Uri $Url -Headers $headers
        }
    }

    # Download $Url to $OutFile and refuse it unless its SHA-256 is $Sha256.
    function Get-Pinned([string]$Url, [string]$OutFile, [string]$Sha256, [string]$What) {
        Note "downloading $Url"
        try {
            Get-FromWeb $Url $OutFile
        } catch {
            Stop-Here "the download of $What failed ($($_.Exception.Message))."
        }
        $actual = Get-Sha256 $OutFile
        if ($actual -ne $Sha256) {
            Stop-Here "the SHA-256 of $What is $actual, and the one this script was written for is $Sha256. It is not the file that was verified with this mod, so it is not used."
        }
        Note "SHA-256 matches the pinned one: $actual"
    }

    function Assert-Tag([string]$Tag) {
        if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$') {
            Stop-Here "'$Tag' is not a release tag of this project; they look like v0.1.0."
        }
    }

    # The line for $Name in a sums file: "<64 hex digits>  <name>".
    function Get-ExpectedHash([string]$SumsFile, [string]$Name) {
        foreach ($line in Get-Content -LiteralPath $SumsFile) {
            if ($line -match '^\s*([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$' -and $Matches[2] -ieq $Name) {
                return $Matches[1].ToUpperInvariant()
            }
        }
        return $null
    }

    # Returns the path of a release archive that has been checked.
    function Get-Release([string]$Work) {
        if ($ZipPath) {
            if (-not (Test-Path -LiteralPath $ZipPath)) { Stop-Here "-ZipPath $ZipPath does not exist." }
            $zip = (Resolve-Path -LiteralPath $ZipPath).Path
            Say "archive     : $zip"
            Note "given with -ZipPath; nothing of the mod is downloaded"
            $sums = $SumsPath
            if (-not $sums) {
                $beside = Join-Path (Split-Path -Parent $zip) $SumsName
                if (Test-Path -LiteralPath $beside) { $sums = $beside }
            }
            if ($sums) {
                if (-not (Test-Path -LiteralPath $sums)) { Stop-Here "-SumsPath $sums does not exist." }
                $expected = Get-ExpectedHash $sums (Split-Path -Leaf $zip)
                if (-not $expected) { Stop-Here "$sums has no line for $(Split-Path -Leaf $zip)." }
                $actual = Get-Sha256 $zip
                if ($actual -ne $expected) {
                    Stop-Here "the archive's SHA-256 is $actual and $sums says $expected. It is not the file that was released."
                }
                Note "SHA-256 matches ${sums}: $actual"
            } else {
                Warn "no $SumsName was given or found beside it, so the archive is used unchecked"
            }
            return $zip
        }

        $tag = $Version
        if ($tag) {
            Assert-Tag $tag
        } else {
            try {
                $latest = Get-FromWeb "https://api.github.com/repos/$Repository/releases/latest" $null
            } catch {
                Stop-Here "could not ask GitHub for the latest release of $Repository ($($_.Exception.Message)). If there is no release yet, there is nothing to install; otherwise try again, or pass -Version."
            }
            $tag = [string]$latest.tag_name
            Assert-Tag $tag
        }
        $name = "mhworld-dlss-$tag.zip"
        $base = "https://github.com/$Repository/releases/download/$tag"
        $zip = Join-Path $Work $name
        $sums = Join-Path $Work $SumsName
        Say "release     : $tag"
        Note "downloading $base/$name"
        try {
            Get-FromWeb "$base/$SumsName" $sums
            Get-FromWeb "$base/$name" $zip
        } catch {
            Stop-Here "the download of release $tag failed ($($_.Exception.Message))."
        }
        $expected = Get-ExpectedHash $sums $name
        if (-not $expected) { Stop-Here "the release's $SumsName has no line for $name." }
        $actual = Get-Sha256 $zip
        if ($actual -ne $expected) {
            Stop-Here "the downloaded archive's SHA-256 is $actual and the release's $SumsName says $expected. Nothing was installed."
        }
        Note "SHA-256 matches the release's ${SumsName}: $actual"
        return $zip
    }

    # Extract into $Stage and return the relative paths, having refused any
    # archive that is not laid out as a release of this project is.
    function Expand-Release([string]$Zip, [string]$Stage) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $names = @()
        $archive = [System.IO.Compression.ZipFile]::OpenRead($Zip)
        try {
            foreach ($entry in $archive.Entries) {
                $name = $entry.FullName.Replace('\', '/')
                if ($name.EndsWith('/')) { continue }
                $leaf = ($name -split '/')[-1]
                # One folder and nothing beside it. Nothing can land on a
                # file of the game's, and nothing can leave the folder.
                $inFolder = $name -match "^$ModFolder/[^:]+$" -and $name -notmatch '(^|/)\.\.?(/|$)'
                if (-not $inFolder) {
                    Stop-Here "the archive contains '$name', which is outside the one folder, $ModFolder, that a release of this project holds. It is not a release archive of this project."
                }
                if ($leaf -match '^(nvngx_.*|sl\..*)\.dll$') {
                    Stop-Here "the archive contains '$name'. This project never ships NVIDIA's libraries, so it is not a release archive of this project."
                }
                $target = Join-Path $Stage $name.Replace('/', '\')
                New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
                [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
                $names += $name.Replace('/', '\')
            }
        } finally {
            $archive.Dispose()
        }
        foreach ($needed in @($Launcher, $Payload, $FetchScript)) {
            if ($names -notcontains $needed) {
                Stop-Here "the archive has no $needed. It is not a release archive of this project."
            }
        }
        return $names
    }

    # --- the manifest ------------------------------------------------------
    # One line per thing this script put in place: "file <path relative to the
    # game folder>" or "chain <line added to the chain file>".

    function Read-Manifest([string]$Game) {
        $manifest = @{ Files = @(); Chain = @() }
        $path = Join-Path $Game $ManifestFile
        if (Test-Path -LiteralPath $path) {
            foreach ($line in Get-Content -LiteralPath $path) {
                if ($line -match '^file (.+)$') { $manifest.Files += $Matches[1].Trim() }
                elseif ($line -match '^chain (.+)$') { $manifest.Chain += $Matches[1].Trim() }
            }
        }
        return $manifest
    }

    function Write-Manifest([string]$Game, [hashtable]$Manifest) {
        $lines = @('# What install.ps1 installed here. -Uninstall reads this; do not edit it.')
        foreach ($file in @($Manifest.Files | Sort-Object -Unique)) { $lines += "file $file" }
        foreach ($entry in @($Manifest.Chain | Sort-Object -Unique)) { $lines += "chain $entry" }
        [System.IO.File]::WriteAllText((Join-Path $Game $ManifestFile), (($lines -join "`r`n") + "`r`n"), (New-Object System.Text.ASCIIEncoding))
    }

    # Whether a manifest entry is somewhere this script may delete from: the
    # mod's folder, or an add-on file in the folder ReShade reads them from.
    # The manifest is a file anybody can edit, so it is not simply believed.
    function Test-MayRemove([string]$Relative) {
        if ($Relative -match '(^|\\)\.\.?(\\|$)' -or $Relative -match '[:/]' -or $Relative.StartsWith('\')) { return $false }
        if ($Relative.StartsWith("$ModFolder\", [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
        $addon = $RenoDXAddon.Name
        if ($AddonFolder) { $addon = "$AddonFolder\$addon" }
        return $Relative -ieq $addon
    }

    # --- small edits to text files, leaving every other byte alone ----------

    # Whether $Text is a settings file of 0.2: keys (render_scale, fg, ...)
    # and no [Section] at all. Every file of 0.3 has sections.
    function Test-OldSettings([string]$Text) {
        $assignment = $false
        foreach ($line in ($Text -split "`r?`n")) {
            $trimmed = $line.Trim().TrimStart([char]0xFEFF)
            if ($trimmed -eq '' -or $trimmed.StartsWith('#') -or $trimmed.StartsWith(';')) { continue }
            if ($trimmed.StartsWith('[')) { return $false }
            if ($trimmed.Contains('=')) { $assignment = $true }
        }
        return $assignment
    }

    # $Text with $Key in [$Section] set to $Value. Sections and keys match
    # without regard to case, as the mod reads them. A key that is not there
    # goes right after its section's header, and a section that is not there
    # at the end of the file. Every other byte is left alone.
    function Set-IniValue([string]$Text, [string]$Section, [string]$Key, [string]$Value) {
        $ending = "`r`n"
        if ($Text -notmatch "`r`n" -and $Text -match "`n") { $ending = "`n" }
        $lines = New-Object System.Collections.Generic.List[string]
        foreach ($piece in [regex]::Split($Text, '(?<=\n)')) { if ($piece -ne '') { $lines.Add($piece) } }
        # PowerShell names are not case-sensitive: $current is not $Section.
        $current = ''
        $header = -1
        $found = -1
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $trimmed = $lines[$i].Trim().TrimStart([char]0xFEFF)
            if ($trimmed -match '^\[\s*([^\]]*?)\s*\]') {
                $current = $Matches[1]
                if ($current -ieq $Section -and $header -lt 0) { $header = $i }
                continue
            }
            if ($current -ine $Section -or $found -ge 0 -or $trimmed.StartsWith(';') -or $trimmed.StartsWith('#')) { continue }
            $index = $trimmed.IndexOf('=')
            if ($index -gt 0 -and $trimmed.Substring(0, $index).Trim() -ieq $Key) { $found = $i }
        }
        $setting = "$Key = $Value"
        if ($found -ge 0) {
            $body = $lines[$found].TrimEnd("`r", "`n")
            $lines[$found] = $setting + $lines[$found].Substring($body.Length)
        } else {
            if ($lines.Count -gt 0 -and -not $lines[$lines.Count - 1].EndsWith("`n")) {
                $lines[$lines.Count - 1] = $lines[$lines.Count - 1] + $ending
            }
            if ($header -ge 0) {
                $lines.Insert($header + 1, "$setting$ending")
            } else {
                if ($lines.Count -gt 0) { $lines.Add($ending) }
                $lines.Add("[$Section]$ending$setting$ending")
            }
        }
        return ($lines -join '')
    }

    function Write-Text([string]$Path, [string]$Text) {
        [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
    }

    # A settings file of 0.2 is set aside as $OldSettings and replaced by
    # the default, which the archive carries: 0.3 would read none of its
    # keys, name each in its log as unknown, and run on the defaults while
    # the file said otherwise. The README maps the old keys to the new.
    function Update-OldSettings([string]$Game) {
        $path = Join-Path $Game $SettingsFile
        if (-not (Test-Path -LiteralPath $path)) { return }
        if (-not (Test-OldSettings ([System.IO.File]::ReadAllText($path)))) { return }
        $backup = Join-Path $Game $OldSettings
        if (Test-Path -LiteralPath $backup) {
            # One set aside already, by an earlier run: that one is kept too.
            $backup = Join-Path $Game ("$ModFolder\mhw-dlss.0.2-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.ini')
        }
        Move-Item -LiteralPath $path -Destination $backup
        Note ("kept        {0}  (your 0.2 settings; 0.3 uses new keys, see the README)" -f $backup.Substring($Game.Length + 1))
        $default = Join-Path $Game $DefaultSettings
        if (Test-Path -LiteralPath $default) {
            Copy-Item -LiteralPath $default -Destination $path
            Note ("added       {0}  (the defaults of this version)" -f $SettingsFile)
        } else {
            Note ("            the mod writes a new {0} when it next starts" -f $SettingsFile)
        }
    }

    # Set [FrameGeneration] Enabled in the settings file. The mod writes that
    # file itself, complete with its explanations, but only when there is
    # none: a file holding just this key would stay that bare. So a file that
    # does not exist yet is started from the copy of the mod's own text that
    # the archive carries.
    function Set-FrameGeneration([string]$Game, [bool]$On) {
        $value = 'false'
        if ($On) { $value = 'true' }
        $path = Join-Path $Game $SettingsFile
        if (Test-Path -LiteralPath $path) {
            $text = [System.IO.File]::ReadAllText($path)
        } else {
            $default = Join-Path $Game $DefaultSettings
            if (Test-Path -LiteralPath $default) {
                $text = [System.IO.File]::ReadAllText($default)
            } else {
                $text = ''
                Warn "this release has no $DefaultSettings to start the settings file from, so it will hold only this one key"
            }
        }
        $updated = Set-IniValue $text 'FrameGeneration' 'Enabled' $value
        if ($updated -ne $text -or -not (Test-Path -LiteralPath $path)) { Write-Text $path $updated }
        Note ("set         [FrameGeneration] Enabled = {0} in {1}" -f $value, $SettingsFile)
    }

    # Add a line to the chain file unless a line already loads that library.
    # The launcher writes the file on first start, with only the mod in it;
    # before that there is none, and one is started with the mod's line.
    function Add-ChainLine([string]$Game, [string]$Library, [hashtable]$Manifest) {
        $relative = $Library
        if ($CompanionFolder -ne $ModFolder) {
            $relative = "$($CompanionFolder.Substring($ModFolder.Length + 1))\$Library"
        }
        $wanted = "$relative  renderer"
        $path = Join-Path $Game $ChainFile
        $text = ''
        if (Test-Path -LiteralPath $path) {
            $text = [System.IO.File]::ReadAllText($path)
        } else {
            $text = "# Libraries loaded into the game, one per line. The launcher fills in the`r`n# explanation the first time it runs without this file.`r`n`r`n$(Split-Path -Leaf $Payload)`r`n"
        }
        foreach ($line in ($text -split "`r?`n")) {
            $trimmed = $line.Trim()
            if ($trimmed.StartsWith('#') -or $trimmed -eq '') { continue }
            # The path, without the word that may follow it (`renderer`, or
            # `after` and a number) and without quotes; it may hold spaces.
            $name = ($trimmed -replace '\s+(renderer|after(\s+[0-9]+)?)\s*$', '').Trim().Trim('"')
            if ($name -ieq $relative -or (Split-Path -Leaf $name) -ieq $Library) {
                Note ("chain file  already loads {0}; left as it is" -f $Library)
                return
            }
        }
        $ending = "`r`n"
        if ($text -notmatch "`r`n" -and $text -match "`n") { $ending = "`n" }
        if ($text.Length -gt 0 -and -not $text.EndsWith("`n")) { $text += $ending }
        Write-Text $path ($text + $wanted + $ending)
        $Manifest.Chain += $wanted
        Note ("chain file  added the line: {0}" -f $wanted)
    }

    # Put one companion file in place. A file of that name that this script
    # did not install is the user's own and is left alone. Returns whether
    # the file there is now the one given.
    function Install-CompanionFile([string]$Game, [string]$Source, [string]$Relative, [hashtable]$Manifest) {
        $target = Join-Path $Game $Relative
        if (Test-Path -LiteralPath $target) {
            if ((Get-Sha256 $target) -eq (Get-Sha256 $Source)) {
                if ($Manifest.Files -notcontains $Relative) { $Manifest.Files += $Relative }
                Note ("up to date  {0}" -f $Relative)
                return $true
            }
            if ($Manifest.Files -notcontains $Relative) {
                Warn ("kept        {0}: a different file of that name is already there and was not installed by this script" -f $Relative)
                return $false
            }
        }
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        $temp = "$target.mhw-dlss-new"
        Copy-Item -LiteralPath $Source -Destination $temp -Force
        if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Force }
        Rename-Item -LiteralPath $temp -NewName (Split-Path -Leaf $target)
        if ($Manifest.Files -notcontains $Relative) { $Manifest.Files += $Relative }
        Note ("installed   {0}" -f $Relative)
        return $true
    }

    # --- the optional tools ------------------------------------------------

    # ReShade's setup program is a .NET executable with a zip archive after
    # it. The archive is found from its own end-of-directory record, cut out
    # into a file, and opened as any zip; the setup program is never run.
    function Expand-ReShadeSetup([string]$Setup, [string]$Entry, [string]$OutFile) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $bytes = [System.IO.File]::ReadAllBytes($Setup)
        $end = -1
        for ($i = $bytes.Length - 22; $i -ge 0 -and $i -ge $bytes.Length - 200000; $i--) {
            if ($bytes[$i] -eq 0x50 -and $bytes[$i + 1] -eq 0x4B -and $bytes[$i + 2] -eq 0x05 -and $bytes[$i + 3] -eq 0x06) { $end = $i; break }
        }
        if ($end -lt 0) { Stop-Here "ReShade's setup program holds no archive where one was expected." }
        $size = [BitConverter]::ToUInt32($bytes, $end + 12)
        $offset = [BitConverter]::ToUInt32($bytes, $end + 16)
        $comment = [BitConverter]::ToUInt16($bytes, $end + 20)
        $start = $end - $size - $offset
        if ($start -lt 0) { Stop-Here "the archive inside ReShade's setup program is not laid out as expected." }
        $zip = "$OutFile.zip"
        $stream = [System.IO.File]::Create($zip)
        try { $stream.Write($bytes, $start, $end + 22 + $comment - $start) } finally { $stream.Dispose() }
        $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
        try {
            $found = $archive.GetEntry($Entry)
            if ($null -eq $found) { Stop-Here "ReShade's setup program has no $Entry in it." }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($found, $OutFile, $true)
        } finally {
            $archive.Dispose()
            Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
        }
    }

    function Install-ReShade([string]$Game, [string]$Work, [hashtable]$Manifest) {
        Say ""
        Say "ReShade $($ReShadeSetup.Version), with add-on support (crosire's, BSD 3-Clause licence)"
        $relative = "$CompanionFolder\$($ReShadeSetup.Entry)"
        $target = Join-Path $Game $relative
        if ((Test-Path -LiteralPath $target) -and (Get-Sha256 $target) -eq $ReShadeSetup.Dll) {
            if ($Manifest.Files -notcontains $relative) { $Manifest.Files += $relative }
            Note ("up to date  {0}" -f $relative)
        } else {
            $setup = Join-Path $Work 'ReShade_Setup.exe'
            Get-Pinned $ReShadeSetup.Url $setup $ReShadeSetup.Sha256 "ReShade's setup program"
            $dll = Join-Path $Work $ReShadeSetup.Entry
            Expand-ReShadeSetup $setup $ReShadeSetup.Entry $dll
            if ((Get-Sha256 $dll) -ne $ReShadeSetup.Dll) {
                Stop-Here "the $($ReShadeSetup.Entry) inside ReShade's setup program is not the one that was verified with this mod, so it is not used."
            }
            [void](Install-CompanionFile $Game $dll $relative $Manifest)
        }
        Add-ChainLine $Game $ReShadeSetup.Entry $Manifest
        Note "ReShade keeps its own files (ReShade.ini, ReShade.log, presets, shaders, add-ons)"
        Note "beside the game's executable, not in $ModFolder. Its overlay opens with the Home key."
    }

    function Install-RenoDX([string]$Game, [string]$Work, [hashtable]$Manifest) {
        Say ""
        Say "RenoDX HDR add-on for this game (clshortfuse's, MIT licence)"
        Warn "This is the project's rolling snapshot build: it has no fixed version, so it cannot be"
        Warn "checked against a pinned SHA-256. It is downloaded over HTTPS from the project's own release."
        $file = Join-Path $Work $RenoDXAddon.Name
        Note "downloading $($RenoDXAddon.Url)"
        try {
            Get-FromWeb $RenoDXAddon.Url $file
        } catch {
            Stop-Here "the download of the RenoDX add-on failed ($($_.Exception.Message))."
        }
        $head = [System.IO.File]::ReadAllBytes($file)
        if ($head.Length -lt 65536 -or $head[0] -ne 0x4D -or $head[1] -ne 0x5A) {
            Stop-Here "what was downloaded is not an add-on library, so it is not installed."
        }
        Note "SHA-256 of what was downloaded: $(Get-Sha256 $file)"
        $relative = $RenoDXAddon.Name
        if ($AddonFolder) { $relative = "$AddonFolder\$relative" }
        if (Install-CompanionFile $Game $file $relative $Manifest) {
            Note "It is beside the game's executable, because that is where ReShade looks for add-ons."
            Note "Turn HDR on in Windows and in the game's display options."
        }
    }

    function Install-OptiScaler([string]$Game, [string]$Work, [hashtable]$Manifest) {
        Say ""
        Say "OptiScaler $($OptiScalerRelease.Version) (the OptiScaler project's, GPL-3.0 licence)"
        $dllRelative = "$CompanionFolder\OptiScaler.dll"
        $iniRelative = "$CompanionFolder\OptiScaler.ini"
        $dllTarget = Join-Path $Game $dllRelative
        $iniTarget = Join-Path $Game $iniRelative
        $have = (Test-Path -LiteralPath $dllTarget) -and (Get-Sha256 $dllTarget) -eq $OptiScalerRelease.Dll -and (Test-Path -LiteralPath $iniTarget)
        if ($have) {
            if ($Manifest.Files -notcontains $dllRelative) { $Manifest.Files += $dllRelative }
            Note ("up to date  {0}" -f $dllRelative)
        } else {
            $extractor = Join-Path $Work '7zr.exe'
            Note "Its release is a .7z archive; 7-Zip's own 7zr.exe $($SevenZip.Version) opens it, from the temp folder."
            Get-Pinned $SevenZip.Url $extractor $SevenZip.Sha256 "7-Zip's 7zr.exe"
            $archive = Join-Path $Work 'OptiScaler.7z'
            Get-Pinned $OptiScalerRelease.Url $archive $OptiScalerRelease.Sha256 "OptiScaler's release archive"
            $out = Join-Path $Work 'optiscaler'
            New-Item -ItemType Directory -Path $out | Out-Null
            # Two files by name, and nothing else: no setup script, no other library.
            & $extractor x -y "-o$out" $archive 'OptiScaler.dll' 'OptiScaler.ini' | Out-Null
            if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath (Join-Path $out 'OptiScaler.dll')) -or -not (Test-Path -LiteralPath (Join-Path $out 'OptiScaler.ini'))) {
                Stop-Here "OptiScaler's archive could not be opened (7zr exit code $LASTEXITCODE)."
            }
            if ((Get-Sha256 (Join-Path $out 'OptiScaler.dll')) -ne $OptiScalerRelease.Dll) {
                Stop-Here "the OptiScaler.dll in the archive is not the one that was verified with this mod, so it is not used."
            }
            if (-not (Install-CompanionFile $Game (Join-Path $out 'OptiScaler.dll') $dllRelative $Manifest)) {
                Stop-Here "OptiScaler is left as you have it: nothing of it was changed."
            }
            if (-not (Test-Path -LiteralPath $iniTarget)) {
                [void](Install-CompanionFile $Game (Join-Path $out 'OptiScaler.ini') $iniRelative $Manifest)
            }
        }

        # The seven settings this mod needs. Written only into an ini this
        # script installed; one that was already there is the user's.
        $wanted = [ordered]@{
            'FGInput'            = 'nofg'
            'FGOutput'           = 'nofg'
            'EnableDlssInputs'   = 'true'
            'NvngxFeaturePath'   = (Join-Path $Game $NvidiaFolder)
            'OverlayMenu'        = 'true'
            'StreamlineSpoofing' = 'false'
            'Dxgi'               = 'false'
        }
        if ($Manifest.Files -contains $iniRelative) {
            $text = [System.IO.File]::ReadAllText($iniTarget)
            $updated = $text
            foreach ($key in $wanted.Keys) {
                $pattern = "(?m)^$key=[^\r\n]*"
                if ($updated -notmatch $pattern) { Stop-Here "OptiScaler.ini has no $key setting, so it is not the file this script knows." }
                $replacement = "$key=$($wanted[$key])".Replace('$', '$$')
                $updated = [regex]::Replace($updated, $pattern, $replacement)
            }
            if ($updated -ne $text) { Write-Text $iniTarget $updated }
            Note "set in OptiScaler.ini: $((@($wanted.Keys) | ForEach-Object { "$_=$($wanted[$_])" }) -join ', ')"
        } else {
            Warn "OptiScaler.ini was already there and is yours, so it was not changed. With this mod it needs:"
            foreach ($key in $wanted.Keys) { Note ("    {0}={1}" -f $key, $wanted[$key]) }
        }
        Add-ChainLine $Game 'OptiScaler.dll' $Manifest
        Note "Only OptiScaler.dll and OptiScaler.ini were installed: enough for its overlay (Insert key)"
        Note "and for DLSS through it. Its FSR and XeSS upscalers need more libraries from its release"
        Note "archive; copy those beside OptiScaler.dll yourself if you want them."
        Note "At start it shows a 'Late Streamline hook detected' notice. With this mod that is expected."
    }

    function Write-LicenceNote([string]$Game, [hashtable]$Choices, [hashtable]$Manifest) {
        $lines = @(
            'Optional tools installed by install.ps1 at your request. None is part of'
            'mhworld-dlss or distributed by it; each was downloaded from its authors.'
            ''
        )
        if ($Choices.ReShade)    { $lines += "ReShade $($ReShadeSetup.Version), by crosire. BSD 3-Clause licence: $($ReShadeSetup.Licence)" }
        if ($Choices.RenoDX)     { $lines += "RenoDX add-on for Monster Hunter: World, by clshortfuse. MIT licence: $($RenoDXAddon.Licence)" }
        if ($Choices.OptiScaler) { $lines += "OptiScaler $($OptiScalerRelease.Version), by the OptiScaler project. GPL-3.0 licence: $($OptiScalerRelease.Licence)" }
        $path = Join-Path $Game $LicenceNote
        # Added to, never shortened: a tool installed by an earlier run is still there.
        if (Test-Path -LiteralPath $path) {
            $existing = @(Get-Content -LiteralPath $path)
            $lines = @($existing) + @($lines | Where-Object { $_ -and $existing -notcontains $_ })
        }
        [System.IO.File]::WriteAllText($path, (($lines -join "`r`n") + "`r`n"), (New-Object System.Text.ASCIIEncoding))
        if ($Manifest.Files -notcontains $LicenceNote) { $Manifest.Files += $LicenceNote }
    }

    # --- install -----------------------------------------------------------

    function Install-Mod {
        Say "mhworld-dlss: installing"
        Say ""
        Say "1/7 Finding the game"
        $game = Resolve-Game
        $folder = Join-Path $game $ModFolder

        Say ""
        Say "2/7 A few questions"
        $choices = Get-Choices

        $work = Join-Path $TempRoot ("mhw-dlss-install-" + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $work | Out-Null
        try {
            Say ""
            Say "3/7 Getting the release and checking it"
            $zip = Get-Release $work
            $stage = Join-Path $work 'stage'
            New-Item -ItemType Directory -Path $stage | Out-Null
            $names = Expand-Release $zip $stage

            Say ""
            Say "4/7 Copying the mod into $folder"
            # The folder is the only thing this step creates in the game folder.
            $createdFolder = $false
            if (-not (Test-Path -LiteralPath $folder)) {
                try {
                    New-Item -ItemType Directory -Path $folder | Out-Null
                } catch {
                    Stop-Here "$folder could not be created ($($_.Exception.Message)). This script does not ask for administrator rights. Install by hand instead: the README's second route says how."
                }
                $createdFolder = $true
            }
            $plan = @()
            $current = 0
            try {
                Assert-Writable $folder
                # Decide first, change nothing yet.
                foreach ($name in $names) {
                    $source = Join-Path $stage $name
                    $target = Join-Path $game $name
                    if (($Settings -contains $name) -and (Test-Path -LiteralPath $target)) {
                        Note ("kept        {0}  (yours; not overwritten)" -f $name)
                        continue
                    }
                    $hash = Get-Sha256 $source
                    if ((Test-Path -LiteralPath $target) -and (Get-Sha256 $target) -eq $hash) {
                        $current++
                        continue
                    }
                    $plan += @{
                        Name = $name; Source = $source; Target = $target; Hash = $hash
                        Temp = "$target.mhw-dlss-new"; Old = "$target.mhw-dlss-old"
                        Existed = (Test-Path -LiteralPath $target)
                        Staged = $false; MovedOld = $false; Installed = $false
                    }
                }
                foreach ($name in $Settings) {
                    if (($names -notcontains $name) -and (Test-Path -LiteralPath (Join-Path $game $name))) {
                        Note ("kept        {0}  (yours; an update never touches it)" -f $name)
                    }
                }
                # Every new file beside where it will live, and checked there.
                foreach ($step in $plan) {
                    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $step.Target) | Out-Null
                    if (Test-Path -LiteralPath $step.Temp) { Remove-Item -LiteralPath $step.Temp -Force }
                    Copy-Item -LiteralPath $step.Source -Destination $step.Temp
                    $step.Staged = $true
                    if ((Get-Sha256 $step.Temp) -ne $step.Hash) {
                        throw "$($step.Name): the copy in the game folder does not have the SHA-256 of the file it was copied from"
                    }
                }
                # Then the real names: renames within one folder.
                foreach ($step in $plan) {
                    if ($step.Existed) {
                        if (Test-Path -LiteralPath $step.Old) { Remove-Item -LiteralPath $step.Old -Force }
                        Rename-Item -LiteralPath $step.Target -NewName (Split-Path -Leaf $step.Old)
                        $step.MovedOld = $true
                    }
                    Rename-Item -LiteralPath $step.Temp -NewName (Split-Path -Leaf $step.Target)
                    $step.Staged = $false
                    $step.Installed = $true
                }
            } catch {
                $reason = $_.Exception.Message
                Warn "copying failed: $reason"
                Warn "undoing what was done, so the folder is as it was"
                $undone = $true
                for ($index = $plan.Count - 1; $index -ge 0; $index--) {
                    $step = $plan[$index]
                    try {
                        if ($step.Installed) { Remove-Item -LiteralPath $step.Target -Force }
                        if ($step.MovedOld) { Rename-Item -LiteralPath $step.Old -NewName (Split-Path -Leaf $step.Target) }
                        if ($step.Staged -and (Test-Path -LiteralPath $step.Temp)) { Remove-Item -LiteralPath $step.Temp -Force }
                    } catch {
                        $undone = $false
                        Warn ("could not undo {0}: {1}" -f $step.Name, $_.Exception.Message)
                    }
                }
                if ($createdFolder -and (Test-Path -LiteralPath $folder) -and -not (Get-ChildItem -LiteralPath $folder -Force)) {
                    Remove-Item -LiteralPath $folder -Force
                }
                $tail = "Everything this run had done was undone."
                if (-not $undone) {
                    $tail = "Not everything could be undone; the lines above say what is left. Files ending .mhw-dlss-old are the previous ones and .mhw-dlss-new are unused copies."
                }
                Stop-Here "$reason $tail A file that cannot be renamed is usually in use: close the game and run this again."
            }

            foreach ($step in $plan) {
                if ($step.Existed) {
                    Remove-Item -LiteralPath $step.Old -Force -ErrorAction SilentlyContinue
                    Note ("updated     {0}" -f $step.Name)
                } else {
                    Note ("added       {0}" -f $step.Name)
                }
            }
            if ($current -gt 0) { Note "$current files were already up to date" }

            # From here on the mod is installed, and what follows is recorded
            # as it happens, so that a later failure still leaves a manifest.
            $manifest = Read-Manifest $game
            foreach ($name in $names) {
                if (($Settings -notcontains $name) -and ($manifest.Files -notcontains $name)) { $manifest.Files += $name }
            }
            Write-Manifest $game $manifest

            Update-OldSettings $game

            if ($null -ne $choices.FrameGeneration) {
                Set-FrameGeneration $game $choices.FrameGeneration
                if ($choices.FrameGeneration) { Note "            Frame generation needs an RTX 40 or 50 series card." }
            }

            Say ""
            Say "5/7 Fetching NVIDIA's libraries"
            $nvidiaDone = $false
            if ($SkipNvidia) {
                Note "skipped, because -SkipNvidia was given. The mod does nothing in the game until"
                Note "they are there: double-click fetch-nvidia.cmd in $folder to get them."
            } else {
                Note "running the fetch-nvidia.ps1 that came in the archive. It downloads NVIDIA's"
                Note "Streamline SDK archive (about 276 MB) from NVIDIA's GitHub and checks every file."
                $fetch = Join-Path $game $FetchScript
                $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $fetch, '-GamePath', $game)
                if ($NvidiaZipPath) {
                    $arguments += @('-ZipPath', $NvidiaZipPath)
                } else {
                    $arguments += @('-CachePath', $NvidiaCache)
                }
                Say ""
                & powershell.exe @arguments
                $code = $LASTEXITCODE
                Say ""
                if ($code -eq 0) {
                    $nvidiaDone = $true
                    foreach ($name in $FromNvidia) {
                        if ((Test-Path -LiteralPath (Join-Path $game $name)) -and ($manifest.Files -notcontains $name)) { $manifest.Files += $name }
                    }
                    Write-Manifest $game $manifest
                } else {
                    Warn "fetch-nvidia.ps1 did not finish (exit code $code); what it printed above says why."
                    Warn "The mod itself is installed. Without NVIDIA's libraries it does nothing in the"
                    Warn "game and says so in its log. Double-click fetch-nvidia.cmd in $folder"
                    Warn "to try again."
                    $State.Failed = $true
                }
            }

            Say ""
            Say "6/7 Optional tools"
            $tools = @()
            if ($choices.ReShade)    { $tools += @{ Name = 'ReShade';    Run = { Install-ReShade $game $work $manifest } } }
            if ($choices.RenoDX)     { $tools += @{ Name = 'RenoDX';     Run = { Install-RenoDX $game $work $manifest } } }
            if ($choices.OptiScaler) { $tools += @{ Name = 'OptiScaler'; Run = { Install-OptiScaler $game $work $manifest } } }
            if ($tools.Count -eq 0) {
                Note "none asked for. docs/companions.md in the repository says how to add one by hand."
            }
            foreach ($tool in $tools) {
                try {
                    & $tool.Run
                } catch {
                    Warn "$($tool.Name) was not installed: $($_.Exception.Message)"
                    Warn "The mod itself is installed and does not need it."
                    $State.Failed = $true
                }
                Write-Manifest $game $manifest
            }
            if ($tools.Count -gt 0) {
                Write-LicenceNote $game $choices $manifest
                Write-Manifest $game $manifest
                Say ""
                Note "Whose these are and under which licences is in $LicenceNote"
            }

            Say ""
            Say "7/7 One step is left, and it is yours"
            Note "In Steam: Library, right-click Monster Hunter: World, Properties, General,"
            Note "and paste this into Launch Options, quotes included:"
            Say ""
            Write-Host ("    `"{0}`" %command%" -f (Join-Path $game $Launcher)) -ForegroundColor Cyan
            Say ""
            Note "This script does not edit Steam's settings for you: Steam rewrites those files"
            Note "while it runs, and a wrong edit can lose the settings of every game."
            Note "In the game, use DirectX 12 and set Anti-Aliasing to TAA. Then start it from Steam."
            Say ""
            if ($State.Failed) {
                Say "The mod is installed in $folder, but not everything went through; see the ! lines above."
            } else {
                Say "Done. The mod is installed in $folder"
            }
        } finally {
            Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    # --- uninstall ---------------------------------------------------------

    function Remove-Listed([string]$Path, [string]$Shown) {
        if (-not (Test-Path -LiteralPath $Path)) { return }
        try {
            Remove-Item -LiteralPath $Path -Recurse -Force
            Note ("removed     {0}" -f $Shown)
        } catch {
            Warn ("could not remove {0}: {1}" -f $Shown, $_.Exception.Message)
            $State.Failed = $true
        }
    }

    function Uninstall-Mod {
        Say "mhworld-dlss: removing"
        Say ""
        $game = Resolve-Game
        $folder = Join-Path $game $ModFolder
        $manifest = Read-Manifest $game
        Say ""

        # What the manifest lists outside the mod's folder goes either way:
        # -Purge is about the folder.
        foreach ($name in $manifest.Files) {
            if (-not (Test-MayRemove $name)) {
                Warn "left alone  ${name}: the manifest lists it, and it is not somewhere this script removes from"
                continue
            }
            if (-not $name.StartsWith("$ModFolder\", [System.StringComparison]::OrdinalIgnoreCase)) {
                Remove-Listed (Join-Path $game $name) "$name  (beside the game's executable; installed by this script)"
            }
        }

        if (-not (Test-Path -LiteralPath $folder)) {
            Note "$folder does not exist; there is nothing of the mod in the game folder"
        } elseif ($Purge) {
            Remove-Listed $folder "$ModFolder\  (the whole folder, because -Purge was given)"
        } else {
            # The chain file is kept, so the lines this script added to it for
            # tools it is about to remove are taken out again.
            $chain = Join-Path $game $ChainFile
            if ($manifest.Chain.Count -gt 0 -and (Test-Path -LiteralPath $chain)) {
                $text = [System.IO.File]::ReadAllText($chain)
                $kept = New-Object System.Text.StringBuilder
                $removed = @()
                foreach ($line in [regex]::Split($text, '(?<=\n)')) {
                    $normal = ($line.Trim() -replace '\s+', ' ')
                    $ours = $false
                    foreach ($entry in $manifest.Chain) { if ($normal -ieq ($entry -replace '\s+', ' ')) { $ours = $true } }
                    if ($ours) { $removed += $line.Trim() } else { [void]$kept.Append($line) }
                }
                if ($removed.Count -gt 0) {
                    Write-Text $chain $kept.ToString()
                    foreach ($line in $removed) { Note ("chain file  removed the line: {0}" -f $line) }
                }
            }
            $names = @($manifest.Files | Where-Object { Test-MayRemove $_ }) + $FromArchive + $FromNvidia + $RunFiles + @($ManifestFile)
            foreach ($name in @($names | Select-Object -Unique)) {
                if ($name.StartsWith("$ModFolder\", [System.StringComparison]::OrdinalIgnoreCase)) {
                    Remove-Listed (Join-Path $game $name) $name
                }
            }
            # Copies an interrupted install or fetch left under a temporary name.
            foreach ($stale in @(Get-ChildItem -LiteralPath $folder -File -Recurse -Force | Where-Object { $_.Name -match '\.mhw-dlss-(new|old)$' })) {
                Remove-Listed $stale.FullName $stale.FullName.Substring($game.Length + 1)
            }
            # Subfolders this left empty, deepest first, then the folder itself.
            foreach ($directory in @(Get-ChildItem -LiteralPath $folder -Directory -Recurse -Force | Sort-Object { $_.FullName.Length } -Descending)) {
                if (-not (Get-ChildItem -LiteralPath $directory.FullName -Force)) { Remove-Item -LiteralPath $directory.FullName -Force }
            }
            $left = @(Get-ChildItem -LiteralPath $folder -Force)
            if ($left.Count -eq 0) {
                Remove-Listed $folder "$ModFolder\  (the folder, now empty)"
            } else {
                Say ""
                Note "Kept in $folder, because this script did not install it:"
                foreach ($item in $left) {
                    $why = ''
                    if ($Settings -contains "$ModFolder\$($item.Name)") { $why = '  (your settings)' }
                    Note ("  {0}{1}" -f $item.Name, $why)
                }
                Note "Delete the folder yourself, or run this again with -Uninstall -Purge, to remove these too."
            }
        }

        # Outside the game folder: the download cache of either route, and
        # the places the mod's log goes when its own folder cannot be written.
        Say ""
        Remove-Listed (Join-Path $TempRoot 'mhw-dlss') '%TEMP%\mhw-dlss\  (the cached NVIDIA archive)'
        Remove-Listed (Join-Path $TempRoot 'mhw-dlss.log') '%TEMP%\mhw-dlss.log'
        if ($env:LOCALAPPDATA) {
            Remove-Listed (Join-Path $env:LOCALAPPDATA 'mhw-dlss') '%LOCALAPPDATA%\mhw-dlss\  (cache and fallback logs)'
        }

        Say ""
        Say "One step is left, and it is yours"
        Note "In Steam: Library, right-click Monster Hunter: World, Properties, General,"
        Note "and clear Launch Options. Until you do, Steam tries to start the game through"
        Note "a launcher that is no longer there, and the game does not start."
        Say ""
        Note "If you used ReShade: what ReShade itself wrote beside the game's executable"
        Note "(ReShade.ini, ReShade.log, presets, shaders) is still there and is yours to remove."
        Say ""
        if ($State.Failed) {
            Say "Not everything could be removed; the lines above say what is left."
        } else {
            Say "Done."
        }
    }

    try {
        if ($Purge -and -not $Uninstall) { Stop-Here "-Purge only means something together with -Uninstall." }
        if ($Uninstall) { Uninstall-Mod } else { Install-Mod }
    } catch {
        $State.Failed = $true
        Write-Host ""
        Write-Host "STOPPED: $($_.Exception.Message)" -ForegroundColor Red
    }
} @mhwDlssArguments

# Only a script run from a file reports failure by its exit code. Piped into
# Invoke-Expression there is no script to exit from, only the user's session.
if ($mhwDlssState.Failed -and $PSCommandPath) { exit 1 }
