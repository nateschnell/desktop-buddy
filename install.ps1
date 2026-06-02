# agent-buddy installer (Windows)
#
# One command (PowerShell):
#   irm https://raw.githubusercontent.com/nateschnell/agent-buddy/main/install.ps1 | iex
#
# Downloads the prebuilt binary, installs it, and wires it into Claude Code.
#
# Env overrides: AGENT_BUDDY_REPO, AGENT_BUDDY_VERSION, AGENT_BUDDY_NO_SETUP

$ErrorActionPreference = 'Stop'

$repo    = if ($env:AGENT_BUDDY_REPO)    { $env:AGENT_BUDDY_REPO }    else { 'nateschnell/agent-buddy' }
$version = if ($env:AGENT_BUDDY_VERSION) { $env:AGENT_BUDDY_VERSION } else { 'latest' }
$binDir  = Join-Path $env:LOCALAPPDATA 'agent-buddy'

$arch = if ([Environment]::Is64BitOperatingSystem) {
  if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
} else { throw 'unsupported 32-bit OS' }
$target = "$arch-pc-windows-msvc"
$asset  = "agent-buddy-$target.zip"

$url = if ($version -eq 'latest') {
  "https://github.com/$repo/releases/latest/download/$asset"
} else {
  "https://github.com/$repo/releases/download/$version/$asset"
}

Write-Host "Installing agent-buddy for $target..." -ForegroundColor Cyan
$tmp = Join-Path $env:TEMP ("agent-buddy-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $zip = Join-Path $tmp $asset
  try {
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  } catch {
    throw "download failed: $url`n(If no release is published yet, build locally:`n   cd bridge; cargo build --release; copy target\release\agent-buddy.exe `"$binDir`")"
  }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Path $binDir -Force | Out-Null
  Copy-Item (Join-Path $tmp 'agent-buddy.exe') (Join-Path $binDir 'agent-buddy.exe') -Force
  Write-Host "OK installed $binDir\agent-buddy.exe" -ForegroundColor Green

  # The per-board firmware images + their versions ride along in the archive
  # (firmware-<board>.bin / .version, plus a legacy firmware.bin = CYD). Drop
  # them all next to the binary so setup (and the desktop app) can bundle them
  # and offer over-the-air updates to whichever board is connected.
  foreach ($src in Get-ChildItem -Path $tmp -Filter 'firmware*' -File) {
    Copy-Item $src.FullName (Join-Path $binDir $src.Name) -Force
  }

  # Add to user PATH if missing.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$binDir*") {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$binDir", 'User')
    Write-Host "  added $binDir to your PATH (restart the shell to pick it up)"
  }

  if ($env:AGENT_BUDDY_NO_SETUP -ne '1') {
    Write-Host "Wiring into Claude Code..." -ForegroundColor Cyan
    & (Join-Path $binDir 'agent-buddy.exe') setup
  }
  Write-Host "Done. Power on your buddy, then run: agent-buddy pair"
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
