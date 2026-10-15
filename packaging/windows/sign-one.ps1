#Requires -Version 5.1
# Sign files with Azure Trusted Signing. Inno Setup runs this as its SignTool (build-windows.ps1
# passes it with ISCC /S) for the uninstaller it embeds and for the finished installer.
[CmdletBinding()]
param([Parameter(Mandatory, Position = 0, ValueFromRemainingArguments)][string[]]$Path)
$ErrorActionPreference = 'Stop'
try {
    . "$PSScriptRoot/trusted-signing.ps1"
    Invoke-TrustedSign -Context (New-TrustedSigningContext) -Path $Path
} catch {
    Write-Error $_
    exit 1
}
exit 0
