; Prime Files installer (Inno Setup 6).
;
; Built by .github/workflows/release.yml:
;   ISCC /DAppVersion=0.1.0 /DExeSource=..\target\dist\nohrs.exe /DIndexerSource=..\target\dist\prime-files-indexer.exe installer\prime-files.iss
;
; Installs for the current user only (no administrator to install), the way
; per-user app installers do; only the dependencies that need it ask for
; elevation, through Windows' own prompt.
;
; What the program needs beyond Windows itself, found by reading its imports:
; - vcruntime140.dll, the Visual C++ runtime. Downloaded from Microsoft and
;   installed when the machine lacks it.
; - mfplat.dll / mfreadwrite.dll, Media Foundation. Windows "N" editions ship
;   without it, and without it the program does not start at all: the Media
;   Feature Pack is added through DISM there.
; - 7-Zip, optional: the preview lists .rar/.7z/.iso through 7z.exe. Offered
;   as a task when it is not installed, downloaded from 7-zip.org.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef ExeSource
  #define ExeSource "..\target\dist\nohrs.exe"
#endif
#ifndef IndexerSource
  #define IndexerSource "..\target\dist\prime-files-indexer.exe"
#endif
; Everything's SDK DLLs (Everything32/64/ARM/ARM64.dll). The program loads the
; one for its architecture from {app}\dll when the person chooses to reuse the
; index of an installed Everything; without them that choice says so instead.
#ifndef DllSource
  #define DllSource "..\target\dist\dll"
#endif

#define AppName "Prime Files"
#define AppExe "PrimeFiles.exe"
#define SevenZipUrl "https://www.7-zip.org/a/7z2409-x64.exe"
#define VcRedistUrl "https://aka.ms/vs/17/release/vc_redist.x64.exe"

[Setup]
AppId={{6B1E4D2A-7C3F-4E8B-9A51-2F0D8C6E4B17}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Prime Files
AppPublisherURL=https://github.com/CakeDev4k/prime-files
AppSupportURL=https://github.com/CakeDev4k/prime-files/issues
AppUpdatesURL=https://github.com/CakeDev4k/prime-files/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1903: the program links the system ICU (icuuc.dll), which
; arrived there.
MinVersion=10.0.18362
OutputDir=..\dist
OutputBaseFilename=PrimeFiles-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
LicenseFile=..\LICENSE
SetupIconFile=..\assets\icon\prime-files.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
ptbr.SevenZipTask=Instalar o 7-Zip (para ver .rar, .7z e .iso no visualizador)
en.SevenZipTask=Install 7-Zip (to see .rar, .7z and .iso files in the preview)
ptbr.Extras=Extras:
en.Extras=Extras:
ptbr.InstallingVc=Instalando o Visual C++ Runtime da Microsoft...
en.InstallingVc=Installing Microsoft's Visual C++ Runtime...
ptbr.InstallingSevenZip=Instalando o 7-Zip...
en.InstallingSevenZip=Installing 7-Zip...
ptbr.InstallingMedia=Ativando o Media Feature Pack do Windows (pode demorar)...
en.InstallingMedia=Turning on Windows' Media Feature Pack (this can take a while)...
ptbr.DownloadFailed=Não foi possível baixar %1.%n%n%2%n%nVerifique a internet e tente de novo.
en.DownloadFailed=Could not download %1.%n%n%2%n%nCheck the connection and try again.
ptbr.MediaRestart=O Media Feature Pack foi ativado. Reinicie o Windows antes de abrir o Prime Files.
en.MediaRestart=The Media Feature Pack was turned on. Restart Windows before opening Prime Files.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "sevenzip"; Description: "{cm:SevenZipTask}"; GroupDescription: "{cm:Extras}"; Check: SevenZipMissing

[Files]
Source: "{#ExeSource}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
; Instant indexing's service. Shipped here, installed only when the person
; turns it on in Settings (it copies itself to Program Files then).
Source: "{#IndexerSource}"; DestDir: "{app}"; Flags: ignoreversion
; Everything's SDK, read only when the person picks Everything's index in
; Settings › Search. A build without the DLLs installs without them.
Source: "{#DllSource}\*.dll"; DestDir: "{app}\dll"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; `shellexec` so Windows asks for elevation: these install machine-wide.
Filename: "{tmp}\vc_redist.x64.exe"; Parameters: "/install /quiet /norestart"; StatusMsg: "{cm:InstallingVc}"; Flags: shellexec waituntilterminated; Check: VcRedistDownloaded
Filename: "{tmp}\7zip-setup.exe"; Parameters: "/S"; StatusMsg: "{cm:InstallingSevenZip}"; Flags: shellexec waituntilterminated; Check: SevenZipDownloaded
Filename: "{sys}\Dism.exe"; Parameters: "/Online /Add-Capability /CapabilityName:Media.MediaFeaturePack~~~~0.0.1.0 /NoRestart"; StatusMsg: "{cm:InstallingMedia}"; Verb: "runas"; Flags: shellexec waituntilterminated runhidden; Check: MediaFoundationMissing; AfterInstall: TellMediaRestart
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent; Check: not MediaFoundationMissing
; An update started from the app's "Atualizar agora" runs silently with
; /RELAUNCH=1: the app closed itself for it, so it is opened again.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: Relaunch

[UninstallRun]
; The service is machine-wide: removing it asks for elevation, and only when
; it was installed.
Filename: "{app}\prime-files-indexer.exe"; Parameters: "--uninstall"; Verb: "runas"; Flags: shellexec waituntilterminated runhidden; Check: IndexerInstalled; RunOnceId: "RemoveIndexer"

[Code]
var
  DownloadPage: TDownloadWizardPage;
  VcDownloaded: Boolean;
  SevenDownloaded: Boolean;

// The Visual C++ 2015-2022 runtime registers itself here when installed.
function VcRedistMissing: Boolean;
var
  Installed: Cardinal;
begin
  Result := not (RegQueryDWordValue(HKLM64, 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64', 'Installed', Installed) and (Installed = 1));
end;

// The program finds 7z.exe where its installer puts it.
function SevenZipMissing: Boolean;
begin
  Result := not FileExists(ExpandConstant('{commonpf64}\7-Zip\7z.exe'));
end;

function MediaFoundationMissing: Boolean;
begin
  Result := not FileExists(ExpandConstant('{sys}\mfplat.dll'));
end;

// Any user may read whether a service exists.
function IndexerInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\PrimeFilesIndexer');
end;

function Relaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:RELAUNCH|0}') = '1');
end;

function VcRedistDownloaded: Boolean;
begin
  Result := VcDownloaded;
end;

function SevenZipDownloaded: Boolean;
begin
  Result := SevenDownloaded;
end;

procedure TellMediaRestart;
begin
  if not WizardSilent then
    MsgBox(CustomMessage('MediaRestart'), mbInformation, MB_OK);
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing), SetupMessage(msgPreparingDesc), nil);
end;

// Downloads what is missing once the person has confirmed, before any file
// is copied, so a failed download leaves nothing half installed.
function NextButtonClick(CurPageID: Integer): Boolean;
var
  Wanted: Boolean;
begin
  Result := True;
  if CurPageID <> wpReady then
    Exit;

  VcDownloaded := False;
  SevenDownloaded := False;
  Wanted := False;
  DownloadPage.Clear;
  if VcRedistMissing then
  begin
    DownloadPage.Add('{#VcRedistUrl}', 'vc_redist.x64.exe', '');
    Wanted := True;
  end;
  if WizardIsTaskSelected('sevenzip') then
  begin
    DownloadPage.Add('{#SevenZipUrl}', '7zip-setup.exe', '');
    Wanted := True;
  end;
  if not Wanted then
    Exit;

  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
      VcDownloaded := VcRedistMissing;
      SevenDownloaded := WizardIsTaskSelected('sevenzip');
    except
      if DownloadPage.AbortedByUser then
        Log('Download aborted by the user.')
      else
        SuppressibleMsgBox(FmtMessage(CustomMessage('DownloadFailed'), ['Visual C++ Runtime / 7-Zip', GetExceptionMessage]), mbCriticalError, MB_OK, IDOK);
      Result := False;
    end;
  finally
    DownloadPage.Hide;
  end;
end;

// Gives folders back to Explorer if Prime Files had been made the default
// file manager (Settings > Explorer), the same entries its "No" removes, so
// nothing is left pointing at a program that is gone.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Value: String;
  NewWindow: String;
  ResultCode: Integer;
begin
  if CurUninstallStep <> usUninstall then
    Exit;
  // Listary's choice lives in its own JSON, which the program edits (and
  // restarts Listary for); the files are still here at this step.
  if not Exec(ExpandConstant('{app}\{#AppExe}'), 'forget-listary', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Log('Could not run forget-listary.')
  else if ResultCode <> 0 then
    Log(Format('forget-listary ended with %d.', [ResultCode]));
  RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Prime Files');
  RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\Directory\shell\nohrs');
  RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\Drive\shell\nohrs');
  if RegQueryStringValue(HKCU, 'Software\Classes\Directory\shell', '', Value) and (Value = 'nohrs') then
    RegDeleteValue(HKCU, 'Software\Classes\Directory\shell', '');
  if RegQueryStringValue(HKCU, 'Software\Classes\Drive\shell', '', Value) and (Value = 'nohrs') then
    RegDeleteValue(HKCU, 'Software\Classes\Drive\shell', '');
  NewWindow := 'Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow';
  if RegQueryStringValue(HKCU, NewWindow + '\command', '', Value) and (Pos(Lowercase('{#AppExe}'), Lowercase(Value)) > 0) then
    RegDeleteKeyIncludingSubkeys(HKCU, NewWindow);
end;
