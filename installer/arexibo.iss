; Installer for the Windows build of Arexibo, made by .github/workflows/windows.yml
; with Inno Setup: iscc /DAppVersion=x.y.z /DSourceDir=<folder made by windeployqt>

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist-windows"
#endif

[Setup]
AppId={{4E5B8185-473B-4E4B-9290-748938B0ACF4}
AppName=Arexibo
AppVersion={#AppVersion}
AppPublisher=Arexibo
DefaultDirName={autopf}\Arexibo
DisableProgramGroupPage=yes
OutputDir=output
OutputBaseFilename=arexibo-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
UninstallDisplayIcon={app}\arexibo.exe

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "autostart"; Description: "Start Arexibo when a user signs in"; Flags: unchecked

[Dirs]
; The player's data (settings, cache): not under Program Files, where users cannot write.
Name: "{commonappdata}\Arexibo"; Permissions: users-modify

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Arexibo"; Filename: "{app}\arexibo.exe"; Parameters: """{commonappdata}\Arexibo"""
Name: "{autodesktop}\Arexibo"; Filename: "{app}\arexibo.exe"; Parameters: """{commonappdata}\Arexibo"""; Tasks: desktopicon
Name: "{commonstartup}\Arexibo"; Filename: "{app}\arexibo.exe"; Parameters: """{commonappdata}\Arexibo"""; Tasks: autostart

[Run]
; The Visual C++ runtime, if the machine has none (windeployqt puts the installer in the folder).
Filename: "{app}\vc_redist.x64.exe"; Parameters: "/install /quiet /norestart"; StatusMsg: "Installing the Visual C++ runtime..."; Check: NeedsVCRedist; Flags: waituntilterminated skipifdoesntexist
; The first start, with what was typed on the CMS page: the player keeps it from then on.
Filename: "{app}\arexibo.exe"; Parameters: "{code:FirstRunArgs}"; Description: "Start Arexibo now"; Flags: postinstall nowait skipifsilent

[Code]
var
  CmsPage: TInputQueryWizardPage;

function NeedsVCRedist: Boolean;
var
  Installed: Cardinal;
begin
  Result := not (RegQueryDWordValue(HKLM64, 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64', 'Installed', Installed)
                 and (Installed = 1));
end;

procedure InitializeWizard;
begin
  CmsPage := CreateInputQueryPage(wpSelectTasks, 'CMS', 'Where is the Xibo CMS?',
    'The address of the CMS and its key are kept by the player after its first start. Leave them empty to give them later.');
  CmsPage.Add('CMS address (for instance https://cms.example.com):', False);
  CmsPage.Add('CMS key:', False);
  CmsPage.Add('Display name (optional):', False);
end;

function Quoted(const Text: string): string;
begin
  Result := '"' + Text + '"';
end;

function FirstRunArgs(Param: string): string;
begin
  Result := Quoted(ExpandConstant('{commonappdata}\Arexibo'));
  if Trim(CmsPage.Values[0]) <> '' then Result := Result + ' --host ' + Quoted(Trim(CmsPage.Values[0]));
  if Trim(CmsPage.Values[1]) <> '' then Result := Result + ' --key ' + Quoted(Trim(CmsPage.Values[1]));
  if Trim(CmsPage.Values[2]) <> '' then Result := Result + ' --display-name ' + Quoted(Trim(CmsPage.Values[2]));
end;
