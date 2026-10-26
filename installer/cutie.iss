; cutie setup — per-user, no admin rights.
;
; Build with `installer\build.ps1`: it compiles cutie, stages the files and
; passes the version in. Compiling this script by hand needs
;   ISCC /DAppVersion=x.y.z /DSrcDir=<folder with cutie.exe + cutie.ico> cutie.iss
;
; What it does:
;   - installs cutie.exe into %LOCALAPPDATA%\cutie (the same folder
;     `cutie.exe --install` uses, so an older manual install is upgraded)
;   - registers "Download with Cutie" in the Explorer context menu
;   - adds a Start-Menu shortcut and an entry in Settings > Apps
;   - installs whatever is missing of yt-dlp, ffmpeg and a JavaScript runtime
;     (Deno) through winget, or downloads yt-dlp.exe directly without winget

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SrcDir
  #define SrcDir "..\dist\stage"
#endif

#define MenuText "Download with Cutie"
#define SilentArgs "--silent --accept-package-agreements --accept-source-agreements --disable-interactivity"

[Setup]
AppId={{8C1F3B52-6C2E-4D0B-9E7A-5A1C7F3E2B10}
AppName=cutie
AppVersion={#AppVersion}
AppVerName=cutie {#AppVersion}
AppPublisher=KiraiEEE
AppPublisherURL=https://github.com/KiraiEEE/Cutie
AppSupportURL=https://github.com/KiraiEEE/Cutie/issues
DefaultDirName={localappdata}\cutie
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=no
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=cutie-{#AppVersion}-setup
SetupIconFile={#SrcDir}\cutie.ico
UninstallDisplayIcon={app}\cutie.exe
UninstallDisplayName=cutie
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
CloseApplications=yes

[Tasks]
Name: deps; Description: "Install what cutie needs: {code:MissingList}"; Check: AnythingMissing

[Files]
Source: "{#SrcDir}\cutie.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\cutie.ico"; DestDir: "{app}"; Flags: ignoreversion
; only present when there is no winget and the direct download worked
Source: "{tmp}\yt-dlp.exe"; DestDir: "{app}"; Flags: external ignoreversion skipifsourcedoesntexist; Tasks: deps

[Registry]
; folder background (also Desktop and drive roots) → the folder is %V
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\cutie"; ValueType: string; ValueName: ""; ValueData: "{#MenuText}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\cutie"; ValueType: string; ValueName: "MUIVerb"; ValueData: "{#MenuText}"
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\cutie"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\cutie.ico"""
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\cutie\command"; ValueType: string; ValueName: ""; ValueData: """{app}\cutie.exe"" ""%V"""

Root: HKCU; Subkey: "Software\Classes\Desktop\Background\shell\cutie"; ValueType: string; ValueName: ""; ValueData: "{#MenuText}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Desktop\Background\shell\cutie"; ValueType: string; ValueName: "MUIVerb"; ValueData: "{#MenuText}"
Root: HKCU; Subkey: "Software\Classes\Desktop\Background\shell\cutie"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\cutie.ico"""
Root: HKCU; Subkey: "Software\Classes\Desktop\Background\shell\cutie\command"; ValueType: string; ValueName: ""; ValueData: """{app}\cutie.exe"" ""%V"""

Root: HKCU; Subkey: "Software\Classes\Drive\Background\shell\cutie"; ValueType: string; ValueName: ""; ValueData: "{#MenuText}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Drive\Background\shell\cutie"; ValueType: string; ValueName: "MUIVerb"; ValueData: "{#MenuText}"
Root: HKCU; Subkey: "Software\Classes\Drive\Background\shell\cutie"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\cutie.ico"""
Root: HKCU; Subkey: "Software\Classes\Drive\Background\shell\cutie\command"; ValueType: string; ValueName: ""; ValueData: """{app}\cutie.exe"" ""%V"""

; right-click on a folder itself → the folder is %1
Root: HKCU; Subkey: "Software\Classes\Directory\shell\cutie"; ValueType: string; ValueName: ""; ValueData: "{#MenuText}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\cutie"; ValueType: string; ValueName: "MUIVerb"; ValueData: "{#MenuText}"
Root: HKCU; Subkey: "Software\Classes\Directory\shell\cutie"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\cutie.ico"""
Root: HKCU; Subkey: "Software\Classes\Directory\shell\cutie\command"; ValueType: string; ValueName: ""; ValueData: """{app}\cutie.exe"" ""%1"""

[Icons]
; same path `cutie.exe --install` uses, so an old shortcut is replaced
Name: "{autoprograms}\cutie\cutie"; Filename: "{app}\cutie.exe"; IconFilename: "{app}\cutie.ico"; Comment: "Download the video link on the clipboard"

[UninstallDelete]
; folders an older `cutie.exe --install` created — Inno only removes the ones
; it made itself
Type: dirifempty; Name: "{autoprograms}\cutie"
Type: dirifempty; Name: "{app}"

[Run]
; yt-dlp's winget package pulls in ffmpeg and Deno as dependencies
Filename: "{code:WingetPath}"; Parameters: "install --exact --id yt-dlp.yt-dlp {#SilentArgs}"; StatusMsg: "Installing yt-dlp, ffmpeg and Deno with winget (this can take a minute)..."; Flags: runhidden waituntilterminated; Tasks: deps; Check: UseWinget and NeedYtdlp
Filename: "{code:WingetPath}"; Parameters: "install --exact --id yt-dlp.FFmpeg {#SilentArgs}"; StatusMsg: "Installing ffmpeg with winget..."; Flags: runhidden waituntilterminated; Tasks: deps; Check: UseWinget and NeedFfmpegOnly
Filename: "{code:WingetPath}"; Parameters: "install --exact --id DenoLand.Deno {#SilentArgs}"; StatusMsg: "Installing Deno with winget..."; Flags: runhidden waituntilterminated; Tasks: deps; Check: UseWinget and NeedJsOnly

[Code]
var
  DownloadPage: TDownloadWizardPage;
  NeedYtdlpFlag, NeedFfmpegFlag, NeedJsFlag: Boolean;

function ExpandEnvironmentStrings(lpSrc: String; lpDst: String; nSize: DWORD): DWORD;
  external 'ExpandEnvironmentStringsW@kernel32.dll stdcall';

function ExpandEnv(const S: String): String;
var
  Buf: String;
  N: DWORD;
begin
  Result := S;
  if Pos('%', S) = 0 then
    Exit;
  SetLength(Buf, 32767);
  N := ExpandEnvironmentStrings(S, Buf, 32767);
  if (N > 0) and (N <= 32767) then
    Result := Copy(Buf, 1, N - 1);
end;

{ The folders cutie itself searches. The PATH is read from the registry too:
  the installer may have inherited a stale one from Explorer. }
function ToolDirs(): String;
var
  P: String;
begin
  Result := ExpandConstant('{localappdata}\cutie') + ';' + GetEnv('PATH');
  if RegQueryStringValue(HKCU, 'Environment', 'Path', P) then
    Result := Result + ';' + P;
  if RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', P) then
    Result := Result + ';' + P;
  Result := Result
    + ';' + ExpandConstant('{localappdata}\Microsoft\WinGet\Links')
    + ';' + GetEnv('USERPROFILE') + '\scoop\shims'
    + ';' + ExpandConstant('{commonappdata}\chocolatey\bin')
    + ';' + ExpandConstant('{commonpf64}\nodejs');
  Result := ExpandEnv(Result);
end;

function HasTool(const Exe: String): Boolean;
var
  Dirs, D: String;
  I: Integer;
begin
  Result := False;
  Dirs := ToolDirs() + ';';
  while (not Result) and (Dirs <> '') do
  begin
    I := Pos(';', Dirs);
    D := Trim(RemoveQuotes(Copy(Dirs, 1, I - 1)));
    Delete(Dirs, 1, I);
    if D <> '' then
      Result := FileExists(AddBackslash(D) + Exe);
  end;
end;

function WingetPath(Param: String): String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\winget.exe');
end;

function UseWinget(): Boolean;
begin
  Result := FileExists(WingetPath(''));
end;

function NeedYtdlp(): Boolean;
begin
  Result := NeedYtdlpFlag;
end;

{ yt-dlp is there already, only ffmpeg is missing }
function NeedFfmpegOnly(): Boolean;
begin
  Result := NeedFfmpegFlag and not NeedYtdlpFlag;
end;

function NeedJsOnly(): Boolean;
begin
  Result := NeedJsFlag and not NeedYtdlpFlag;
end;

function AnythingMissing(): Boolean;
begin
  Result := NeedYtdlpFlag or NeedFfmpegFlag or NeedJsFlag;
end;

function MissingList(Param: String): String;
begin
  Result := '';
  if NeedYtdlpFlag then
    Result := Result + ', yt-dlp';
  if NeedFfmpegFlag then
    Result := Result + ', ffmpeg';
  if NeedJsFlag then
    Result := Result + ', Deno';
  Delete(Result, 1, 2);
  if UseWinget() then
    Result := Result + ' (via winget)'
  else if NeedYtdlpFlag then
    Result := 'yt-dlp (direct download)';
end;

procedure InitializeWizard();
begin
  NeedYtdlpFlag := not HasTool('yt-dlp.exe');
  NeedFfmpegFlag := not HasTool('ffmpeg.exe');
  { yt-dlp needs a JavaScript runtime for YouTube; cutie passes node on when
    it finds it, yt-dlp picks up deno by itself }
  NeedJsFlag := not (HasTool('deno.exe') or HasTool('node.exe'));
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing),
    'Downloading yt-dlp...', nil);
end;

{ Without winget, fetch the standalone yt-dlp.exe into the app folder —
  cutie looks next to itself first. }
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID <> wpReady) or UseWinget() or (not NeedYtdlpFlag)
    or (not WizardIsTaskSelected('deps')) then
    Exit;
  DownloadPage.Clear;
  DownloadPage.Add('https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe', 'yt-dlp.exe', '');
  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
    except
      if not DownloadPage.AbortedByUser then
        SuppressibleMsgBox('Could not download yt-dlp:' + #13#10 + GetExceptionMessage + #13#10#13#10 +
          'cutie is installed anyway. Get yt-dlp from https://github.com/yt-dlp/yt-dlp and put yt-dlp.exe next to cutie.exe.',
          mbError, MB_OK, IDOK);
    end;
  finally
    DownloadPage.Hide;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
var
  Msg, Still: String;
begin
  if CurPageID <> wpFinished then
    Exit;
  Msg := 'cutie is installed.' + #13#10#13#10 +
    'Copy a video link, then right-click inside any folder and choose "{#MenuText}".' + #13#10 +
    '(On Windows 11 it is under "Show more options".)';
  Still := '';
  if not HasTool('yt-dlp.exe') then
    Still := Still + ', yt-dlp';
  if not HasTool('ffmpeg.exe') then
    Still := Still + ', ffmpeg';
  if Still <> '' then
  begin
    Delete(Still, 1, 2);
    Msg := Msg + #13#10#13#10 + 'Still missing: ' + Still + '. Install with:' + #13#10 +
      'winget install yt-dlp.yt-dlp';
  end;
  WizardForm.FinishedLabel.Caption := Msg;
end;
