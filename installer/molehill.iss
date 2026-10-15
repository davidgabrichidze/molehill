; Molehill installer (Inno Setup 6). Build the release binaries first:
;   cargo build --release
;   iscc installer\molehill.iss
; Output: installer\Output\molehill-setup-<version>.exe

#define AppName "Molehill"
#define AppVersion "0.1.0"
#define AppExe "molehill.exe"
#define ServiceExe "molehill-service.exe"
#define Bin "..\target\release"

[Setup]
AppId={{6F3C2B1E-8D4A-4E59-9C1B-4D2E7A9F0B3C}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Shota Giorgobiani
AppPublisherURL=https://github.com/giorgobiani/molehill
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputBaseFilename=molehill-setup-{#AppVersion}
SetupIconFile=molehill.ico
UninstallDisplayIcon={app}\{#AppExe}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
CloseApplications=yes

[Tasks]
Name: "autostart"; Description: "Start Molehill when I sign in to Windows"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "{#Bin}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Bin}\{#ServiceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; \
  ValueData: """{app}\{#AppExe}"" --hidden"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#ServiceExe}"; Parameters: "install"; Flags: runhidden waituntilterminated; StatusMsg: "Setting up the Molehill service..."
Filename: "{app}\{#AppExe}"; Description: "Open Molehill"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\{#ServiceExe}"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[Code]
// Stop the service so its binary can be replaced on upgrade.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop Molehill', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
  Result := '';
end;
