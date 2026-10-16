# Packaging and releases

The same release pipeline as Switchyard and switchhead: each platform's package is built by
its own manually-run workflow on `main`, after CI passed on that exact commit, and uploaded
to one shared **draft** release for the workspace version. You publish the draft once all
three platforms are attached; publishing creates the `v<version>` tag.

| Platform | Package | Workflow | Script |
| --- | --- | --- | --- |
| Windows x64 | `switchscribe-setup-<version>-x64.exe` (Inno Setup) | `release-windows.yml` | `packaging/windows/build-windows.ps1` |
| macOS (universal) | `switchscribe-<version>-macos-universal.dmg` | `release-macos.yml` | `packaging/macos/build-macos.sh` |
| Linux x86_64 | `switchscribe-<version>-x86_64.AppImage` | `release.yml` | `packaging/linux/build-appimage.sh` |

Each package gets a `.sha256` (`<hash>  <file>`), plus a copy under a version-less name
(`switchscribe-windows-x64-setup.exe`, `switchscribe-macos-universal.dmg`,
`switchscribe-linux-x86_64.AppImage`) for "latest" download links.

Recording works on Windows (Teams process loopback) and macOS 14.2 or later (a Core Audio
process tap on Teams); the Mac app asks for the microphone and "System Audio Recording"
permissions the first time it records, using the usage strings in `Info.plist.in`, and is
signed with `switchscribe.entitlements` (the microphone under the hardened runtime). The
Linux build opens, plays, transcribes and searches existing meetings, and says capture is
not available yet when you try to record.

## Cutting a release

1. Actions → **Bump version** → Run workflow (patch / minor / major, or an exact
   version). It opens a pull request that bumps `[workspace.package] version` and
   `Cargo.lock` (`scripts/bump-version.py` does the same locally). Merge it.
2. Wait for CI on `main` to pass for that commit; the release workflows refuse to run
   otherwise.
3. Run **Windows release package**, **macOS release package** and **Linux release
   package** on `main`. The first creates the draft release `v<version>` (with generated
   notes); the others add their files to it. They share a concurrency group, so starting
   all three at once is fine.
4. Check the draft on the Releases page and publish it.

A release whose version is already published is refused: bump the version first.

The in-app updater (`crates/integrations/src/update.rs`) reads the latest published,
non-prerelease release and looks for exactly `switchscribe-setup-<version>-x64.exe` and its
`.sha256`; it installs silently on Windows and opens the release page on macOS and Linux.
So publish a draft only once the Windows installer is on it.

## Signing

Signing is optional. Without it, each workflow still builds and keeps its package as a
workflow artifact (14 days), with a warning; tick **upload_unsigned** when running the
workflow to put an unsigned package on the draft anyway. SmartScreen and Gatekeeper warn
about unsigned packages.

**Windows: Azure Trusted Signing over OIDC**, in the `windows-release` environment
(Settings → Environments), the same setup as Switchyard's. Variables:
`AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`,
`AZURE_CODESIGNING_ENDPOINT`, `AZURE_CODESIGNING_ACCOUNT`, `AZURE_CODESIGNING_PROFILE` and
`SWITCHSCRIBE_SIGN_EXPECTED_SUBJECT` (the certificate profile's subject, without quotes).
The app registration needs a federated credential for this repository's `windows-release`
environment. The exe, the DLLs that ship unsigned (sherpa-onnx, onnxruntime), the
uninstaller and the installer are signed, and `verify-signatures.ps1` checks them before
anything is uploaded.

**macOS: Developer ID and notarization**, as secrets in the `macos-release` environment:
`MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` (a Developer ID Application certificate),
`APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID` and `APPLE_API_KEY_P8_BASE64` (an App Store
Connect API key for `notarytool`). Without them the app is ad-hoc signed.

Linux AppImages are not signed.

## Building locally

All three scripts read the version from `Cargo.toml`, build with `--locked`, and write to
`dist/`. They set `GGML_NATIVE=OFF` so whisper.cpp is not tuned to the build machine's CPU
(see `docs/DECISIONS.md`).

**Linux** (the GPUI packages from CLAUDE.md, plus `libasound2-dev` and CMake):

```bash
packaging/linux/build-appimage.sh          # or packaging/package.sh
```

The AppImage carries sherpa-onnx's shared libraries next to the binary; the host provides
glibc, GPUI's libraries (xkbcommon, wayland, xcb, fontconfig, freetype, the Vulkan loader)
and ALSA. Build on the oldest distro to support (CI uses Ubuntu 22.04).

**macOS** (Xcode command line tools, CMake):

```bash
packaging/macos/build-macos.sh --arch universal   # .dmg and .pkg; --arch arm64 for a quick one
```

sherpa-onnx's dylibs go in `Contents/Frameworks`, merged with `lipo` when universal. Set
`MACOS_SIGN_IDENTITY` (and `MACOS_NOTARY_PROFILE`) to sign and notarize.

**Windows** (Rust, CMake, Visual Studio C++ build tools, [Inno Setup 6](https://jrsoftware.org/isinfo.php):
`winget install JRSoftware.InnoSetup`):

```powershell
./packaging/windows/build-windows.ps1     # -Sign for Azure Trusted Signing
```

The installer ships the exe, sherpa-onnx's DLLs and the Visual C++ runtime DLLs (app-local),
so a clean Windows needs no redistributable. Test a silent upgrade the way the app runs it:

```powershell
dist\switchscribe-setup-0.1.0-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS
```

## The Windows installer

`windows/switchscribe.iss` installs per user (no admin) into
`%LOCALAPPDATA%\Programs\switchscribe`: the exe and its DLLs, a Start menu shortcut, the
`switchscribe://` URL scheme and an optional "Start with Windows" entry. Uninstalling keeps
the data folder (`%APPDATA%\switchscribe`). Never change its `AppId`: Windows recognizes
upgrades and the uninstaller by it.

## Icons

`icons/switchscribe.svg` (48 px and up) and `icons/switchscribe-small.svg` (16-32 px) are
the sources; `python3 packaging/icons/generate.py` (needs `cairosvg` and `pillow`) renders
the checked-in PNG, ICO and ICNS files.
