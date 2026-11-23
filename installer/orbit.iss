; ORBIT installer (Inno Setup 6). Build: cargo build --release, then
;   ISCC /DAppVersion=0.1.2 installer\orbit.iss   ->  dist\ORBIT-Setup.exe
; (release.ps1 and the GitHub release workflow pass the version for you.)

#define AppName "ORBIT"
#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#define AppExe "orbit.exe"

[Setup]
AppId={{6F2B8E1A-0C3D-4B7E-9A41-0B1D5E7C2F90}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=ORBIT
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Admin install: Program Files + a firewall rule so sharing works without a prompt.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputDir=..\dist
OutputBaseFilename=ORBIT-Setup
SetupIconFile=orbit.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; "Start sharing when Windows starts" lives here (written by ORBIT itself); remove it on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "ORBIT"; Flags: uninsdeletevalue

[Run]
; Allow incoming connections to ORBIT (UDP, QUIC). ORBIT itself only listens on the Tailscale
; address, so this doesn't expose it on the local network.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden; StatusMsg: "Configuring Windows Firewall..."
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#AppName}"" dir=in action=allow program=""{app}\{#AppExe}"" protocol=UDP profile=any enable=yes"; Flags: runhidden; StatusMsg: "Configuring Windows Firewall..."
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
; In-app updates run the installer silently: start the new version afterwards, as the normal user.
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: WizardSilent

[UninstallRun]
; "Always on" (the ORBIT Service): remove it before the files go.
Filename: "{app}\{#AppExe}"; Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden; RunOnceId: "RemoveFirewallRule"

[Code]
// "Always on" sharing runs orbit.exe as a Windows service, which keeps the file in use: stop
// it before replacing the files, and start it again afterwards (in-app updates included).
var
  ServiceWasInstalled: Boolean;

function ServiceInstalled(): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{sys}\sc.exe'), 'query ORBIT', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code, I: Integer;
begin
  ServiceWasInstalled := ServiceInstalled();
  if ServiceWasInstalled then
  begin
    Exec(ExpandConstant('{sys}\sc.exe'), 'stop ORBIT', '', SW_HIDE, ewWaitUntilTerminated, Code);
    // Wait until it has stopped (and with it the agent), up to ~10 s.
    for I := 1 to 20 do
    begin
      Sleep(500);
      Exec(ExpandConstant('{cmd}'), '/C sc query ORBIT | find "STOPPED"', '', SW_HIDE, ewWaitUntilTerminated, Code);
      if Code = 0 then Break;
    end;
  end;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if (CurStep = ssPostInstall) and ServiceWasInstalled then
    Exec(ExpandConstant('{sys}\sc.exe'), 'start ORBIT', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;
