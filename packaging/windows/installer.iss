; Inno Setup script for Agent Buddy — one downloadable Setup.exe that installs
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
; where StageDir holds agent-buddy.exe, agent-buddy-app.exe, and firmware*.*
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
AppId={{B4D8F2A1-3C7E-4E2B-9A6F-AGENTBUDDY01}
AppName=Agent Buddy
AppVersion={#AppVersion}
AppPublisher=nateschnell
DefaultDirName={autopf}\Agent Buddy
DefaultGroupName=Agent Buddy
DisableProgramGroupPage=yes
; Per-user install — no elevation, matches the per-user daemon/service model.
PrivilegesRequired=lowest
OutputDir=.
OutputBaseFilename=Agent-Buddy-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "{#StageDir}\agent-buddy-app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\agent-buddy.exe";     DestDir: "{app}"; Flags: ignoreversion
; Every board's firmware image + version (and the legacy firmware.* alias) so the
; app's one-click OTA has an image for whichever board connects.
Source: "{#StageDir}\firmware*.bin";        DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\firmware*.version";     DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; Bundled-asset license notice (Lucide/Feather icon font, ISC + MIT). Stage
; bridge\assets\LICENSE into StageDir as THIRD_PARTY_LICENSES alongside the
; binaries so the required notice ships with the install.
Source: "{#StageDir}\THIRD_PARTY_LICENSES"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\Agent Buddy";               Filename: "{app}\agent-buddy-app.exe"
Name: "{userdesktop}\Agent Buddy";         Filename: "{app}\agent-buddy-app.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Run]
; Launch the app right after install so the user lands in the setup UI.
Filename: "{app}\agent-buddy-app.exe"; Description: "Launch Agent Buddy"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Full teardown via the daemon binary: removes the Claude Code hooks, the
; installed daemon + its logon task, the app login task, the launcher, and the
; per-user state. Runs before Inno removes {app}. Best-effort (ignore failure).
Filename: "{app}\agent-buddy.exe"; Parameters: "uninstall"; Flags: runhidden; RunOnceId: "AgentBuddyUninstall"
; Backstops: ensure both scheduled tasks are gone even if the call above
; couldn't run (e.g. a damaged install).
Filename: "{cmd}"; Parameters: "/c schtasks /Delete /F /TN AgentBuddy"; Flags: runhidden; RunOnceId: "DelTask"
Filename: "{cmd}"; Parameters: "/c schtasks /Delete /F /TN AgentBuddyApp"; Flags: runhidden; RunOnceId: "DelAppTask"
