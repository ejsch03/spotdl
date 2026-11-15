; spotdl's Windows installer, spotdl-<version>-<arch>-setup.exe, made with Inno Setup 6.7 or
; later (https://jrsoftware.org/isinfo.php). install.ps1 builds the programs and then this:
;
;   powershell -ExecutionPolicy Bypass -File install.ps1 -BuildOnly
;
; or, with the programs already built: ISCC [/DArch=arm64] [/DBinDir=<folder>] spotdl.iss
; (Arch is x64 or arm64; BinDir is where the .exe files are, target\release by default).
; The installer goes in target\installer. (wizard-*.png are spotdl-128.png and
; spotdl-256.png from assets\icon\png, centered on a transparent background.)
;
; It installs both programs for the current user (in %LOCALAPPDATA%\Programs\spotdl, without
; asking for admin rights) or, if chosen, for everyone (in Program Files). It adds spotdl to
; the Start menu and to the PATH, and to Settings > Apps to uninstall it. The login and
; settings, in %APPDATA%\spotdl, are kept either way.

#if Ver < EncodeVer(6, 7, 0)
  #error Inno Setup 6.7 or later is needed
#endif

#ifndef Arch
  #define Arch "x64"
#endif
#ifndef BinDir
  #define BinDir AddBackslash(SourcePath) + "..\..\target\release"
#endif

#define App BinDir + "\spotdl-gui.exe"
#if !FileExists(App)
  #error The programs aren't built yet (cargo build --release --locked)
#endif
; the version built into the programs from Cargo.toml (see build.rs)
#define Version GetStringFileInfo(App, "ProductVersion")

#if Arch == "x64"
  ; Windows 11 on ARM runs x64 programs too
  #define Architectures "x64compatible"
#elif Arch == "arm64"
  #define Architectures "arm64"
#else
  #error Arch must be x64 or arm64
#endif

[Setup]
AppId={{F8C04EF7-E87E-41A3-B99A-AF43699A8A4A}
AppName=spotdl
AppVersion={#Version}
AppPublisher=spotdl
AppComments=Download songs, albums, playlists and artists from Spotify
UninstallDisplayName=spotdl
UninstallDisplayIcon={app}\spotdl-gui.exe
VersionInfoVersion={#GetVersionNumbersString(App)}
VersionInfoDescription=spotdl Setup
; For the current user, unless the user picks everyone when the installer asks (or it's
; given /ALLUSERS), which needs admin rights. An update keeps the choice made before.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\spotdl
DisableProgramGroupPage=yes
DisableReadyPage=yes
ArchitecturesAllowed={#Architectures}
ArchitecturesInstallIn64BitMode={#Architectures}
MinVersion=10.0
; A running spotdl is stopped so its files can be replaced (see PrepareToInstall); in case
; that didn't work, Windows' Restart Manager closes it (after a 30-second wait for it to quit
; by itself, which it doesn't).
CloseApplications=force
RestartApplications=no
; Explorer, and what's started from it from now on, sees the new PATH
ChangesEnvironment=yes
SetupIconFile=..\icon\spotdl.ico
WizardStyle=modern dynamic
WizardSmallImageFile=..\icon\png\spotdl-64.png,..\icon\png\spotdl-128.png,..\icon\png\spotdl-256.png
; the icon on the last page, centered in the tall image area at 100% and 200% scaling
WizardImageFile=wizard-202x386.png,wizard-404x772.png
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\..\target\installer
OutputBaseFilename=spotdl-{#Version}-{#Arch}-setup

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "path"; Description: "Add spotdl to the &PATH, to use it in a terminal"; GroupDescription: "Command line:"

[Files]
Source: "{#BinDir}\spotdl-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\spotdl.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[InstallDelete]
; the icon install.ps1 put next to the programs before there was an installer
Type: files; Name: "{app}\spotdl.ico"

[Icons]
Name: "{autoprograms}\spotdl"; Filename: "{app}\spotdl-gui.exe"; WorkingDir: "{app}"; Comment: "Search Spotify and download songs, albums and playlists"
Name: "{autodesktop}\spotdl"; Filename: "{app}\spotdl-gui.exe"; WorkingDir: "{app}"; Comment: "Search Spotify and download songs, albums and playlists"; Tasks: desktopicon

[Run]
Filename: "{app}\spotdl-gui.exe"; Description: "{cm:LaunchProgram,spotdl}"; Flags: postinstall nowait skipifsilent

[Code]
{ The environment the PATH is in: the user's, or when installing for everyone, the
  system's. }
procedure GetEnvironmentKey(var Root: Integer; var Key: String);
begin
  if IsAdminInstallMode then
  begin
    Root := HKEY_LOCAL_MACHINE;
    Key := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  end
  else
  begin
    Root := HKEY_CURRENT_USER;
    Key := 'Environment';
  end;
end;

{ Puts the program folder on the PATH, or takes it off. Only that entry is added or
  removed: the rest of the PATH stays as it's stored, %VARIABLES% and all. }
procedure SetOnPath(OnPath: Boolean);
var
  Root, I: Integer;
  Key, Dir, Path, NewPath: String;
  Entries: TArrayOfString;
  Found, First: Boolean;
begin
  GetEnvironmentKey(Root, Key);
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(Root, Key, 'Path', Path) then
    Path := '';
  { the PATH without this folder }
  Entries := StringSplit(Path, [';'], stAll);
  NewPath := '';
  Found := False;
  First := True;
  for I := 0 to GetArrayLength(Entries) - 1 do
    if (Trim(Entries[I]) <> '') and
       PathSame(RemoveBackslashUnlessRoot(Trim(Entries[I])), Dir) then
      Found := True
    else
    begin
      if not First then
        NewPath := NewPath + ';';
      NewPath := NewPath + Entries[I];
      First := False;
    end;
  if Found = OnPath then
    Exit;
  if OnPath then
  begin
    { at the end, before a closing ';' if there is one, so taking it off again leaves the
      PATH as it was }
    if NewPath = '' then
      NewPath := Dir
    else if NewPath[Length(NewPath)] = ';' then
      NewPath := NewPath + Dir + ';'
    else
      NewPath := NewPath + ';' + Dir;
  end;
  if NewPath = '' then
    RegDeleteValue(Root, Key, 'Path')
  else if not RegWriteExpandStringValue(Root, Key, 'Path', NewPath) then
    Log('Couldn''t write the PATH');
  Log('The PATH is now ' + NewPath);
end;

{ Finds the copies of spotdl running from the program folder (the desktop app may be out
  of sight in the notification area), and stops them if asked to. Stopping spotdl is safe:
  a track is only renamed into place once it's complete, and the Downloads list carries on
  next time. }
function FindRunningPrograms(Stop: Boolean): Boolean;
var
  Wmi, Processes, Process: Variant;
  Dir: String;
  I: Integer;
begin
  Result := False;
  Dir := ExpandConstant('{app}');
  try
    Wmi := CreateOleObject('WbemScripting.SWbemLocator');
    Wmi := Wmi.ConnectServer('.', 'root\CIMV2');
    Processes := Wmi.ExecQuery('SELECT Handle, ExecutablePath FROM Win32_Process ' +
      'WHERE Name = ''spotdl.exe'' OR Name = ''spotdl-gui.exe''');
    for I := 0 to Processes.Count - 1 do
    begin
      Process := Processes.ItemIndex(I);
      { other users' programs have no path, unless installing for everyone }
      if not VarIsNull(Process.ExecutablePath) and
         PathSame(ExtractFileDir(Process.ExecutablePath), Dir) then
      begin
        Result := True;
        if Stop then
        begin
          Log('Stopping ' + Process.ExecutablePath + ' (process ' + Process.Handle + ')');
          Process.Terminate(0);
        end;
      end;
    end;
  except
    Log('Couldn''t look for a running spotdl: ' + GetExceptionMessage);
  end;
end;

procedure StopRunningPrograms;
var
  Tries: Integer;
begin
  if not FindRunningPrograms(True) then
    Exit;
  for Tries := 1 to 50 do
  begin
    Sleep(100);
    if not FindRunningPrograms(False) then
      Exit;
  end;
  Log('spotdl is still running');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not FindRunningPrograms(False) then
    Exit;
  if WizardSilent or (SuppressibleMsgBox('spotdl is running, so Setup will close it to ' +
      'update it. Anything left to download carries on when you open spotdl again.',
      mbInformation, MB_OKCANCEL, IDOK) = IDOK) then
    StopRunningPrograms
  else
    Result := 'Quit spotdl, then run Setup again. (If spotdl is in the notification ' +
      'area, right-click its icon there and choose Quit spotdl.)';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SetOnPath(WizardIsTaskSelected('path'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopRunningPrograms;
    SetOnPath(False);
  end;
end;
