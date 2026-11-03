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
; Relaunch the instance Setup closed. Installs without a running instance
; keep the default-checked "Launch SysVitals" entry on the Finished page.
Filename: "{app}\SysVitals.exe"; WorkingDir: "{app}"; Flags: nowait; Check: RelaunchAfterUpgrade
Filename: "{app}\SysVitals.exe"; Description: "{cm:LaunchProgram,SysVitals}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent; Check: OfferLaunchOnFinish

[Code]
var
  WasRunning: Boolean;

function NormalizeVersion(const V: String): String;
var
  P: Integer;
begin
  Result := Trim(V);
  if (Length(Result) > 0) and ((Result[1] = 'v') or (Result[1] = 'V')) then
    Result := Copy(Result, 2, Length(Result));
  P := Pos('-', Result);
  if P > 0 then
    Result := Copy(Result, 1, P - 1);
  P := Pos('+', Result);
  if P > 0 then
    Result := Copy(Result, 1, P - 1);
end;

function TakeVersionPart(var S: String): Integer;
var
  P: Integer;
begin
  P := Pos('.', S);
  if P > 0 then
  begin
    Result := StrToIntDef(Copy(S, 1, P - 1), 0);
    Delete(S, 1, P);
  end
  else
  begin
    Result := StrToIntDef(S, 0);
    S := '';
  end;
end;

function CompareVersions(const A, B: String): Integer;
var
  SA, SB: String;
  I, PA, PB: Integer;
begin
  SA := NormalizeVersion(A);
  SB := NormalizeVersion(B);
  Result := 0;
  for I := 1 to 4 do
  begin
    PA := TakeVersionPart(SA);
    PB := TakeVersionPart(SB);
    if PA < PB then
    begin
      Result := -1;
      Exit;
    end;
    if PA > PB then
    begin
      Result := 1;
      Exit;
    end;
  end;
end;

function GetInstalledVersion(var Version: String): Boolean;
var
  Key, InstallLocation, ExePath, FileVersion: String;
begin
  Result := False;
  Version := '';
  Key := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{7E2F6C1A-4B3D-4F8E-9A21-6C5D8B7E9F10}_is1';
  if RegQueryStringValue(HKCU, Key, 'DisplayVersion', Version) and
     (Version <> '') then
    Result := True;

  if RegQueryStringValue(HKCU, Key, 'InstallLocation', InstallLocation) and
     (InstallLocation <> '') then
  begin
    ExePath := AddBackslash(InstallLocation) + 'SysVitals.exe';
    if FileExists(ExePath) then
    begin
      { The executable's fixed version is the most direct record of what
        will actually be replaced; keep the higher of the two values. }
      if GetVersionNumbersString(ExePath, FileVersion) and
         ((not Result) or (CompareVersions(FileVersion, Version) > 0)) then
      begin
        Version := FileVersion;
        Result := True;
      end;
    end
    else if Result then
    begin
      { Orphan uninstall record: do not let it block a fresh install. }
      Version := '';
      Result := False;
    end;
  end;
end;

function InitializeSetup(): Boolean;
var
  InstalledVersion: String;
begin
  Result := True;
  WasRunning := CheckForMutexes('Local\SysVitals_SingleInstance');

  if GetInstalledVersion(InstalledVersion) then
  begin
    if CompareVersions(InstalledVersion, '{#AppVersion}') > 0 then
    begin
      Log('Downgrade blocked: installed=' + InstalledVersion +
        ', installer={#AppVersion}');
      SuppressibleMsgBox(
        'A newer version of SysVitals (' + InstalledVersion +
        ') is already installed.' + Chr(13) + Chr(10) + Chr(13) + Chr(10) +
        'This installer is version {#AppVersion}. Downgrading is not supported.' +
        Chr(13) + Chr(10) + Chr(13) + Chr(10) +
        'Uninstall the newer version first if you need to install this one.',
        mbError, MB_OK, IDOK);
      Result := False;
      Exit;
    end;
  end
  else if WasRunning then
  begin
    Log('SysVitals is running but no registered installation was found.');
    SuppressibleMsgBox(
      'SysVitals is currently running, but Setup could not find a registered installation.' +
      Chr(13) + Chr(10) + Chr(13) + Chr(10) +
      'Please close all SysVitals instances and run Setup again.',
      mbError, MB_OK, IDOK);
    Result := False;
    Exit;
  end;
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  while CheckForMutexes('Local\SysVitals_SingleInstance') do
  begin
    if SuppressibleMsgBox(
      'SysVitals is currently running. Please close all instances and click OK to continue, or Cancel to exit.',
      mbError, MB_OKCANCEL, IDCANCEL) <> IDOK then
    begin
      Result := False;
      Exit;
    end;
  end;
end;

function RelaunchAfterUpgrade(): Boolean;
begin
  Result := WasRunning;
end;

function OfferLaunchOnFinish(): Boolean;
begin
  Result := not WasRunning;
end;

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
