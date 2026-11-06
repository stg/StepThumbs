#define MyAppName "StepThumbs"
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef BuildId
  #error BuildId is required
#endif
#ifndef BinaryDir
  #define BinaryDir "..\publish"
#endif
#ifndef LicenseDir
  #define LicenseDir "..\publish\licenses"
#endif
[Setup]
AppId={{A8B827E1-90E3-49A1-80B6-CB3C7BDE1821}
AppName={#MyAppName}
AppVersion={#AppVersion}
DefaultDirName={localappdata}\StepThumbnail\app-rust\{#AppVersion}-{#BuildId}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DisableProgramGroupPage=yes
DisableDirPage=yes
ChangesAssociations=yes
UsePreviousAppDir=no
OutputDir=..\dist
OutputBaseFilename=StepThumbs-{#AppVersion}-Setup
Compression=lzma2
SolidCompression=yes
UninstallDisplayIcon={app}\StepThumbs.exe
SetupIconFile=..\rust\app\assets\app.ico
[Tasks]
Name: "autostart"; Description: "Start StepThumbs when I sign in"; Flags: checkedonce
[Files]
Source: "{#BinaryDir}\StepThumbs.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinaryDir}\StepThumbsEngine.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinaryDir}\StepThumbsShell.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LicenseDir}\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs
[Icons]
Name: "{autoprograms}\StepThumbs"; Filename: "{app}\StepThumbs.exe"; Parameters: "--background"
[Registry]
Root: HKCU; Subkey: "Software\Classes\CLSID\{{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}"; ValueType: string; ValueName: ""; ValueData: "StepThumbs thumbnail provider"
Root: HKCU; Subkey: "Software\Classes\CLSID\{{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}"; ValueType: dword; ValueName: "DisableProcessIsolation"; ValueData: "1"
Root: HKCU; Subkey: "Software\Classes\CLSID\{{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}\InprocServer32"; ValueType: string; ValueName: ""; ValueData: "{app}\StepThumbsShell.dll"
Root: HKCU; Subkey: "Software\Classes\CLSID\{{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}\InprocServer32"; ValueType: string; ValueName: "ThreadingModel"; ValueData: "Apartment"
[Run]
Filename: "{app}\StepThumbs.exe"; Parameters: "--background"; Description: "Start StepThumbs in the system tray"; Flags: nowait postinstall skipifsilent
[Code]
const RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
const RunName = 'StepThumbnail Preflight';
const ClassKey = 'Software\Classes\CLSID\{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}';
procedure CurStepChanged(CurStep: TSetupStep);
var Existing: String; ResultCode: Integer;
begin
  if CurStep = ssPostInstall then begin
    Exec(ExpandConstant('{app}\StepThumbs.exe'), '--shutdown', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if WizardIsTaskSelected('autostart') then
      RegWriteStringValue(HKCU, RunKey, RunName, '"' + ExpandConstant('{app}\StepThumbs.exe') + '" --background')
    else if RegQueryStringValue(HKCU, RunKey, RunName, Existing) then begin
      if Pos(Lowercase(ExpandConstant('{localappdata}\StepThumbnail\')), Lowercase(Existing)) > 0 then
        RegDeleteValue(HKCU, RunKey, RunName);
    end;
  end;
end;
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var Existing: String; ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then begin
    Exec(ExpandConstant('{app}\StepThumbs.exe'), '--shutdown', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if RegQueryStringValue(HKCU, ClassKey + '\InprocServer32', '', Existing) and
       (CompareText(Existing, ExpandConstant('{app}\StepThumbsShell.dll')) = 0) then begin
      if not Exec(ExpandConstant('{app}\StepThumbs.exe'), '--restore-hooks', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then begin
        MsgBox('The previous thumbnail providers could not be restored. Uninstall was cancelled so StepThumbs can keep handling these files.', mbError, MB_OK);
        Abort;
      end;
      RegDeleteKeyIncludingSubkeys(HKCU, ClassKey);
    end;
    if RegQueryStringValue(HKCU, RunKey, RunName, Existing) and
       (CompareText(Existing, '"' + ExpandConstant('{app}\StepThumbs.exe') + '" --background') = 0) then
      RegDeleteValue(HKCU, RunKey, RunName);
  end;
end;
