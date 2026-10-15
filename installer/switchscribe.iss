; switchscribe installer (Inno Setup 6).
;
; Built by .github/workflows/release.yml (see installer/README.md):
;   ISCC.exe /DAppVersion=0.2.0 /DSourceDir=dist /O"out" installer\switchscribe.iss
; produces out\switchscribe-setup-0.2.0-x64.exe. The update checker
; (crates/integrations/src/update.rs) looks for exactly that asset name and runs it with
; /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS (INSTALLER_ARGS there).
; A relative SourceDir resolves against this script's folder.
;
; Per-user install: no admin rights and no UAC prompt, so silent self-updates work on
; locked-down work laptops. Everything lives under {localappdata}\Programs\switchscribe
; and HKCU.
;
; The uninstaller removes only what this script installed. The user's data folder
; (%APPDATA%\switchscribe: recordings, transcripts, voiceprints, models, settings) is
; deliberately NOT deleted: uninstalling or reinstalling must never lose meetings. There
; is no [UninstallDelete] entry for it on purpose; users delete it by hand.

#ifndef AppVersion
  #error Pass the version: ISCC /DAppVersion=X.Y.Z
#endif
#ifndef SourceDir
  #define SourceDir "..\dist"
#endif

#define AppName "switchscribe"
#define AppExe "switchscribe.exe"
#define AppPublisher "Jeremie Lumandong"
#define AppUrl "https://github.com/jeremielumandong/switchscribe"

[Setup]
; Stable forever: Windows matches upgrades and the uninstaller by this id. Never change it.
AppId={{131DB58A-DD2D-4E68-BCC8-ECAE8AC02A80}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UsePreviousAppDir=yes
UsePreviousTasks=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1809: process loopback capture needs 2004+, but the app explains that itself.
MinVersion=10.0.17763
OutputBaseFilename={#AppName}-setup-{#AppVersion}-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
; Close a running switchscribe (Restart Manager) before replacing its files. The app's
; single instance is a loopback port, not a mutex, so there is no AppMutex.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "Start {#AppName} with Windows (in the tray)"; GroupDescription: "Other:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
; sherpa-onnx (speaker identification) ships as shared libraries that must sit next to
; the exe, along with any other DLL the build put there.
Source: "{#SourceDir}\*.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"

[Registry]
; Start with Windows: the app treats --background as "start in the tray, no window".
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"" --background"; Tasks: startup; Flags: uninsdeletevalue
; switchscribe:// links (the app also re-registers them at each start, so a moved exe
; keeps working). The uninstaller removes the whole key.
Root: HKCU; Subkey: "Software\Classes\switchscribe"; ValueType: string; ValueName: ""; ValueData: "URL:switchscribe"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\switchscribe"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\switchscribe\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"",0"
Root: HKCU; Subkey: "Software\Classes\switchscribe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""

[Run]
; An interactive install offers to launch the app.
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
; A silent install is the in-app updater, which quit to let this run: start the new
; version again.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: WizardSilent
