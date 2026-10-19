; Godspeed for Windows — per-user installer (Inno Setup 6).
;
; Built by scripts/package-windows.ps1, which passes the version, the package
; architecture, and the staged portable directory:
;   ISCC.exe /DAppVersion=0.2.97 /DArch=x86_64 /DPackageDir=<stage> /DOutputDir=<out> godspeed.iss
;
; Installs into %LOCALAPPDATA%\Programs\Godspeed without elevation, like VS
; Code's user setup: the directory stays writable by its user, so the in-app
; updater (crates/update/src/windows.rs) can replace godspeed.exe in place. The
; staged directory already carries zeron-update.json, which marks the install
; as update-managed. Re-running a newer installer upgrades in place; user data
; lives in %LOCALAPPDATA%\Godspeed and is never touched here.

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=x.y.z)
#endif
#ifndef Arch
  #error Arch must be defined (/DArch=x86_64 or /DArch=aarch64)
#endif
#ifndef PackageDir
  #error PackageDir must be defined (/DPackageDir=<staged package directory>)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

#if Arch == "aarch64"
  #define ArchAllowed "arm64"
#else
  #define ArchAllowed "x64compatible"
#endif

[Setup]
; Never change AppId: it identifies the installation across upgrades, and
; crates/update/src/windows.rs refreshes DisplayVersion under this key after
; in-app updates.
AppId={{8105F319-A824-46C3-967A-8B3177CF8FA5}
AppName=Godspeed
AppVersion={#AppVersion}
AppVerName=Godspeed {#AppVersion}
AppPublisher=Godspeed
AppPublisherURL=https://github.com/ShriD5/godspeed-code
AppSupportURL=https://github.com/ShriD5/godspeed-code/issues
AppUpdatesURL=https://github.com/ShriD5/godspeed-code/releases
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
DefaultDirName={autopf}\Godspeed
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchAllowed}
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=godspeed-{#AppVersion}-windows-{#Arch}-setup
SetupIconFile=godspeed.ico
UninstallDisplayIcon={app}\godspeed.exe
UninstallDisplayName=Godspeed
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; A running Godspeed is closed through the Restart Manager before its files are
; replaced; the updated app starts again from the finish page.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#PackageDir}\godspeed.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\zeron-update.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Godspeed"; Filename: "{app}\godspeed.exe"
Name: "{autodesktop}\Godspeed"; Filename: "{app}\godspeed.exe"; Tasks: desktopicon

[Registry]
; godspeed:// conversation links — the scheme macOS registers in Info.plist and
; Linux in godspeed.desktop.
Root: HKCU; Subkey: "Software\Classes\godspeed"; ValueType: string; ValueName: ""; ValueData: "URL:Godspeed"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\godspeed"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\godspeed\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\godspeed.exe"",0"
Root: HKCU; Subkey: "Software\Classes\godspeed\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\godspeed.exe"" ""%1"""

[Run]
Filename: "{app}\godspeed.exe"; Description: "{cm:LaunchProgram,Godspeed}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Leftovers of in-app updates (crates/update/src/windows.rs).
Type: files; Name: "{app}\godspeed.exe.old"
Type: files; Name: "{app}\.zeron-update-incoming.exe"
Type: filesandordirs; Name: "{app}\.zeron-update-*"
