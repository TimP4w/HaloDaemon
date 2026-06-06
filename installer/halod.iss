; Inno Setup script for HaloDaemon — Windows installer.
;
; Builds a single halod-setup-x64.exe that:
;   * installs halod.exe, halod-gui.exe and the bundled GTK 4
;     runtime into {pf}\HaloDaemon,
;   * registers halod as an auto-starting service (the supervisor;
;     see src/daemon/src/service/mod.rs),
;   * adds Start Menu shortcuts and an optional sign-in entry for the tray.
;
; The GTK runtime + resources are NOT in the repo: run installer\collect-gtk.ps1
; first to populate installer\staging\, which this script packages verbatim.
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
DefaultDirName={autopf}\HaloD
DefaultGroupName=HaloDaemon
DisableProgramGroupPage=yes
LicenseFile=LICENSE.txt
; Disclaimer shown to the user before installation proceeds.
InfoBeforeFile=DISCLAIMER.txt
OutputDir=Output
OutputBaseFilename=halod-setup-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The service must run elevated (chipset SMBus via PawnIO).
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
Name: "traystartup"; Description: "Start HaloDaemon in the background when I sign in"; GroupDescription: "Startup:"

[Files]
; installer\staging\ is produced by collect-gtk.ps1 — the two exes, the GTK 4
; runtime DLL tree + resources, the PawnIO blobs and style.css.
Source: "staging\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
; One user-facing entry. Launches the UI directly.
Name: "{group}\HaloDaemon"; Filename: "{app}\halod-gui.exe"
Name: "{group}\Uninstall HaloDaemon"; Filename: "{uninstallexe}"
Name: "{autodesktop}\HaloDaemon"; Filename: "{app}\halod-gui.exe"; Tasks: desktopicon
; Sign-in autostart: start hidden in the background (tray icon only, no window).
; Per-machine install, so it autostarts for every user via commonstartup.
Name: "{commonstartup}\HaloDaemon"; Filename: "{app}\halod-gui.exe"; Parameters: "--background"; Tasks: traystartup

[Run]
; Register and start the supervisor service (idempotent — safe on upgrades).
Filename: "{app}\halod.exe"; Parameters: "--install-service"; \
  StatusMsg: "Registering the HaloDaemon service..."; Flags: runhidden waituntilterminated
; One post-install launch: open the UI.
Filename: "{app}\halod-gui.exe"; Description: "Launch HaloDaemon"; \
  Flags: postinstall skipifsilent nowait

[UninstallRun]
; Stop and remove the service before the files are deleted.
Filename: "{app}\halod.exe"; Parameters: "--uninstall-service"; \
  RunOnceId: "UninstallHalodService"; Flags: runhidden waituntilterminated

[Code]
{ On an upgrade the running supervisor service keeps halod.exe
  locked. Stop it before files are copied; failure (e.g. a first install where
  the service does not exist yet) is harmless and ignored. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop HalodDaemon', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  { The supervisor polls every ~2 s; give it time to terminate the worker and
    release the executable before the copy step. }
  Sleep(5000);
end;
