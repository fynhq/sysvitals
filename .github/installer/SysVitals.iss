; Per-user SysVitals installer.
;
; CI supplies these defines:
;   AppVersion, VersionInfoVersion, Arch, ArchAllowed, ArchMode,
;   SourceDir, OutputDir, OutputBaseFilename, RepoRoot

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#ifndef VersionInfoVersion
  #define VersionInfoVersion "0.0.0.0"
#endif

#ifndef Arch
  #define Arch "x64"
#endif

#ifndef ArchAllowed
  #define ArchAllowed "x64os"
#endif

#ifndef ArchMode
  #define ArchMode "x64os"
#endif

#ifndef SourceDir
  #define SourceDir "SysVitals"
#endif

#ifndef OutputDir
  #define OutputDir "."
#endif

#ifndef OutputBaseFilename
  #define OutputBaseFilename "SysVitals_Windows_" + Arch + "_setup"
#endif

#ifndef RepoRoot
  #define RepoRoot "."
#endif

[Setup]
AppId={{7E2F6C1A-4B3D-4F8E-9A21-6C5D8B7E9F10}
AppName=SysVitals
AppVersion={#AppVersion}
AppVerName=SysVitals {#AppVersion}
AppPublisher=SysVitals
AppPublisherURL=https://github.com/fynhq/sysvitals
AppSupportURL=https://github.com/fynhq/sysvitals
AppUpdatesURL=https://github.com/fynhq/sysvitals/releases
AppComments=https://github.com/fynhq/sysvitals
AppContact=https://github.com/fynhq/sysvitals
AppCopyright=Copyright (c) 2026 SysVitals
DefaultDirName={localappdata}\Programs\SysVitals
DefaultGroupName=SysVitals
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseFilename}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchMode}
UninstallDisplayIcon={app}\SysVitals.exe
LicenseFile={#SourceDir}\LICENSE
CloseApplications=yes
RestartApplications=no
AppMutex=Local\SysVitals_SingleInstance
SetupIconFile={#RepoRoot}\crates\app-win\assets\icon.ico
VersionInfoVersion={#VersionInfoVersion}
VersionInfoDescription=SysVitals Setup
VersionInfoProductName=SysVitals

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\SysVitals.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\SysVitalsSettings.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\SysVitalsHistory.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\SysVitals"; Filename: "{app}\SysVitals.exe"; WorkingDir: "{app}"
Name: "{userdesktop}\SysVitals"; Filename: "{app}\SysVitals.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\SysVitals.exe"; Description: "{cm:LaunchProgram,SysVitals}"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  RunValue: String;
  ExpectedQuoted: String;
  ExpectedPlain: String;
begin
  if CurUninstallStep <> usUninstall then
    Exit;

  if not RegQueryStringValue(HKCU,
    'Software\Microsoft\Windows\CurrentVersion\Run',
    'SysVitals', RunValue) then
    Exit;

  ExpectedQuoted := '"' + ExpandConstant('{app}\SysVitals.exe') + '"';
  ExpectedPlain := ExpandConstant('{app}\SysVitals.exe');
  if (CompareText(RunValue, ExpectedQuoted) = 0) or
     (CompareText(RunValue, ExpectedPlain) = 0) then
    RegDeleteValue(HKCU,
      'Software\Microsoft\Windows\CurrentVersion\Run',
      'SysVitals');
end;
