# Build the SodaM portable zip from an already-built Windows binary.
#
# Usage (from repo root, after `cargo build --release --locked --workspace`):
#   pwsh -File packaging/windows/make-portable-zip.ps1 [-Version <ver>] [-Binary <path>]
#
# Output: release/sodam-<version>-windows-x64-portable.zip (+ .sha256)
# Prints the zip path on stdout, like the other packaging scripts.
#
# Layout inside the zip (extracts to a top-level SodaM/ folder):
#   SodaM/sodam.exe
#   SodaM/portable.txt   <- marker: makes config/cache/downloads live in this folder
#   SodaM/down/          <- default download folder in portable mode
#   SodaM/LICENSE
#
# NOTE: keep this file ASCII-only. Windows PowerShell 5.1 reads .ps1 as ANSI
# unless it has a BOM, so non-ASCII bytes can break parsing on older hosts.

[CmdletBinding()]
param(
  [string]$Version,
  [string]$Binary = "target/release/sodam.exe",
  [string]$OutputDir = "release"
)

$ErrorActionPreference = "Stop"

# Repo root = two levels up from this script (packaging/windows/ -> repo).
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
Set-Location $repoRoot

if (-not $Version) {
  $match = Select-String -Path "Cargo.toml" -Pattern '^version = "(.*)"' | Select-Object -First 1
  if (-not $match) { throw "cannot read version from Cargo.toml" }
  $Version = $match.Matches.Groups[1].Value
}
if (-not (Test-Path -LiteralPath $Binary)) {
  throw "$Binary is missing; run 'cargo build --release --locked --workspace' first"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$outputRoot = (Resolve-Path $OutputDir).Path
$zip = Join-Path $outputRoot "sodam-$Version-windows-x64-portable.zip"
Remove-Item -Force $zip -ErrorAction SilentlyContinue
Remove-Item -Force "$zip.sha256" -ErrorAction SilentlyContinue

# Stage into a temp folder so the zip only ever contains what we put there.
$stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("sodam-portable-" + [guid]::NewGuid().ToString("N"))
$appDir = Join-Path $stageRoot "SodaM"

try {
  New-Item -ItemType Directory -Force -Path (Join-Path $appDir "down") | Out-Null
  Copy-Item -LiteralPath $Binary -Destination (Join-Path $appDir "sodam.exe")
  if (Test-Path -LiteralPath "LICENSE") {
    Copy-Item -LiteralPath "LICENSE" -Destination (Join-Path $appDir "LICENSE")
  }

  # The marker file is what switches the app into portable mode. Its mere
  # presence next to sodam.exe is the whole contract (see sodam-core/src/paths.rs).
  $marker = @(
    "SodaM portable mode marker.",
    "",
    "Keep this file next to sodam.exe to keep config, cache and downloads inside",
    "this folder (data/, cache/, down/).",
    "Delete it to fall back to the per-user AppData locations."
  ) -join "`r`n"
  Set-Content -LiteralPath (Join-Path $appDir "portable.txt") -Value $marker -Encoding ascii

  # bsdtar ships with Windows 10 1803+ and writes standards-compliant zip
  # entries with forward slashes. Compress-Archive writes backslashes, which
  # non-Windows extractors turn into literal filenames.
  Push-Location $stageRoot
  try {
    tar -a -c -f $zip SodaM
    if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code $LASTEXITCODE" }
  } finally {
    Pop-Location
  }

  if (-not (Test-Path -LiteralPath $zip)) { throw "zip not found: $zip" }
  $size = (Get-Item -LiteralPath $zip).Length
  if ($size -le 0) { throw "zip is empty: $zip" }

  $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower()
  "$hash  $(Split-Path $zip -Leaf)" | Out-File -FilePath "$zip.sha256" -Encoding ascii

  Write-Output $zip
} finally {
  Remove-Item -Recurse -Force $stageRoot -ErrorAction SilentlyContinue
}
