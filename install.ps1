# reman installer for Windows.
#   irm https://raw.githubusercontent.com/rdarshan10/reman/master/install.ps1 | iex
# Downloads the latest release, checks its SHA-256, and runs `reman setup`, which copies reman into
# ~\.reman\bin, starts the background daemon, and wires PowerShell and Command Prompt.
# It then connects every AI agent it finds (`reman connect all`; undo with `reman disconnect all`).
# To pin a version, skip setup, or skip connecting agents, download this file and run:
#   .\install.ps1 -Version v0.1.0 -NoSetup -NoConnect      ($env:REMAN_NO_CONNECT=1 also skips it)
param(
  [string]$Version = "latest",
  [switch]$NoSetup,
  [switch]$NoConnect
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest is many times faster without the bar
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo = "rdarshan10/reman"
$name = "reman-windows-x64"
if (-not [Environment]::Is64BitOperatingSystem) { throw "reman needs 64-bit Windows." }

$base = if ($Version -eq "latest") { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$Version" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("reman-install-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
  Write-Host "Downloading $name ($Version)..."
  $zip = Join-Path $tmp "$name.zip"
  Invoke-WebRequest "$base/$name.zip" -OutFile $zip -UseBasicParsing
  Invoke-WebRequest "$base/SHA256SUMS.txt" -OutFile (Join-Path $tmp "SHA256SUMS.txt") -UseBasicParsing

  $want = (Get-Content (Join-Path $tmp "SHA256SUMS.txt") | Where-Object { $_ -match "\s\*?$name\.zip$" }) -split '\s+' | Select-Object -First 1
  $got = (Get-FileHash $zip -Algorithm SHA256).Hash
  if (-not $want -or $got -ne $want.ToUpper()) { throw "Checksum mismatch for $name.zip (expected $want, got $got). Not installing." }
  Write-Host "Checksum OK."

  Expand-Archive $zip -DestinationPath $tmp -Force
  $exe = Join-Path $tmp "reman.exe"

  $bin = Join-Path $HOME ".reman\bin"
  if ($NoSetup) {
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item $exe, (Join-Path $tmp "reman-hook.exe") $bin -Force
    Write-Host "Installed to $bin (setup skipped)."
  } else {
    & $exe setup
    if ($LASTEXITCODE -ne 0) { throw "reman setup failed (exit $LASTEXITCODE)." }
    # plug into every AI agent that's installed (Claude Code, Codex, Cursor, VS Code, ...). Each
    # config file gets a .reman-bak backup; `reman disconnect all` undoes it.
    if (-not $NoConnect -and -not $env:REMAN_NO_CONNECT) {
      Write-Host ""
      & (Join-Path $bin "reman.exe") connect all
    }
  }

  # `reman` on the user PATH, so new terminals of any kind can run it
  $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if (($userPath -split ';') -notcontains $bin) {
    [Environment]::SetEnvironmentVariable("Path", ($(if ($userPath) { "$userPath;" } else { "" }) + $bin), "User")
    Write-Host "Added $bin to your user PATH."
  }

  Write-Host ""
  Write-Host "reman is installed. Open a new terminal, then:" -ForegroundColor Green
  Write-Host "  PowerShell      press Up for the finder, Ctrl+R to search everywhere"
  Write-Host "  Command Prompt  type r (this folder) or rr (everywhere)"
  Write-Host "  AI agents       reman connect   (see what's connected; reman disconnect all to undo)"
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
