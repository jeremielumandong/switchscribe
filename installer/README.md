# Installer and releases

`switchscribe.iss` is the Inno Setup 6 script for the Windows installer. It installs per
user (no admin) into `%LOCALAPPDATA%\Programs\switchscribe`: `switchscribe.exe`, every
DLL next to it (sherpa-onnx's shared libraries), a Start menu shortcut, the
`switchscribe://` URL scheme and an optional "Start with Windows" entry. Uninstalling
keeps the data folder (`%APPDATA%\switchscribe`).

## Cutting a release

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml` (plain `X.Y.Z`),
   run `cargo build` so `Cargo.lock` follows, and commit.
2. Tag and push:

   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. `.github/workflows/release.yml` checks the tag against `Cargo.toml`, builds, signs
   (when configured), builds the installer and publishes the GitHub release with:
   - `switchscribe-setup-<version>-x64.exe`
   - `switchscribe-setup-<version>-x64.exe.sha256` (lowercase hex, just the hash)

   The in-app update checker (`crates/integrations/src/update.rs`) looks for exactly
   these names on the latest non-draft, non-prerelease release.

You can also run the workflow by hand (Actions → Release → Run workflow) with a version;
it must still match `Cargo.toml`, and the release is tagged `v<version>` at that commit.

## Code signing

Unsigned builds work, but SmartScreen warns on them and the workflow writes a warning
annotation. Configure one of these in the repository settings:

| Method | Secrets | Variables |
| --- | --- | --- |
| Certificate (PFX) | `WINDOWS_CERT_PFX` (the .pfx, base64), `WINDOWS_CERT_PASSWORD` | |
| Azure Trusted Signing | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | `TRUSTED_SIGNING_ENDPOINT`, `TRUSTED_SIGNING_ACCOUNT`, `TRUSTED_SIGNING_PROFILE` |

The PFX takes precedence when both are set. Encode a certificate with
`[Convert]::ToBase64String([IO.File]::ReadAllBytes("cert.pfx")) | Set-Clipboard` in
PowerShell. The exe is signed before it is packed, then the installer; the `.sha256` is
written last, over the signed installer.

## Building the installer locally (Windows)

Needs the Rust toolchain, CMake, a C++ compiler (Visual Studio Build Tools) and
[Inno Setup 6](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`
or `choco install innosetup`).

```powershell
cargo build --release -p switchscribe-app
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\switchscribe.exe dist\
Get-ChildItem target\release -Filter *.dll -File | Copy-Item -Destination dist\
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" /DAppVersion=0.0.1 /DSourceDir="$PWD\dist" /O"out" installer\switchscribe.iss
```

The installer lands in `out\switchscribe-setup-0.0.1-x64.exe`. Test a silent upgrade the
way the app runs it:

```powershell
out\switchscribe-setup-0.0.1-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS
```

Never change the script's `AppId`: Windows recognizes upgrades and the uninstaller by it.
