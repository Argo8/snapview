; snapview installer — Inno Setup script
; Build: ISCC.exe installer\snapview.iss
; Sign:  set SIGNTOOL to a signtool.exe command with cert/timestamp args before running ISCC.

#define MyAppName "snapview"
#define MyAppVersion "1.0.0"
#define MyAppPublisher "Filip Kozina"
#define MyAppExeName "snapview.exe"
#define MyAppId "{{B7F4C3E0-5E18-4D2D-9A4B-1A2F1E8A9D11}"
#define MyVendorKey "Software\Filip Kozina\snapview"

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

; ---------- Registry: Open With + Default Apps (always installed) ----------
; Reference: https://learn.microsoft.com/windows/win32/shell/default-programs
;
; Windows 11's Default Apps UI is per-extension, so we register a *distinct*
; ProgID for every supported type (snapview.jpg, snapview.png, ...) and map
; each ProgID back to the same shell\open\command. Capabilities then maps
; one extension to one ProgID. Doing this lets the user pick snapview as
; default for .jpg without also forcing it on .tiff.

[Registry]
; --- Application registration (HKLM\...\Applications\snapview.exe) ---
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpg"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpeg"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".png"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".bmp"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".gif"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".webp"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tif"; ValueData: ""
Root: HKLM; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tiff"; ValueData: ""

; --- Per-extension ProgIDs ---
; .jpg
Root: HKLM; Subkey: "Software\Classes\snapview.jpg"; ValueType: string; ValueData: "JPEG image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.jpg"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "JPEG image"
Root: HKLM; Subkey: "Software\Classes\snapview.jpg\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.jpg\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.jpg\OpenWithProgids"; ValueType: string; ValueName: "snapview.jpg"; ValueData: ""; Flags: uninsdeletevalue
; .jpeg
Root: HKLM; Subkey: "Software\Classes\snapview.jpeg"; ValueType: string; ValueData: "JPEG image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.jpeg"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "JPEG image"
Root: HKLM; Subkey: "Software\Classes\snapview.jpeg\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.jpeg\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.jpeg\OpenWithProgids"; ValueType: string; ValueName: "snapview.jpeg"; ValueData: ""; Flags: uninsdeletevalue
; .png
Root: HKLM; Subkey: "Software\Classes\snapview.png"; ValueType: string; ValueData: "PNG image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.png"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "PNG image"
Root: HKLM; Subkey: "Software\Classes\snapview.png\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.png\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.png\OpenWithProgids"; ValueType: string; ValueName: "snapview.png"; ValueData: ""; Flags: uninsdeletevalue
; .bmp
Root: HKLM; Subkey: "Software\Classes\snapview.bmp"; ValueType: string; ValueData: "Bitmap image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.bmp"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "Bitmap image"
Root: HKLM; Subkey: "Software\Classes\snapview.bmp\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.bmp\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.bmp\OpenWithProgids"; ValueType: string; ValueName: "snapview.bmp"; ValueData: ""; Flags: uninsdeletevalue
; .gif
Root: HKLM; Subkey: "Software\Classes\snapview.gif"; ValueType: string; ValueData: "GIF image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.gif"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "GIF image"
Root: HKLM; Subkey: "Software\Classes\snapview.gif\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.gif\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.gif\OpenWithProgids"; ValueType: string; ValueName: "snapview.gif"; ValueData: ""; Flags: uninsdeletevalue
; .webp
Root: HKLM; Subkey: "Software\Classes\snapview.webp"; ValueType: string; ValueData: "WebP image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.webp"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "WebP image"
Root: HKLM; Subkey: "Software\Classes\snapview.webp\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.webp\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.webp\OpenWithProgids"; ValueType: string; ValueName: "snapview.webp"; ValueData: ""; Flags: uninsdeletevalue
; .tif
Root: HKLM; Subkey: "Software\Classes\snapview.tif"; ValueType: string; ValueData: "TIFF image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.tif"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "TIFF image"
Root: HKLM; Subkey: "Software\Classes\snapview.tif\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.tif\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.tif\OpenWithProgids"; ValueType: string; ValueName: "snapview.tif"; ValueData: ""; Flags: uninsdeletevalue
; .tiff
Root: HKLM; Subkey: "Software\Classes\snapview.tiff"; ValueType: string; ValueData: "TIFF image"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\snapview.tiff"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "TIFF image"
Root: HKLM; Subkey: "Software\Classes\snapview.tiff\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "Software\Classes\snapview.tiff\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\.tiff\OpenWithProgids"; ValueType: string; ValueName: "snapview.tiff"; ValueData: ""; Flags: uninsdeletevalue

; --- Capabilities under the vendor key (Default Apps in Settings) ---
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Fast, minimal image viewer"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpg"; ValueData: "snapview.jpg"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpeg"; ValueData: "snapview.jpeg"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "snapview.png"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".bmp"; ValueData: "snapview.bmp"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gif"; ValueData: "snapview.gif"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".webp"; ValueData: "snapview.webp"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tif"; ValueData: "snapview.tif"
Root: HKLM; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tiff"; ValueData: "snapview.tiff"
Root: HKLM; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "{#MyVendorKey}\Capabilities"; Flags: uninsdeletevalue
