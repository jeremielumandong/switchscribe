<#
.SYNOPSIS
  Build the switchscribe Inno Setup installer: dist\switchscribe-setup-<version>-x64.exe

.DESCRIPTION
  The name is fixed: the in-app update checker (crates/integrations/src/update.rs) looks for
  exactly switchscribe-setup-<version>-x64.exe and its .sha256 on the latest release.

.PARAMETER SkipBuild
  Reuse the existing release build in target\x86_64-pc-windows-msvc\release.

.PARAMETER Sign
  Sign with Azure Trusted Signing (see trusted-signing.ps1): the exe, the unsigned DLLs
  beside it, the uninstaller Inno Setup embeds, and the installer; then verify the
  signatures. Needs AZURE_CODESIGNING_ENDPOINT / _ACCOUNT / _PROFILE,
  SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT and an Azure login. The release workflow builds first,
  signs in, then runs -Sign -SkipBuild.

.NOTES
  Needs Rust (rustup), CMake, the Visual Studio C++ build tools and Inno Setup 6
  (`winget install JRSoftware.InnoSetup` or `choco install innosetup`).
  The Visual C++ runtime DLLs ship next to the exe (app-local), so the installer needs no
  redistributable.
  Without -Sign, optional certificate signing (skipped when unset) of the exe, the DLLs and the installer:
    WINDOWS_SIGN_THUMBPRINT   SHA-1 thumbprint of a cert in the user/machine store, or
    WINDOWS_SIGN_PFX + WINDOWS_SIGN_PFX_PASSWORD
    WINDOWS_SIGN_TIMESTAMP    RFC 3161 URL (default http://timestamp.digicert.com)
#>
param([switch]$SkipBuild, [switch]$Sign)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$DistDir = if ($env:DIST_DIR) { $env:DIST_DIR } else { Join-Path $RepoRoot 'dist' }
$Target = 'x86_64-pc-windows-msvc'

function Log($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# Version from [workspace.package] in the root Cargo.toml.
$Version = $env:VERSION
if (-not $Version) {
  $m = Select-String -Path (Join-Path $RepoRoot 'Cargo.toml') -Pattern '^version = "(.+)"' | Select-Object -First 1
  if (-not $m) { throw 'could not read version from Cargo.toml' }
  $Version = $m.Matches[0].Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
  throw "'$Version' is not X.Y.Z (the installer and the update checker need a plain version)"
}

Set-Location $RepoRoot

if ($Sign) {
  . (Join-Path $PSScriptRoot 'trusted-signing.ps1')
  if (-not $env:SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT) { throw 'Set SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT to the certificate profile subject.' }
  $Signing = New-TrustedSigningContext
  # Child processes (ISCC -> sign-one.ps1) reuse the same tools.
  $env:SWITCHSCRIBE_SIGN_SIGNTOOL = $Signing.SignTool
  $env:SWITCHSCRIBE_SIGN_DLIB = $Signing.Dlib
}

if (-not $SkipBuild) {
  # whisper.cpp builds for the build machine's CPU unless told otherwise; a release must run
  # on other machines (AVX2 baseline). See DECISIONS.
  if (-not $env:GGML_NATIVE) { $env:GGML_NATIVE = 'OFF' }
  rustup target add $Target | Out-Null
  Log "cargo build --release --target $Target"
  cargo build --release --locked --target $Target -p switchscribe-app
  if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}

$BinDir = Join-Path $RepoRoot "target\$Target\release"
if (-not (Test-Path (Join-Path $BinDir 'switchscribe.exe'))) { throw "missing $BinDir\switchscribe.exe (run without -SkipBuild)" }

# --- signing ------------------------------------------------------------------------------
function Find-SignTool {
  $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
  $found = Get-ChildItem $kits -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1
  if ($found) { return $found.FullName }
  throw 'signtool.exe not found (install the Windows SDK)'
}

function Sign-File($path) {
  if ($Sign) {
    Log "signing $(Split-Path $path -Leaf) (Azure Trusted Signing)"
    Invoke-TrustedSign -Context $Signing -Path $path
    return
  }
  if (-not $env:WINDOWS_SIGN_THUMBPRINT -and -not $env:WINDOWS_SIGN_PFX) { return }
  $tool = Find-SignTool
  $ts = if ($env:WINDOWS_SIGN_TIMESTAMP) { $env:WINDOWS_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }
  $signArgs = @('sign', '/fd', 'sha256', '/tr', $ts, '/td', 'sha256')
  if ($env:WINDOWS_SIGN_THUMBPRINT) {
    $signArgs += @('/sha1', $env:WINDOWS_SIGN_THUMBPRINT)
  } else {
    $signArgs += @('/f', $env:WINDOWS_SIGN_PFX)
    if ($env:WINDOWS_SIGN_PFX_PASSWORD) { $signArgs += @('/p', $env:WINDOWS_SIGN_PFX_PASSWORD) }
  }
  Log "signing $(Split-Path $path -Leaf)"
  & $tool @signArgs $path
  if ($LASTEXITCODE -ne 0) { throw "signing $path failed" }
}

# Stage and sign copies so target\ keeps the unsigned build artifacts untouched.
$Stage = Join-Path $RepoRoot 'target\package\windows'
Remove-Item $Stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage, $DistDir | Out-Null
Copy-Item (Join-Path $BinDir 'switchscribe.exe') $Stage
# sherpa-onnx (speaker identification) and onnxruntime are DLLs its build script copies next
# to the exe; they must ship beside it.
$dlls = @(Get-ChildItem $BinDir -Filter *.dll -File)
if ($dlls.Count -eq 0) { throw "no DLLs in $BinDir (sherpa-onnx's)" }
$dlls | Copy-Item -Destination $Stage
# The Visual C++ runtime, app-local (Microsoft supports this for VS 2015-2022), so a clean
# Windows needs no redistributable. These are already signed by Microsoft.
foreach ($dll in 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll') {
  $src = Join-Path $env:SystemRoot "System32\$dll"
  if (-not (Test-Path $src)) { throw "missing $src (install the Visual C++ redistributable)" }
  Copy-Item $src $Stage -Force
}
Sign-File (Join-Path $Stage 'switchscribe.exe')
foreach ($dll in Get-ChildItem $Stage -Filter *.dll -File) {
  if ((Get-AuthenticodeSignature -LiteralPath $dll.FullName).Status -eq 'NotSigned') { Sign-File $dll.FullName }
}

# --- Inno Setup ---------------------------------------------------------------------------
$iscc = Get-Command ISCC.exe -ErrorAction SilentlyContinue
$iscc = if ($iscc) { $iscc.Source } else {
  @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) { throw 'ISCC.exe not found. Install Inno Setup 6: winget install JRSoftware.InnoSetup' }

$OutFile = Join-Path $DistDir "switchscribe-setup-$Version-x64.exe"
Remove-Item $OutFile -Force -ErrorAction SilentlyContinue
Log "creating $(Split-Path $OutFile -Leaf)"
$isccArgs = @('/Q', "/DAppVersion=$Version", "/DSourceDir=$Stage", "/O$DistDir")
# With -Sign, Inno Setup signs the uninstaller it embeds and the finished installer itself
# through sign-one.ps1. $q is a quote and $f the file in Inno's sign tool syntax.
if ($Sign) {
  $signOne = Join-Path $PSScriptRoot 'sign-one.ps1'
  $isccArgs += '/DSign'
  $isccArgs += "/Ssignone=powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `$q$signOne`$q `$f"
}
& $iscc @isccArgs (Join-Path $PSScriptRoot 'switchscribe.iss')
if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }
if (-not (Test-Path $OutFile)) { throw "$OutFile was not produced" }

if ($Sign) {
  & (Join-Path $PSScriptRoot 'verify-signatures.ps1') -Installer $OutFile -StageDir $Stage
} else {
  Sign-File $OutFile
}
Log "done -> $OutFile"
Get-Item $OutFile | Format-Table Name, Length
