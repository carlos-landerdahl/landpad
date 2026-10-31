; Instalador do Landpad (Inno Setup 6), gerado pelo GitHub Actions a cada versão.
;
; Instala por usuário, sem pedir administrador, em %LOCALAPPDATA%\Programs\Landpad.
; O próprio Landpad usa este instalador para se atualizar: baixa a versão nova e roda
; com /VERYSILENT, que troca o executável e abre o Landpad de novo, sem assistente.
;
; Compilar à mão (depois de cargo build --release e landpad.exe --export-assets target\assets):
;   ISCC.exe /DVersion=0.1.0 packaging\landpad.iss

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{ADD58170-FB9A-474E-9779-18E051E2CACB}
AppName=Landpad
AppVersion={#Version}
AppVerName=Landpad {#Version}
AppPublisher=Carlos Landerdahl
AppPublisherURL=https://github.com/carlos-landerdahl/landpad
AppSupportURL=https://github.com/carlos-landerdahl/landpad/issues
AppUpdatesURL=https://github.com/carlos-landerdahl/landpad/releases
DefaultDirName={localappdata}\Programs\Landpad
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir=..\target\installer
OutputBaseFilename=Landpad-Setup-{#Version}
SetupIconFile=..\target\assets\landpad.ico
UninstallDisplayIcon={app}\Landpad.exe
UninstallDisplayName=Landpad
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no
; Numa atualização o Landpad já fechou sozinho; se ainda estiver aberto, o Windows o fecha.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Files]
Source: "..\target\release\landpad.exe"; DestDir: "{app}"; DestName: "Landpad.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Landpad"; Filename: "{app}\Landpad.exe"; Comment: "Histórico da área de transferência e captura de tela"

[Registry]
; O Landpad cria esta entrada quando você liga "Iniciar com o Windows"; ao desinstalar, ela sai junto.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "Landpad"; ValueType: none; Flags: uninsdeletevalue

[Run]
; Também roda no modo silencioso: é o que reabre o Landpad depois de uma atualização.
Filename: "{app}\Landpad.exe"; Description: "Abrir o Landpad"; Flags: nowait postinstall

[UninstallDelete]
; Imagens do histórico e atualizações baixadas. As preferências (%APPDATA%\Landpad) ficam,
; como na maioria dos apps, para uma reinstalação começar do jeito que você deixou.
Type: filesandordirs; Name: "{localappdata}\Landpad"

[Code]
// Numa atualização o Landpad chama este instalador e fecha em seguida: espera até 5 s
// ele sair, para o executável não estar em uso na hora da cópia.
function InitializeSetup(): Boolean;
var
  Waited: Integer;
begin
  Waited := 0;
  while CheckForMutexes('Local\Landpad.Instance') and (Waited < 50) do
  begin
    Sleep(100);
    Waited := Waited + 1;
  end;
  Result := True;
end;

// Desinstalar com o Landpad aberto deixaria o executável preso: fecha ele antes.
function InitializeUninstall(): Boolean;
var
  ResultCode: Integer;
begin
  if CheckForMutexes('Local\Landpad.Instance') then
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM Landpad.exe /F', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := True;
end;
