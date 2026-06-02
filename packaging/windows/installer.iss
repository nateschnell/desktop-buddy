; Inno Setup script for Claude Buddy — one downloadable Setup.exe that installs
; the GUI + the daemon + bundled firmware, drops a Start Menu shortcut, and
; launches the app. On first run the app registers the daemon as a per-user
; logon Scheduled Task (setup.rs::install_daemon_service) and wires the Claude
; Code hooks — so the user only ever runs this one installer.
;
; Both binaries install side-by-side because the GUI locates the daemon as its
; sibling (setup.rs::daemon_exe_path); the app then copies the daemon to a
; stable per-user location for the service, insulating it from upgrades.
;
; Compiled in CI with:
;   iscc /DAppVersion=<ver> /DStageDir=<dir-with-binaries+firmware> installer.iss
; where StageDir holds claude-buddy.exe, claude-buddy-app.exe, and firmware*.*
;
; Signing (deferred): when an Authenticode cert exists, add SignTool here and a
; signing step in CI — the layout is otherwise unchanged.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef StageDir
  #define StageDir "stage"
#endif

[Setup]
AppId={{B4D8F2A1-3C7E-4E2B-9A6F-CLAUDEBUDDY01}
AppName=Claude Buddy
AppVersion={#AppVersion}
AppPublisher=Anthropic
DefaultDirName={autopf}\Claude Buddy
DefaultGroupName=Claude Buddy
DisableProgramGroupPage=yes
; Per-user install — no elevation, matches the per-user daemon/service model.
PrivilegesRequired=lowest
OutputDir=.
OutputBaseFilename=Claude-Buddy-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "{#StageDir}\claude-buddy-app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\claude-buddy.exe";     DestDir: "{app}"; Flags: ignoreversion
; Every board's firmware image + version (and the legacy firmware.* alias) so the
; app's one-click OTA has an image for whichever board connects.
Source: "{#StageDir}\firmware*.bin";        DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\firmware*.version";     DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\Claude Buddy";               Filename: "{app}\claude-buddy-app.exe"
Name: "{userdesktop}\Claude Buddy";         Filename: "{app}\claude-buddy-app.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Run]
; Launch the app right after install so the user lands in the setup UI.
Filename: "{app}\claude-buddy-app.exe"; Description: "Launch Claude Buddy"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Tear down the logon task the app registered (best-effort; ignore if absent).
Filename: "{cmd}"; Parameters: "/c schtasks /Delete /F /TN ClaudeBuddy"; Flags: runhidden; RunOnceId: "DelTask"
