; snapview installer — Inno Setup script
; Build: ISCC.exe installer\snapview.iss
;        ISCC.exe /DMyAppVersion=1.2.3 installer\snapview.iss   (CI override)
; Sign:  set SIGNTOOL to a signtool.exe command with cert/timestamp args before running ISCC.
;
; Versioning convention (semver-style):
;   * Small update / bug fix  -> bump PATCH       (1.0.0 -> 1.0.1)
;   * Bigger update / new feature -> bump MINOR   (1.0.5 -> 1.1.0)
;   * Major release / breaking change -> bump MAJOR (1.9.0 -> 2.0.0)
; Keep this in sync with Cargo.toml's `version = "..."`. CI overrides it
; from the pushed git tag (v1.2.3 -> 1.2.3), so for tagged release builds
; only the tag matters.

#define MyAppName "snapview"
#ifndef MyAppVersion
  #define MyAppVersion "1.0.1"
#endif
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
; VersionInfo* stamps setup.exe's Win32 file resource so Explorer's
; Properties dialog, signtool, Add/Remove Programs, etc. see a consistent
; version everywhere. VersionInfoVersion must be a 4-part numeric tag.
VersionInfoVersion={#MyAppVersion}.0
VersionInfoCompany={#MyAppPublisher}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoDescription={#MyAppName} installer
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
DisableProgramGroupPage=yes
; Allow both per-user (no UAC, %LocalAppData%\Programs, HKCU) and per-machine
; (UAC, Program Files, HKLM). Inno Setup prompts the user on launch when both
; are possible. {autopf} / {group} / {autodesktop} / HKA all adapt to match.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
; Smooth in-place upgrades: if snapview.exe is running, prompt the user to
; close it, restart afterwards (or the installer aborts with a clear error
; instead of failing on a locked file).
CloseApplications=yes
RestartApplications=yes
CloseApplicationsFilter=*.exe,*.dll
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
; --- Application registration (HKA\...\Applications\snapview.exe) ---
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\DefaultIcon"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpg"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".jpeg"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".png"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".bmp"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".gif"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".webp"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tif"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExeName}\SupportedTypes"; ValueType: string; ValueName: ".tiff"; ValueData: ""

; --- Per-extension ProgIDs ---
; DefaultIcon "%1" makes Explorer use the file's own thumbnail rather than
; falling back to the Applications\snapview.exe icon when our ProgID is the
; default handler. Without this, picking snapview as the default for .jpg
; replaces every jpg's thumbnail with the snapview app icon.
; .jpg
Root: HKA; Subkey: "Software\Classes\snapview.jpg"; ValueType: string; ValueData: "JPEG image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.jpg"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "JPEG image"
Root: HKA; Subkey: "Software\Classes\snapview.jpg\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.jpg\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.jpg\OpenWithProgids"; ValueType: string; ValueName: "snapview.jpg"; ValueData: ""; Flags: uninsdeletevalue
; .jpeg
Root: HKA; Subkey: "Software\Classes\snapview.jpeg"; ValueType: string; ValueData: "JPEG image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.jpeg"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "JPEG image"
Root: HKA; Subkey: "Software\Classes\snapview.jpeg\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.jpeg\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.jpeg\OpenWithProgids"; ValueType: string; ValueName: "snapview.jpeg"; ValueData: ""; Flags: uninsdeletevalue
; .png
Root: HKA; Subkey: "Software\Classes\snapview.png"; ValueType: string; ValueData: "PNG image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.png"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "PNG image"
Root: HKA; Subkey: "Software\Classes\snapview.png\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.png\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.png\OpenWithProgids"; ValueType: string; ValueName: "snapview.png"; ValueData: ""; Flags: uninsdeletevalue
; .bmp
Root: HKA; Subkey: "Software\Classes\snapview.bmp"; ValueType: string; ValueData: "Bitmap image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.bmp"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "Bitmap image"
Root: HKA; Subkey: "Software\Classes\snapview.bmp\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.bmp\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.bmp\OpenWithProgids"; ValueType: string; ValueName: "snapview.bmp"; ValueData: ""; Flags: uninsdeletevalue
; .gif
Root: HKA; Subkey: "Software\Classes\snapview.gif"; ValueType: string; ValueData: "GIF image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.gif"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "GIF image"
Root: HKA; Subkey: "Software\Classes\snapview.gif\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.gif\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.gif\OpenWithProgids"; ValueType: string; ValueName: "snapview.gif"; ValueData: ""; Flags: uninsdeletevalue
; .webp
Root: HKA; Subkey: "Software\Classes\snapview.webp"; ValueType: string; ValueData: "WebP image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.webp"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "WebP image"
Root: HKA; Subkey: "Software\Classes\snapview.webp\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.webp\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.webp\OpenWithProgids"; ValueType: string; ValueName: "snapview.webp"; ValueData: ""; Flags: uninsdeletevalue
; .tif
Root: HKA; Subkey: "Software\Classes\snapview.tif"; ValueType: string; ValueData: "TIFF image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.tif"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "TIFF image"
Root: HKA; Subkey: "Software\Classes\snapview.tif\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.tif\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.tif\OpenWithProgids"; ValueType: string; ValueName: "snapview.tif"; ValueData: ""; Flags: uninsdeletevalue
; .tiff
Root: HKA; Subkey: "Software\Classes\snapview.tiff"; ValueType: string; ValueData: "TIFF image"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\snapview.tiff"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "TIFF image"
Root: HKA; Subkey: "Software\Classes\snapview.tiff\DefaultIcon"; ValueType: string; ValueData: "%1"
Root: HKA; Subkey: "Software\Classes\snapview.tiff\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.tiff\OpenWithProgids"; ValueType: string; ValueName: "snapview.tiff"; ValueData: ""; Flags: uninsdeletevalue

; --- Capabilities under the vendor key (Default Apps in Settings) ---
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Fast, minimal image viewer"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: """{app}\{#MyAppExeName}"",0"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpg"; ValueData: "snapview.jpg"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpeg"; ValueData: "snapview.jpeg"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "snapview.png"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".bmp"; ValueData: "snapview.bmp"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gif"; ValueData: "snapview.gif"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".webp"; ValueData: "snapview.webp"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tif"; ValueData: "snapview.tif"
Root: HKA; Subkey: "{#MyVendorKey}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tiff"; ValueData: "snapview.tiff"
Root: HKA; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "{#MyVendorKey}\Capabilities"; Flags: uninsdeletevalue

; Tell Explorer to refresh file associations immediately on install/uninstall
; (otherwise the new ProgIDs only show up in 'Open with' after a logoff).
[Code]
const
  SHCNE_ASSOCCHANGED = $08000000;
  SHCNF_IDLIST = $0000;
procedure SHChangeNotify(wEventId: Cardinal; uFlags: Cardinal; dwItem1, dwItem2: Cardinal);
  external 'SHChangeNotify@shell32.dll stdcall';

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, 0, 0);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, 0, 0);
end;
