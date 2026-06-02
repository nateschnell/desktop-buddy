# claude-buddy installer (Windows)
#
# One command (PowerShell):
#   irm https://raw.githubusercontent.com/nateschnell/desktop-buddy/main/install.ps1 | iex
#
# Downloads the prebuilt binary, installs it, and wires it into Claude Code.
#
# Env overrides: CLAUDE_BUDDY_REPO, CLAUDE_BUDDY_VERSION, CLAUDE_BUDDY_NO_SETUP

$ErrorActionPreference = 'Stop'

$repo    = if ($env:CLAUDE_BUDDY_REPO)    { $env:CLAUDE_BUDDY_REPO }    else { 'nateschnell/desktop-buddy' }
$version = if ($env:CLAUDE_BUDDY_VERSION) { $env:CLAUDE_BUDDY_VERSION } else { 'latest' }
$binDir  = Join-Path $env:LOCALAPPDATA 'claude-buddy'

$arch = if ([Environment]::Is64BitOperatingSystem) {
  if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
} else { throw 'unsupported 32-bit OS' }
$target = "$arch-pc-windows-msvc"
$asset  = "claude-buddy-$target.zip"

$url = if ($version -eq 'latest') {
  "https://github.com/$repo/releases/latest/download/$asset"
} else {
  "https://github.com/$repo/releases/download/$version/$asset"
}

Write-Host "Installing claude-buddy for $target..." -ForegroundColor Cyan
$tmp = Join-Path $env:TEMP ("claude-buddy-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $zip = Join-Path $tmp $asset
  try {
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  } catch {
    throw "download failed: $url`n(If no release is published yet, build locally:`n   cd bridge; cargo build --release; copy target\release\claude-buddy.exe `"$binDir`")"
  }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Path $binDir -Force | Out-Null
  Copy-Item (Join-Path $tmp 'claude-buddy.exe') (Join-Path $binDir 'claude-buddy.exe') -Force
  Write-Host "OK installed $binDir\claude-buddy.exe" -ForegroundColor Green

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

  if ($env:CLAUDE_BUDDY_NO_SETUP -ne '1') {
    Write-Host "Wiring into Claude Code..." -ForegroundColor Cyan
    & (Join-Path $binDir 'claude-buddy.exe') setup
  }
  Write-Host "Done. Power on your buddy, then run: claude-buddy pair"
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
