; Inno Setup: per-user install, no admin rights.
; Build: iscc /DAppVersion=0.1.0 installer\lights-out.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6B1F6C2E-3B0E-4E52-9C3E-1A7F5D0C9A11}
AppName=LightsOut
AppVersion={#AppVersion}
AppPublisher=LightsOut
DefaultDirName={autopf}\LightsOut
DefaultGroupName=LightsOut
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=LightsOut-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\lights-out.exe

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[CustomMessages]
en.AutostartTask=Start with Windows
ru.AutostartTask=Запускать при входе в Windows
en.DesktopIconTask=Create a desktop shortcut
ru.DesktopIconTask=Ярлык на рабочем столе
en.RunApp=Launch LightsOut
ru.RunApp=Запустить LightsOut

[Tasks]
Name: "autostart"; Description: "{cm:AutostartTask}"; Flags: checkedonce
Name: "desktopicon"; Description: "{cm:DesktopIconTask}"; Flags: unchecked

[Files]
Source: "..\target\release\lights-out.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\LightsOut"; Filename: "{app}\lights-out.exe"
Name: "{autodesktop}\LightsOut"; Filename: "{app}\lights-out.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lights-out.exe"; Description: "{cm:RunApp}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM lights-out.exe"; Flags: runhidden; RunOnceId: "KillApp"

[Code]
// The app syncs autostart with its config itself (autostart = true by default).
// If the box is unchecked, write a config with autostart = false so the app does not re-add the entry.
procedure CurStepChanged(CurStep: TSetupStep);
var
  CfgDir, CfgFile: String;
begin
  if (CurStep = ssPostInstall) and (not WizardIsTaskSelected('autostart')) then
  begin
    CfgDir := ExpandConstant('{userappdata}\LightsOut\config');
    CfgFile := CfgDir + '\config.toml';
    if not FileExists(CfgFile) then
    begin
      ForceDirectories(CfgDir);
      SaveStringToFile(CfgFile, 'autostart = false' + #13#10, False);
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'LightsOut');
end;
