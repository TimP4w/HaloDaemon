; Inno Setup script for HaloDaemon — Windows installer.
;
; Builds a single halod-setup-x64.exe that:
;   * installs halod.exe, halod-gui.exe, halod-broker.exe and the PawnIO blobs into {pf}\HaloDaemon,
;   * registers halod-broker.exe as a demand-start LocalSystem service (the only
;     elevated component; see src/broker/src/service.rs and
;     docs/windows-privilege-separation.md),
;   * adds Start Menu shortcuts and an optional sign-in entry for the tray.
;
; Run packaging\windows\stage-release.ps1 first to populate packaging\windows\staging\,
; which this script packages verbatim.
;
; The version is supplied by CI:  ISCC /DAppVersion=<tag-without-v> halod.iss

#define AppName "HaloDaemon"
#define AppPublisher "TimP4w"
#define AppURL "https://github.com/TimP4w/HaloDaemon"

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif

[Setup]
; A stable AppId so upgrades replace the previous install in place.
AppId={{9370d7c0-5d3c-44c9-ab3d-593c509f5299}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\HaloDaemon
DefaultGroupName=HaloDaemon
DisableProgramGroupPage=yes
LicenseFile=LICENSE.txt
; Disclaimer shown to the user before installation proceeds.
InfoBeforeFile=DISCLAIMER.txt
OutputDir=Output
OutputBaseFilename=halod-setup-x64
; The app icon: shown on the setup wizard and (via the installed exe) in
; Add/Remove Programs. Path is relative to this .iss (packaging\windows\).
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\halod-gui.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Admin is needed only to register the LocalSystem broker service at install
; time. At runtime only halod-broker.exe is elevated (on demand); halod.exe and
; the GUI run unprivileged — see docs/windows-privilege-separation.md.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
UninstallDisplayName={#AppName} {#AppVersion}
; Let Setup offer to close halod-gui if it locks files.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut for HaloDaemon"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
; packaging\windows\staging\ is produced by stage-release.ps1 — the three exes
; (halod, halod-gui, halod-broker), bundled ffmpeg (+ its DLLs and license), and
; the PawnIO blobs.
Source: "staging\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
; One user-facing entry. Launches the UI directly.
Name: "{group}\HaloDaemon"; Filename: "{app}\halod-gui.exe"
Name: "{group}\Uninstall HaloDaemon"; Filename: "{uninstallexe}"
Name: "{autodesktop}\HaloDaemon"; Filename: "{app}\halod-gui.exe"; Tasks: desktopicon
; Sign-in autostart is owned by the app itself now (Settings' "Start on boot"
; toggle writes an HKCU Run value — see ui/src/lifecycle/autostart/windows.rs).
; No installer-created shortcut: an elevated installer writing a per-machine
; {commonstartup} shortcut can't set the signed-in user's HKCU value anyway,
; and having both an installer task and an in-app toggle is two sources of
; truth for the same setting. The user enables it from Settings after first run.

[Registry]
; Uninstall-only cleanup for the toggle's HKCU Run value: `uninsdeletevalue`
; (without `deletevalue`) means this entry is never touched at install/upgrade
; time — the user's "Start on boot" setting survives upgrades — and is only
; removed when HaloDaemon itself is uninstalled. A missing value is not an error.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
  ValueName: "HaloDaemon"; ValueType: none; \
  Flags: uninsdeletevalue dontcreatekey

[Run]
; Register the on-demand elevated broker service (idempotent — safe on upgrades).
; The unprivileged daemon (launched by the GUI) starts it on demand; nothing is
; auto-started here. The privileged binary is halod-broker.exe — halod.exe is
; never a service and never elevated.
Filename: "{app}\halod-broker.exe"; Parameters: "--install-service"; \
  StatusMsg: "Registering the HaloDaemon broker service..."; Flags: runhidden waituntilterminated
; One post-install launch: open the UI (which starts the user-level daemon).
Filename: "{app}\halod-gui.exe"; Description: "Launch HaloDaemon"; \
  Flags: postinstall skipifsilent nowait

[UninstallRun]
; Stop and remove the broker service before the files are deleted.
Filename: "{app}\halod-broker.exe"; Parameters: "--uninstall-service"; \
  RunOnceId: "UninstallHalodBrokerService"; Flags: runhidden waituntilterminated

[Code]
{ Before copying files, stop everything holding the executables:
  - the new broker service (HalodBroker) if this is a reinstall/upgrade,
  - the OLD supervisor service (HalodDaemon) from pre-split installs, which is
    now obsolete and points at a role halod.exe no longer has — delete it,
  - the user-level worker/broker processes.
  All failures (e.g. a first install) are harmless and ignored. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop HalodBroker', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  { Obsolete pre-split service — stop and remove it. }
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop HalodDaemon', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\sc.exe'), 'delete HalodDaemon', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  { The user-level daemon/broker processes may still hold their exes. }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im halod.exe', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im halod-broker.exe', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  { Give the SCM / processes a moment to release the executables. }
  Sleep(3000);
end;
