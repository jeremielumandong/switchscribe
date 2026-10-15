#Requires -Version 5.1
# Fail closed before a Windows release is uploaded: the staged exe and the installer must carry
# a valid, timestamped signature from the expected publisher, and every staged DLL a valid one.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Installer,
    [string]$StageDir,
    [string]$ExpectedSubject = $env:SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security') -ErrorAction Stop
if (-not $ExpectedSubject) { throw 'Set SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT to the certificate profile subject.' }
if (-not $StageDir) { $StageDir = Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path 'target\package\windows' }

# The installer and the exe carry our signature.
foreach ($path in @($Installer, (Join-Path $StageDir 'switchscribe.exe'))) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing file to verify: $path" }
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate) {
        throw "Invalid or untimestamped signature: $path ($($signature.Status))"
    }
    if ($signature.SignerCertificate.Subject -cne $ExpectedSubject) {
        throw "Unexpected publisher for $path : $($signature.SignerCertificate.Subject)"
    }
    Write-Host "Verified: $path"
}
# The DLLs beside it: ours where they came unsigned (sherpa-onnx, onnxruntime), Microsoft's
# for the Visual C++ runtime. Every one must be validly signed.
foreach ($dll in Get-ChildItem -LiteralPath $StageDir -Filter *.dll -File) {
    $signature = Get-AuthenticodeSignature -LiteralPath $dll.FullName
    if ($signature.Status -ne 'Valid') {
        throw "Invalid signature: $($dll.FullName) ($($signature.Status))"
    }
    Write-Host "Verified: $($dll.FullName) ($($signature.SignerCertificate.Subject))"
}
