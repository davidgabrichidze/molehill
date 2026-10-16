; Molehill installer (Inno Setup 6). Build the release binaries first:
;   cargo build --release
;   iscc installer\molehill.iss
; Output: installer\Output\molehill-setup-<version>.exe

#define AppName "Molehill"
#define AppExe "molehill.exe"
#define ServiceExe "molehill-service.exe"
#define Bin "..\target\release"
; The release workflow passes /DAppVersion=<tag version>; a local build falls
; back to the binary's version resource, so neither can drift from Cargo.toml.
#ifndef AppVersion
  #define AppVersion GetVersionNumbersString(Bin + "\" + AppExe)
#endif

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
; The id lets Windows show Molehill's notifications (update available) as Molehill's.
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; AppUserModelID: "Giorgobiani.Molehill"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; \
  ValueData: """{app}\{#AppExe}"" --hidden"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Description: "Open Molehill"; Flags: nowait postinstall skipifsilent
; An update started from the app (/UPDATE) runs silently; start Molehill again
; afterwards, as the signed-in user rather than as the elevated installer.
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: IsAppUpdate

[UninstallRun]
Filename: "{app}\{#ServiceExe}"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

; Service data (downloads, logs, run files). Profiles and saved passwords
; are per user and stay: %APPDATA%\Molehill and Credential Manager.
[UninstallDelete]
Type: filesandordirs; Name: "{commonappdata}\Molehill"

[Code]
function IsAppUpdate: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/UPDATE') = 0 then
      Result := True;
end;

// Stop the service so its binary can be replaced on upgrade, and close the
// app (it answers WM_ENDSESSION, which the Restart Manager sends).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code, I: Integer;
begin
  // 1060: no such service (fresh install), nothing to wait for.
  Exec(ExpandConstant('{sys}\sc.exe'), 'query Molehill', '', SW_HIDE, ewWaitUntilTerminated, Code);
  if Code <> 1060 then
  begin
    Exec(ExpandConstant('{sys}\sc.exe'), 'stop Molehill', '', SW_HIDE, ewWaitUntilTerminated, Code);
    for I := 1 to 60 do
    begin
      Exec(ExpandConstant('{cmd}'), '/c sc query Molehill | find "STOPPED" > nul', '', SW_HIDE, ewWaitUntilTerminated, Code);
      if Code = 0 then Break;
      Sleep(500);
    end;
  end;
  Result := '';
end;

// The service must be set up for anything to work; say so if it isn't.
procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if CurStep = ssPostInstall then
  begin
    if not Exec(ExpandConstant('{app}\{#ServiceExe}'), 'install', '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
      MsgBox('The Molehill service could not be installed (code ' + IntToStr(Code) + '). ' +
             'Run "molehill-service.exe install" from an elevated terminal, or reinstall.', mbError, MB_OK);
  end;
end;
