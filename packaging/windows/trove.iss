; Inno Setup 6 script — builds Trove-<version>-Setup.exe from the
; already-built exes; it compiles nothing. CI invokes it as
;
;   ISCC.exe //DAppVersion=<version> //DBindir=<release-dir> //O<outdir> packaging/windows/trove.iss
;
; (double slashes because the runner drives ISCC from bash, where a single
; slash gets mangled into a path). Locally, iscc resolves relative paths
; against this script's directory, so the default Bindir below works from a
; checkout.

#define AppName "Trove"
#define AppPublisher "panzhifu"
#define AppURL "https://github.com/panzhifu/trove"

#ifndef AppVersion
#define AppVersion "0.0.0"
#endif

#ifndef Bindir
#define Bindir "..\..\target\x86_64-pc-windows-msvc\release"
#endif

[Setup]
; Fixed GUID: identifies the installation across versions, so an upgrade
; lands in place and the uninstaller registry entry stays single. Never
; regenerate it casually.
AppId={{8E5F1A02-6C3B-4A7D-9B4E-2F1D3A5C7E09}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
; The library's data lives under %APPDATA%\trove and %LOCALAPPDATA%\trove
; (see trove-core/src/paths.rs), never under the install dir, so uninstalling
; removes the binaries and nothing else — no [UninstallDelete] on purpose.
DefaultDirName={autopf}\Trove
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=.
OutputBaseFilename=Trove-{#AppVersion}-Setup
SetupIconFile=..\..\design\icon\trove.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\trove-app.exe

[Languages]
; Simplified Chinese ships with Inno Setup 6.3+ as an official language
; (compiler:Languages\ChineseSimplified.isl) — no download, no fallback
; logic; choco's innosetup is well past 6.3.
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinese"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
    GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Bindir}\trove-app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Bindir}\trove.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\trove-app.exe"
Name: "{group}\{#AppName} CLI"; Filename: "{app}\trove.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\trove-app.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\trove-app.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; \
    Flags: nowait postinstall skipifsilent
