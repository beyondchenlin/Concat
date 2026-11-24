; Concat's Windows installer.
;
; Built by build-app.yml with Inno Setup from the staged folder the .msi
; is also made of, so the two ship the same files; this is the one a
; person double-clicks, and it puts Concat in the Start menu and can take
; it out again. Everything it needs is handed in on the command line:
;
;   iscc /DVersion=0.2.12 /DArch=x64compatible /DSuffix=x86_64 ^
;        /DStage=C:\...\stage\Concat-0.2.12-windows-x86_64 /DOut=C:\...\stage ^
;        assets\windows\concat.iss
;
; Arch is Inno's own word for the machine: x64compatible for the x86_64
; build (which also installs on ARM PCs, under emulation), arm64 for the
; native one. Suffix is the bundle's word for it, which names the file.

#ifndef Version
  #error Version is required
#endif
#ifndef Arch
  #error Arch is required: x64compatible or arm64
#endif
#ifndef Suffix
  #error Suffix is required: x86_64 or aarch64
#endif
#ifndef Stage
  #error Stage is required: the staged folder to install
#endif
#ifndef Out
  #define Out "."
#endif

[Setup]
; The existing BK product identity is preserved from the installed 0.2.12
; release. The upstream Concat install has its own identity and is untouched.
AppId={{A8DCE4CA-3E70-4263-9ED8-319D23851DC4}
AppName=BK CUT
AppVersion={#Version}
AppVerName=BK CUT {#Version}
AppPublisher=Concat contributors
AppPublisherURL=https://github.com/beyondchenlin/Concat
AppSupportURL=https://github.com/beyondchenlin/Concat/issues
AppUpdatesURL=https://github.com/beyondchenlin/Concat/releases
DefaultDirName={localappdata}\Programs\BK-CAT
DefaultGroupName=BK CUT
UsePreviousAppDir=yes
CloseApplications=no
RestartApplications=no
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir={#Out}
OutputBaseFilename=BK CUT-{#Version}-windows-{#Suffix}-setup
SetupIconFile=..\icons\bk-cut-app.ico
ChangesAssociations=yes
UninstallDisplayIcon={app}\bk-cut-app.ico
UninstallDisplayName=BK CUT
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#Arch}
ArchitecturesInstallIn64BitMode={#Arch}
; For the user alone unless they ask for the machine: no prompt for an
; administrator to install a video editor into one's own account.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; Follow Windows' display language when the setup speaks it, and ask only
; when it does not.
ShowLanguageDialog=auto

[Languages]
; Every language the app ships. Inno Setup carries the official
; translations. The ones it keeps as unofficial - Chinese, Persian,
; Croatian, Bengali, Hindi, Indonesian, Urdu, Vietnamese - and the ones
; made official lately - Korean, Arabic and Thai, which an older compiler
; lacks - are vendored in languages/ from the Inno Setup 6.7.1 tree
; (https://github.com/jrsoftware/issrc/tree/is-6_7_1/Files/Languages),
; verbatim, so the setup offers them whatever the compiler on the build
; machine ships (#277). Some are in an older format, which the compiler
; still reads with the newer messages in English and a warning for each:
; Traditional Chinese 6.1, Urdu 6.1, Indonesian 6.4, and Bengali and Hindi
; 5.5.3, whose obsolete title and copyright font sizes it ignores.
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "turkish"; MessagesFile: "compiler:Languages\Turkish.isl"
Name: "chinesesimplified"; MessagesFile: "languages\ChineseSimplified.isl"
Name: "chinesetraditional"; MessagesFile: "languages\ChineseTraditional.isl"
Name: "korean"; MessagesFile: "languages\Korean.isl"
Name: "croatian"; MessagesFile: "languages\Croatian.isl"
Name: "farsi"; MessagesFile: "languages\Farsi.isl"
Name: "dutch"; MessagesFile: "compiler:Languages\Dutch.isl"
Name: "hebrew"; MessagesFile: "compiler:Languages\Hebrew.isl"
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"
Name: "ukrainian"; MessagesFile: "compiler:Languages\Ukrainian.isl"
Name: "arabic"; MessagesFile: "languages\Arabic.isl"
Name: "thai"; MessagesFile: "languages\Thai.isl"
Name: "bengali"; MessagesFile: "languages\Bengali.islu"
Name: "hindi"; MessagesFile: "languages\Hindi.islu"
Name: "indonesian"; MessagesFile: "languages\Indonesian.isl"
Name: "urdu"; MessagesFile: "languages\Urdu.isl"
Name: "vietnamese"; MessagesFile: "languages\Vietnamese.isl"

[CustomMessages]
english.SafeGraphics=safe graphics
german.SafeGraphics=sichere Grafik
spanish.SafeGraphics=gráficos seguros
french.SafeGraphics=mode graphique sans échec
italian.SafeGraphics=grafica sicura
japanese.SafeGraphics=セーフグラフィックス
brazilianportuguese.SafeGraphics=gráficos seguros
russian.SafeGraphics=безопасная графика
turkish.SafeGraphics=güvenli grafik
chinesesimplified.SafeGraphics=安全图形模式
chinesetraditional.SafeGraphics=安全圖形模式
korean.SafeGraphics=안전 그래픽
croatian.SafeGraphics=sigurna grafika
farsi.SafeGraphics=گرافیک ایمن
dutch.SafeGraphics=veilige grafische modus
hebrew.SafeGraphics=מצב גרפי בטוח
polish.SafeGraphics=bezpieczna grafika
ukrainian.SafeGraphics=безпечна графіка
arabic.SafeGraphics=وضع الرسومات الآمن
thai.SafeGraphics=โหมดกราฟิกปลอดภัย
bengali.SafeGraphics=নিরাপদ গ্রাফিক্স
hindi.SafeGraphics=सुरक्षित ग्राफ़िक्स
indonesian.SafeGraphics=grafis aman
urdu.SafeGraphics=محفوظ گرافکس
vietnamese.SafeGraphics=đồ họa an toàn

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Stage}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\icons\bk-cut-app.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\BK CUT"; Filename: "{app}\concat.exe"; IconFilename: "{app}\bk-cut-app.ico"
; The way in when a graphics driver draws the window as coloured squares:
; it turns off Settings > General > Draw on the GPU, which that window hides.
Name: "{autoprograms}\BK CUT ({cm:SafeGraphics})"; Filename: "{app}\concat.exe"; Parameters: "--software"; IconFilename: "{app}\bk-cut-app.ico"
Name: "{autodesktop}\BK CUT"; Filename: "{app}\concat.exe"; IconFilename: "{app}\bk-cut-app.ico"; Check: WantsBkDesktopShortcut

[InstallDelete]
; Only the known previous BK product shortcuts are retired. User projects,
; settings, credentials and the separately installed upstream app are retained.
Type: files; Name: "{autoprograms}\BK CAT.lnk"; Check: IsBkProductUpgrade
Type: files; Name: "{autoprograms}\BK CAT (safe graphics).lnk"; Check: IsBkProductUpgrade
Type: files; Name: "{autodesktop}\BK CAT.lnk"; Check: IsBkProductUpgrade

[Code]
var
  PreserveBkDesktopShortcut: Boolean;

function IsBkProductUpgrade: Boolean;
begin
  Result := RegKeyExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{A8DCE4CA-3E70-4263-9ED8-319D23851DC4}_is1') or
    RegKeyExists(HKLM, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{A8DCE4CA-3E70-4263-9ED8-319D23851DC4}_is1');
end;

procedure InitializeWizard;
begin
  PreserveBkDesktopShortcut := IsBkProductUpgrade and
    (FileExists(ExpandConstant('{autodesktop}\BK CUT.lnk')) or
     FileExists(ExpandConstant('{autodesktop}\BK CAT.lnk')));
end;

function WantsBkDesktopShortcut: Boolean;
begin
  Result := WizardIsTaskSelected('desktopicon') or PreserveBkDesktopShortcut;
end;

[Run]
Filename: "{app}\concat.exe"; Description: "{cm:LaunchProgram,BK CUT}"; Flags: nowait postinstall skipifsilent
