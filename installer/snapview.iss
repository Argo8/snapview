; snapview installer — Inno Setup script
; Build: ISCC.exe installer\snapview.iss
; Sign:  set SIGNTOOL to a signtool.exe command with cert/timestamp args before running ISCC.

#define MyAppName "snapview"
#define MyAppVersion "1.0.0"
#define MyAppPublisher "Filip Kozina"
#define MyAppExeName "snapview.exe"
#define MyAppId "{{B7F4C3E0-5E18-4D2D-9A4B-1A2F1E8A9D11}"
#define MyAppProgId "snapview.image"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppCopyright=Copyright (C) 2026 {#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
Compression=lzma2/ultra
SolidCompression=yes
OutputBaseFilename=snapview-setup-{#MyAppVersion}
OutputDir=..\dist
WizardStyle=modern
SetupIconFile=..\assets\icon.ico
; Signing — set SIGNTOOL env var before running ISCC for signed builds.
; Example: set SIGNTOOL=signtool.exe sign /f cert.pfx /p PASSWORD /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $f
SignTool=signtool
SignedUninstaller=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"
Name: "openwith"; Description: "Add snapview to the Windows ""Open with"" menu for images"; GroupDescription: "Integration:"
Name: "defaultapps"; Description: "Register snapview so it can be set as the default photo viewer"; GroupDescription: "Integration:"; Flags: checkedonce

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\icon.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion isreadme

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

; ---------- Registry: Open with + Default Apps ----------
; See: https://learn.microsoft.com/windows/win32/shell/default-programs
;
; 1. Application registration (HKLM\Software\Classes\Applications\snapview.exe)
;    Makes snapview appear in the "Open with" submenu for supported types.
; 2. ProgID (HKLM\Software\Classes\snapview.image)
;    Allows snapview to be associated as the default handler.
; 3. RegisteredApplications + Capabilities
;    Makes snapview appear in Settings → Apps → Default apps.

[Registry]
; --- Application registration ---
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpg"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpeg"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".png"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".bmp"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".gif"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".webp"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tif"; ValueData: ""; Tasks: openwith
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tiff"; ValueData: ""; Tasks: openwith

; --- "Open with" hint for each extension (OpenWithProgids) ---
Root: HKLM; Subkey: "Software\Classes\.jpg\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.jpeg\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.png\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.bmp\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.gif\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.webp\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.tif\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue
Root: HKLM; Subkey: "Software\Classes\.tiff\OpenWithProgids"; ValueType: string; ValueName: "{#MyAppProgId}"; ValueData: ""; Tasks: openwith; Flags: uninsdeletevalue

; --- ProgID for Default Apps ---
Root: HKLM; Subkey: "Software\Classes\{#MyAppProgId}"; ValueType: string; ValueData: "snapview image"; Flags: uninsdeletekey; Tasks: defaultapps
Root: HKLM; Subkey: "Software\Classes\{#MyAppProgId}\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\Classes\{#MyAppProgId}\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: defaultapps

; --- Capabilities (Default Apps in Settings) ---
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Fast, minimal image viewer"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpg"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpeg"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".bmp"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gif"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".webp"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tif"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tiff"; ValueData: "{#MyAppProgId}"; Tasks: defaultapps
Root: HKLM; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "Software\{#MyAppName}\Capabilities"; Flags: uninsdeletevalue; Tasks: defaultapps
