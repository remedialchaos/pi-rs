#!/usr/bin/env pwsh
<#
.SYNOPSIS
  pi-rs installer for Windows (PowerShell 5.1+ / PowerShell 7+).

.DESCRIPTION
  Downloads the verified prebuilt pi-rs Windows release archive from GitHub Releases,
  checks SHA256SUMS.txt, installs binaries into a managed directory under
  %USERPROFILE%\.pi\agent\install-rs (isolated from upstream pi's install folder),
  copies them into the target bin directory, and optionally adds the bin directory
  to the user PATH.

.EXAMPLE
  powershell -c "irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex"

.EXAMPLE
  $env:PI_AS_PI_RS = "1"; powershell -c "irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex"

.EXAMPLE
  .\install.ps1 -Version v1.6.0 -CoreOnly -Yes
#>

[CmdletBinding()]
param(
  [string]$Version = $(if ($env:PI_VERSION) { $env:PI_VERSION } else { "latest" }),
  [string]$BinDir = $(if ($env:PI_BIN_DIR) { $env:PI_BIN_DIR } else { "" }),
  [string]$InstallDir = $(if ($env:PI_INSTALL_DIR) { $env:PI_INSTALL_DIR } else { "" }),
  [switch]$CoreOnly,
  [string]$Binaries = $(if ($env:PI_BINARIES) { $env:PI_BINARIES } else { "" }),
  [switch]$AsPiRs,
  [switch]$NoAlias,
  [switch]$Uninstall,
  [switch]$Yes,
  [switch]$Force,
  [switch]$NoModifyPath,
  [switch]$NoStart,
  [string]$Repo = $(if ($env:PI_REPO) { $env:PI_REPO } else { "remedialchaos/pi-rs" }),
  [string]$ReleaseBaseUrl = $(if ($env:PI_RELEASE_BASE_URL) { $env:PI_RELEASE_BASE_URL } else { "" }),
  [string]$Archive = $(if ($env:PI_ARCHIVE_PATH) { $env:PI_ARCHIVE_PATH } else { "" }),
  [string]$Checksums = $(if ($env:PI_CHECKSUMS_PATH) { $env:PI_CHECKSUMS_PATH } else { "" }),
  [switch]$Help
)

$ErrorActionPreference = "Stop"

function Test-EnvTruthy {
  param([string]$Value)
  if ([string]::IsNullOrWhiteSpace($Value)) { return $false }
  return $Value.Trim().ToLowerInvariant() -in @("1", "true", "yes", "on")
}

if (-not $CoreOnly -and (Test-EnvTruthy $env:PI_CORE_ONLY)) { $CoreOnly = $true }
if (-not $AsPiRs -and (Test-EnvTruthy $env:PI_AS_PI_RS)) { $AsPiRs = $true }
if (-not $NoAlias -and (Test-EnvTruthy $env:PI_NO_ALIAS)) { $NoAlias = $true }
if (-not $Uninstall -and (Test-EnvTruthy $env:PI_UNINSTALL)) { $Uninstall = $true }
if (-not $Yes -and (Test-EnvTruthy $env:PI_YES)) { $Yes = $true }
if (-not $Force -and (Test-EnvTruthy $env:PI_FORCE)) { $Force = $true }
if (-not $NoModifyPath -and (Test-EnvTruthy $env:PI_NO_MODIFY_PATH)) { $NoModifyPath = $true }
if (-not $NoStart -and (Test-EnvTruthy $env:PI_NO_START)) { $NoStart = $true }

if ($Help) {
  Get-Help -Full $PSCommandPath
  exit 0
}

$Target = "x86_64-pc-windows-msvc"
$MarkerName = "pi-rs-install.json"
$UpstreamMarkerName = "managed-install.json"
$AllBinaries = @("pi", "pi-ai", "pi-env", "pi-evals", "p3", "p2", "pi-voice-server")

$AgentDir = if ($env:PI_CODING_AGENT_DIR) {
  $env:PI_CODING_AGENT_DIR.TrimEnd('\', '/')
} else {
  Join-Path $env:USERPROFILE ".pi\agent"
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
  $DefaultInstallDir = Join-Path $AgentDir "install-rs"
  $LegacyInstallDir = Join-Path $AgentDir "install"
  if ((-not (Test-Path $DefaultInstallDir)) -and (Test-Path (Join-Path $LegacyInstallDir $MarkerName)) -and (-not (Test-Path (Join-Path $LegacyInstallDir $UpstreamMarkerName)))) {
    $InstallDir = $LegacyInstallDir
  } else {
    $InstallDir = $DefaultInstallDir
  }
}
$InstallDir = $InstallDir.TrimEnd('\', '/')

$MarkerPath = Join-Path $InstallDir $MarkerName
$ExistingMarker = $null
if (Test-Path $MarkerPath) {
  try {
    $ExistingMarker = Get-Content -Raw -Path $MarkerPath | ConvertFrom-Json
  } catch {}
}

$InstallMode = if ($AsPiRs) {
  "side-by-side"
} elseif ($ExistingMarker -and $ExistingMarker.installMode -eq "side-by-side") {
  "side-by-side"
} else {
  "drop-in"
}

$ExistingPiCmd = Get-Command "pi" -ErrorAction SilentlyContinue
$OtherPiPath = ""
if ($ExistingPiCmd -and $ExistingPiCmd.Source) {
  $isOwnPi = $ExistingMarker -and $ExistingMarker.binDir -and ($ExistingPiCmd.Source -like "$($ExistingMarker.binDir)\*")
  if (-not $isOwnPi) {
    $OtherPiPath = $ExistingPiCmd.Source
  }
}

if ([string]::IsNullOrWhiteSpace($BinDir)) {
  if ($ExistingMarker -and $ExistingMarker.binDir) {
    $BinDir = [string]$ExistingMarker.binDir
  } else {
    $DefaultBin = Join-Path $AgentDir "bin"
    $UpstreamDefaultPi = Join-Path $DefaultBin "pi.exe"
    $UpstreamDefaultCmd = Join-Path $DefaultBin "pi.cmd"
    if (($InstallMode -eq "drop-in") -and (-not $ExistingMarker) -and ((Test-Path $UpstreamDefaultPi) -or (Test-Path $UpstreamDefaultCmd))) {
      $BinDir = Join-Path $AgentDir "bin-rs"
    } else {
      $BinDir = $DefaultBin
    }
  }
}
$BinDir = $BinDir.TrimEnd('\', '/')

function Normalize-VersionTag {
  param([string]$Tag)
  if ($Tag -match '^(v|dev-)') { return $Tag }
  return "v$Tag"
}

function Resolve-RequestedBinaries {
  if ($CoreOnly -and [string]::IsNullOrWhiteSpace($Binaries)) {
    return @("pi")
  }
  if ([string]::IsNullOrWhiteSpace($Binaries)) {
    return @()
  }
  $selected = @()
  foreach ($part in ($Binaries -split ",")) {
    $name = $part.Trim()
    if ([string]::IsNullOrWhiteSpace($name)) { continue }
    if ($AllBinaries -notcontains $name) {
      throw "Unknown binary '$name' (supported: $($AllBinaries -join ', '))"
    }
    if ($selected -notcontains $name) { $selected += $name }
  }
  if ($selected.Count -eq 0) {
    throw "-Binaries produced an empty binary list"
  }
  return $selected
}

function Resolve-LatestVersion {
  if (-not [string]::IsNullOrWhiteSpace($ReleaseBaseUrl)) {
    try {
      $verUrl = "$($ReleaseBaseUrl.TrimEnd('/'))/latest-version.txt"
      $verText = (Invoke-WebRequest -UseBasicParsing -Uri $verUrl).Content.Trim()
      if (-not [string]::IsNullOrWhiteSpace($verText)) { return $verText }
    } catch {}
  }

  $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
  try {
    $headers = @{ "User-Agent" = "pi-rs-installer" }
    $resp = Invoke-RestMethod -Uri $apiUrl -Headers $headers
    if ($resp.tag_name) { return [string]$resp.tag_name }
  } catch {
    throw "No published GitHub release found for $Repo. Specify -Version <TAG> or build from source with Cargo."
  }
  throw "Could not determine latest release tag for $Repo."
}

function Verify-Checksum {
  param(
    [string]$FilePath,
    [string]$ChecksumsPath,
    [string]$ArchiveName
  )
  $expected = $null
  foreach ($line in (Get-Content -Path $ChecksumsPath)) {
    if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?\.?/?(.+?)\s*$') {
      if ($Matches[2] -eq $ArchiveName) {
        $expected = $Matches[1].ToLowerInvariant()
        break
      }
    }
  }
  if (-not $expected) {
    throw "Archive '$ArchiveName' not listed in $ChecksumsPath"
  }
  $actual = (Get-FileHash -Algorithm SHA256 -Path $FilePath).Hash.ToLowerInvariant()
  if ($actual -ne $expected) {
    throw "SHA-256 mismatch for $ArchiveName (expected $expected, got $actual)"
  }
}

function Uninstall-PiRs {
  if ((Test-Path (Join-Path $InstallDir $UpstreamMarkerName)) -and (-not (Test-Path $MarkerPath))) {
    throw "Refusing to remove upstream managed installation at $InstallDir"
  }
  $trackedBins = @()
  $targetBinDir = $BinDir
  if ($ExistingMarker) {
    if ($ExistingMarker.binDir) { $targetBinDir = [string]$ExistingMarker.binDir }
    if ($ExistingMarker.binaries) { $trackedBins = @($ExistingMarker.binaries) }
  }
  if ($trackedBins.Count -eq 0) { $trackedBins = $AllBinaries + @("pi-rs") }

  foreach ($b in $trackedBins) {
    $exePath = Join-Path $targetBinDir "$b.exe"
    if (Test-Path $exePath) {
      Remove-Item -Force $exePath
    }
  }
  if (Test-Path $InstallDir) {
    Remove-Item -Recurse -Force $InstallDir
  }
  Write-Host "pi-rs was uninstalled successfully."
  Write-Host "Your configuration, credentials, sessions, and packages in $AgentDir were preserved."
}

function Add-ToUserPath {
  param([string]$Dir)
  $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
  $entries = @()
  if ($userPath) {
    $entries = $userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
  }
  $normalized = $Dir.TrimEnd('\')
  foreach ($entry in $entries) {
    if ($entry.TrimEnd('\').Equals($normalized, [System.StringComparison]::OrdinalIgnoreCase)) {
      return $false
    }
  }
  $newPath = if ($entries.Count -gt 0) { ($entries + $Dir) -join ';' } else { $Dir }
  [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
  $env:Path = "$Dir;$env:Path"
  return $true
}

if ($Uninstall) {
  Uninstall-PiRs
  exit 0
}

$HasManaged = $null -ne $ExistingMarker
if (-not $Yes -and -not [Console]::IsInputRedirected) {
  Write-Host ""
  Write-Host "  pi-rs Installer"
  Write-Host "  100% Rust port of pi"
  Write-Host ""
  if ($HasManaged) {
    Write-Host "Existing managed pi-rs installation found in $InstallDir (mode: $InstallMode)."
    Write-Host "  [Enter/y] Reinstall or update pi-rs"
    Write-Host "  [u]       Uninstall pi-rs"
    Write-Host "  [n]       Cancel"
    $choice = Read-Host "Action"
    if ($choice -match '^(u|U)$') {
      Uninstall-PiRs
      exit 0
    }
    if ($choice -match '^(n|N)$') {
      Write-Host "Cancelled."
      exit 0
    }
  } elseif (-not [string]::IsNullOrWhiteSpace($OtherPiPath) -and ($InstallMode -ne "side-by-side")) {
    Write-Host "Another pi is already on PATH: $OtherPiPath"
    Write-Host "This installer will not overwrite or remove it."
    Write-Host "  [a]       Install side-by-side as pi-rs (keep existing pi untouched)"
    Write-Host "  [y]       Install as pi (and pi-rs) alongside it"
    Write-Host "  [Enter/n] Cancel"
    $choice = Read-Host "Action"
    if ($choice -match '^(a|A)$') {
      $InstallMode = "side-by-side"
    } elseif ($choice -notmatch '^(y|Y)$') {
      Write-Host "Cancelled."
      exit 0
    }
  } else {
    Write-Host "Install directory: $InstallDir"
    Write-Host "Bin directory:     $BinDir"
    Write-Host "Command mode:      $InstallMode"
    $choice = Read-Host "Proceed with install? [Y/n]"
    if ($choice -match '^(n|N)$') {
      Write-Host "Cancelled."
      exit 0
    }
  }
} elseif ((-not [string]::IsNullOrWhiteSpace($OtherPiPath)) -and ($InstallMode -ne "side-by-side") -and (-not $Force)) {
  throw "Another pi is already on PATH ($OtherPiPath). Pass -AsPiRs to install side-by-side as pi-rs, or -Force to install alongside it."
}

$WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ("pi-rs-installer-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

try {
  $ResolvedVersion = ""
  $ArchivePath = ""
  $ArchiveName = ""

  if (-not [string]::IsNullOrWhiteSpace($Archive)) {
    if (-not (Test-Path $Archive)) { throw "Archive not found: $Archive" }
    $ArchivePath = (Resolve-Path $Archive).Path
    $ArchiveName = Split-Path -Leaf $ArchivePath
    $ResolvedVersion = if ($Version -eq "latest") { "local" } else { Normalize-VersionTag $Version }
    if (-not [string]::IsNullOrWhiteSpace($Checksums)) {
      Write-Host "Verifying SHA-256 checksum for $ArchiveName..."
      Verify-Checksum -FilePath $ArchivePath -ChecksumsPath $Checksums -ArchiveName $ArchiveName
    }
  } else {
    if ($Version -eq "latest") {
      Write-Host "Resolving latest release for $Repo..."
      $ResolvedVersion = Resolve-LatestVersion
    } else {
      $ResolvedVersion = Normalize-VersionTag $Version
    }

    $BaseUrl = if (-not [string]::IsNullOrWhiteSpace($ReleaseBaseUrl)) {
      $ReleaseBaseUrl.TrimEnd('/')
    } else {
      "https://github.com/$Repo/releases/download"
    }

    $ArchiveName = "pi-rs-$ResolvedVersion-$Target.zip"
    $ArchivePath = Join-Path $WorkDir $ArchiveName
    $ChecksumsPath = Join-Path $WorkDir "SHA256SUMS.txt"

    Write-Host "Downloading $ArchiveName ($ResolvedVersion)..."
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$ResolvedVersion/$ArchiveName" -OutFile $ArchivePath

    Write-Host "Downloading and verifying SHA256SUMS.txt..."
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$ResolvedVersion/SHA256SUMS.txt" -OutFile $ChecksumsPath
    Verify-Checksum -FilePath $ArchivePath -ChecksumsPath $ChecksumsPath -ArchiveName $ArchiveName
  }

  $ExtractDir = Join-Path $WorkDir "extract"
  Expand-Archive -Path $ArchivePath -DestinationPath $ExtractDir -Force

  $PayloadDir = Join-Path $ExtractDir "pi-rs-$ResolvedVersion-$Target"
  if (-not (Test-Path $PayloadDir)) {
    $subDirs = @(Get-ChildItem -Directory $ExtractDir)
    $PayloadDir = if ($subDirs.Count -eq 1) { $subDirs[0].FullName } else { $ExtractDir }
  }

  $Requested = Resolve-RequestedBinaries
  $ReleaseDir = Join-Path (Join-Path $InstallDir "releases") $ResolvedVersion
  $StageDir = Join-Path (Join-Path $InstallDir "staging") ("install-" + [System.Guid]::NewGuid().ToString("N"))
  $StageBinDir = Join-Path $StageDir "bin"
  New-Item -ItemType Directory -Force -Path $StageBinDir, $BinDir | Out-Null

  $StagedBins = @()
  $Candidates = if ($Requested.Count -gt 0) { $Requested } else { $AllBinaries }
  foreach ($binName in $Candidates) {
    $srcExe = Join-Path $PayloadDir "$binName.exe"
    if (Test-Path $srcExe) {
      Copy-Item -Force $srcExe (Join-Path $StageBinDir "$binName.exe")
      $StagedBins += $binName
    } elseif ($Requested.Count -gt 0) {
      throw "Requested binary '$binName.exe' was not found in archive"
    }
  }
  if ($StagedBins.Count -eq 0) {
    throw "No pi-rs binaries found inside release archive"
  }

  $StagedPi = Join-Path $StageBinDir "pi.exe"
  if (Test-Path $StagedPi) {
    $verOut = & $StagedPi --version 2>&1
    if ($LASTEXITCODE -ne 0) {
      throw "Smoke test failed for staged pi.exe: $verOut"
    }
  }

  if (Test-Path $ReleaseDir) { Remove-Item -Recurse -Force $ReleaseDir }
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ReleaseDir) | Out-Null
  Move-Item -Force $StageDir $ReleaseDir

  $PublishedCommands = @()
  foreach ($binName in $StagedBins) {
    if (($binName -eq "pi") -and ($InstallMode -eq "side-by-side")) {
      continue
    }
    Copy-Item -Force (Join-Path (Join-Path $ReleaseDir "bin") "$binName.exe") (Join-Path $BinDir "$binName.exe")
    $PublishedCommands += $binName
  }

  if (($StagedBins -contains "pi") -and (($InstallMode -eq "side-by-side") -or (-not $NoAlias))) {
    Copy-Item -Force (Join-Path (Join-Path $ReleaseDir "bin") "pi.exe") (Join-Path $BinDir "pi-rs.exe")
    $PublishedCommands += "pi-rs"
  }

  $MarkerObj = [ordered]@{
    kind          = "pi-rs-managed-install"
    schemaVersion = 1
    layout        = "releases-v1"
    installMode   = $InstallMode
    version       = $ResolvedVersion
    target        = $Target
    source        = "release"
    binDir        = $BinDir
    binaries      = $PublishedCommands
  }
  $MarkerJson = $MarkerObj | ConvertTo-Json -Depth 4
  [System.IO.File]::WriteAllText((Join-Path $InstallDir $MarkerName), $MarkerJson + "`n", (New-Object System.Text.UTF8Encoding($false)))
  [System.IO.File]::WriteAllText((Join-Path $InstallDir "current-version"), $ResolvedVersion + "`n", (New-Object System.Text.UTF8Encoding($false)))

  # Prune old releases.
  $ReleasesRoot = Join-Path $InstallDir "releases"
  foreach ($entry in (Get-ChildItem -Directory $ReleasesRoot)) {
    if ($entry.Name -ne $ResolvedVersion) {
      Remove-Item -Recurse -Force $entry.FullName
    }
  }
  $StagingRoot = Join-Path $InstallDir "staging"
  if (Test-Path $StagingRoot) { Remove-Item -Recurse -Force $StagingRoot }

  $PrimaryCmd = if ($InstallMode -eq "side-by-side") { "pi-rs" } else { "pi" }
  Write-Host ""
  Write-Host "pi-rs ($ResolvedVersion) was installed successfully."
  Write-Host "Installed commands ($BinDir): $($PublishedCommands -join ', ')"
  if (-not [string]::IsNullOrWhiteSpace($OtherPiPath)) {
    Write-Host "Shared state note: both pi and pi-rs use $AgentDir by default (override with PI_CODING_AGENT_DIR)."
  }

  if (-not $NoModifyPath) {
    if (Add-ToUserPath -Dir $BinDir) {
      Write-Host "Added $BinDir to your user PATH. Open a new terminal to use $PrimaryCmd."
    }
  }

  if (-not $NoStart -and -not $Yes -and -not [Console]::IsInputRedirected -and ($PublishedCommands -contains $PrimaryCmd)) {
    $startChoice = Read-Host "Start $PrimaryCmd now? [Y/n]"
    if ($startChoice -notmatch '^(n|N)') {
      & (Join-Path $BinDir "$PrimaryCmd.exe")
    }
  }
} finally {
  if (Test-Path $WorkDir) {
    Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
  }
}
