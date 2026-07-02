; Inno Setup script for MassFckinMailer (Windows installer).
; Built in CI by ISCC; overridable values are passed with /D defines:
;   ISCC /DMyAppVersion=0.1.0 /DOutputBase=massfckinmailer-windows-x86_64-setup installer\windows\massfckinmailer.iss
; Relative paths below resolve against this script's directory.

#define MyAppName "MassFckinMailer"
#define MyAppPublisher "Emre Beşirik"
#define MyAppURL "https://github.com/ebesirik/MassFckinMailer"
#define MyAppExeName "massfckinmailer.exe"

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\massfckinmailer.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef OutputBase
  #define OutputBase "massfckinmailer-setup"
#endif
#ifndef AppIcon
  #define AppIcon "..\..\assets\icon.ico"
#endif

[Setup]
; A stable AppId keeps upgrades/uninstall consistent across versions.
AppId={{7B4E9C2A-1F3D-4A6B-9E8C-2D5F0A1B3C4D}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
DisableProgramGroupPage=yes
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Let the auto-updater upgrade a running instance: close it (Restart Manager)
; so its locked .exe can be replaced, then the [Run] entry relaunches it.
CloseApplications=yes
RestartApplications=no
SetupIconFile={#AppIcon}
LicenseFile=..\..\LICENSE-MIT
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBase}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; runasoriginaluser: relaunch as the user (not elevated). No skipifsilent, so a
; silent auto-update relaunches the app after upgrading.
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall runasoriginaluser
