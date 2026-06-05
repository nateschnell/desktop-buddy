# agent-buddy installer (Windows)
#
# One command (PowerShell):
#   irm https://raw.githubusercontent.com/nateschnell/agent-buddy/main/install.ps1 | iex
#
# Downloads the prebuilt binary, installs it, and wires it into Claude Code.
#
# Env overrides: AGENT_BUDDY_REPO, AGENT_BUDDY_VERSION, AGENT_BUDDY_NO_SETUP,
#                AGENT_BUDDY_UNINSTALL=1 (remove everything)

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 can still default to TLS 1.0/1.1; GitHub requires 1.2+,
# so a download would otherwise fail with an opaque error. Enable it explicitly.
try {
  [Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

$repo    = if ($env:AGENT_BUDDY_REPO)    { $env:AGENT_BUDDY_REPO }    else { 'nateschnell/agent-buddy' }
$version = if ($env:AGENT_BUDDY_VERSION) { $env:AGENT_BUDDY_VERSION } else { 'latest' }
$binDir  = Join-Path $env:LOCALAPPDATA 'agent-buddy'

# --- uninstall mode --------------------------------------------------------
# Reverse everything: the installed binary's own `uninstall` knows every
# artifact (hooks, daemon, tasks, launcher, state); then remove the binary.
if ($env:AGENT_BUDDY_UNINSTALL -eq '1') {
  Write-Host "Uninstalling agent-buddy..." -ForegroundColor Cyan
  $exe = Join-Path $binDir 'agent-buddy.exe'
  if (Test-Path $exe) {
    & $exe uninstall
    Remove-Item (Join-Path $binDir 'agent-buddy.exe') -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $binDir 'firmware*') -Force -ErrorAction SilentlyContinue
    Write-Host "OK removed $exe" -ForegroundColor Green
  } else {
    Write-Host "  no agent-buddy.exe at $binDir - nothing to remove"
  }
  Write-Host "Done."
  return
}

$arch = if ([Environment]::Is64BitOperatingSystem) {
  if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
} else { throw 'unsupported 32-bit OS' }
$target = "$arch-pc-windows-msvc"
$asset  = "agent-buddy-$target.zip"

$base = if ($version -eq 'latest') {
  "https://github.com/$repo/releases/latest/download"
} else {
  "https://github.com/$repo/releases/download/$version"
}
$url     = "$base/$asset"
$sumsUrl = "$base/SHA256SUMS"

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

  # Verify the archive against the release's published SHA256SUMS before trusting
  # it - catches a corrupt/partial download or a tampered asset. Fail closed.
  $sums = Join-Path $tmp 'SHA256SUMS'
  try {
    Invoke-WebRequest -Uri $sumsUrl -OutFile $sums -UseBasicParsing
  } catch {
    throw "could not fetch SHA256SUMS from the release - refusing to install an unverified binary"
  }
  $expected = $null
  foreach ($line in Get-Content $sums) {
    $parts = $line -split '\s+', 2
    if ($parts.Count -eq 2 -and $parts[1].TrimStart('*').Trim() -eq $asset) {
      $expected = $parts[0].ToLower(); break
    }
  }
  if (-not $expected) { throw "SHA256SUMS has no entry for $asset" }
  $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
  if ($actual -ne $expected) {
    throw "checksum mismatch for $asset`n  expected: $expected`n  actual:   $actual`nRefusing to install."
  }
  Write-Host "OK verified $asset (sha256)" -ForegroundColor Green

  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exeSrc = Join-Path $tmp 'agent-buddy.exe'
  if (-not (Test-Path $exeSrc)) { throw "archive did not contain agent-buddy.exe" }
  New-Item -ItemType Directory -Path $binDir -Force | Out-Null
  Copy-Item $exeSrc (Join-Path $binDir 'agent-buddy.exe') -Force
  Write-Host "OK installed $binDir\agent-buddy.exe" -ForegroundColor Green

  # The per-board firmware images + their versions ride along in the archive
  # (firmware-<board>.bin / .version, plus a legacy firmware.bin = CYD). Drop
  # them all next to the binary so setup (and the desktop app) can bundle them
  # and offer over-the-air updates to whichever board is connected.
  foreach ($src in Get-ChildItem -Path $tmp -Filter 'firmware*' -File) {
    Copy-Item $src.FullName (Join-Path $binDir $src.Name) -Force
  }

  # Add to user PATH if missing. Split on ';' and match exactly so a partial
  # overlap can't fool the check and we never emit a stray ';;'.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $entries  = @()
  if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ -ne '' } }
  if ($entries -notcontains $binDir) {
    $newPath = (($entries + $binDir) -join ';')
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
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
