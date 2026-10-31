//! Integração com o Windows: escuta do clipboard, atalhos globais, ícone na bandeja,
//! colar e medição de memória.
//!
//! Tudo que espera mensagens do Windows roda numa thread própria com uma janela oculta,
//! que nunca aparece. Nada fica consultando o clipboard em loop.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO,
    NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::logo;

const HOTKEY_ID: i32 = 1;
const SHOT_ID: i32 = 2;
/// Ctrl+Shift+1..9 usam os ids 10 a 18.
const QUICK_ID: i32 = 10;
/// Pedido da thread da interface para ligar (wparam 1) ou desligar os atalhos de número.
const WM_QUICK_KEYS: u32 = WM_APP + 1;
/// Pedido para registrar de novo os atalhos (wparam 1) ou soltá-los enquanto um atalho novo é gravado (0).
const WM_HOTKEYS: u32 = WM_APP + 2;
/// Cliques no ícone da bandeja.
const WM_TRAY: u32 = WM_APP + 3;
/// Redesenhar o ícone da bandeja (pausado ou não).
const WM_TRAY_REFRESH: u32 = WM_APP + 4;
/// Tirar o ícone da bandeja e encerrar a thread, ao fechar o app.
const WM_SHUTDOWN: u32 = WM_APP + 5;
/// Mostrar o balão guardado em BALLOON.
const WM_TRAY_BALLOON: u32 = WM_APP + 6;
/// Clique no balão da bandeja (NIN_BALLOONUSERCLICK), que o crate não exporta.
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
const TRAY_ID: u32 = 1;
/// Enter ou espaço no ícone da bandeja (NIN_SELECT | NINF_KEY), que o crate não exporta.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Um print 4K em 32 bits tem ~33 MB; acima disso não guardamos.
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

/// A janela que estava em foco quando o atalho foi apertado: é para ela que colamos.
#[derive(Clone, Copy, Default)]
pub struct Target(isize);

/// O que foi copiado, e o que colar de volta. Imagens vêm como DIB, o formato que todo
/// app do Windows entende, e também como PNG quando o app de origem oferece (preserva transparência).
pub enum Captured {
    Text(String),
    Image { dib: Vec<u8>, png: Option<Vec<u8>> },
}

pub struct Handlers {
    pub on_clip: Box<dyn Fn(Captured, String) + Send>,
    pub on_hotkey: Box<dyn Fn(Instant, Target) + Send>,
    pub on_foreground: Box<dyn Fn(isize) + Send>,
    /// Ctrl+Shift+número com o popup aberto; recebe 0 para o item 1.
    pub on_quick: Box<dyn Fn(usize) + Send>,
    pub on_shot: Box<dyn Fn() + Send>,
    pub on_tray: Box<dyn Fn(TrayCommand) + Send>,
    /// O Windows trocou entre claro e escuro.
    pub on_theme: Box<dyn Fn() + Send>,
    /// Resultado de cada registro de atalhos: na partida e depois de cada mudança.
    pub on_hotkeys: Box<dyn Fn(Hotkeys) + Send>,
}

/// O que ligar na partida. O `--bench` roda ao lado do app de verdade e não pode
/// disputar atalhos nem pôr um segundo ícone na bandeja.
pub struct Options {
    pub tray: bool,
    pub hotkeys: bool,
}

#[derive(Clone, Copy)]
pub enum TrayCommand {
    Open,
    /// "Atualizar agora" ou "Baixar a versão nova", pelo menu ou pelo balão.
    Update,
    Shot,
    Pause,
    Settings,
    Quit,
}

/// Um atalho global: modificadores do RegisterHotKey (MOD_CONTROL etc.) e a tecla virtual.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    /// Ctrl+` como o Ditto. No ABNT2 é a tecla à esquerda do 1, a do apóstrofo.
    pub const DEFAULT_POPUP: Hotkey = Hotkey { mods: MOD_CONTROL.0, vk: VK_OEM_3.0 as u32 };
    const FALLBACK_POPUP: Hotkey = Hotkey { mods: MOD_CONTROL.0 | MOD_ALT.0, vk: VK_V.0 as u32 };
    /// PrtSc como o Lightshot.
    pub const DEFAULT_SHOT: Hotkey = Hotkey { mods: 0, vk: VK_SNAPSHOT.0 as u32 };
    const FALLBACK_SHOT: Hotkey = Hotkey { mods: MOD_CONTROL.0, vk: VK_SNAPSHOT.0 as u32 };

    /// Como o atalho aparece para o usuário, com o nome da tecla no layout atual do teclado.
    pub fn label(self) -> String {
        let mut parts: Vec<String> = [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")]
            .iter()
            .filter(|(m, _)| self.mods & m.0 != 0)
            .map(|(_, name)| name.to_string())
            .collect();
        parts.push(key_name(self.vk));
        parts.join("+")
    }

    pub fn to_config(self) -> String {
        format!("{}:{}", self.mods, self.vk)
    }

    pub fn parse(s: &str) -> Option<Hotkey> {
        let (mods, vk) = s.split_once(':')?;
        Some(Hotkey { mods: mods.trim().parse().ok()?, vk: vk.trim().parse().ok()? })
    }

    /// F1–F24, PrtSc e Pause podem ser atalho sozinhos; o resto precisa de um modificador,
    /// senão a tecla deixaria de funcionar em todos os outros apps.
    pub fn is_valid(self) -> bool {
        self.mods != 0 || (0x70..=0x87).contains(&self.vk) || self.vk == VK_SNAPSHOT.0 as u32 || self.vk == VK_PAUSE.0 as u32
    }

    fn fallback(self) -> Option<Hotkey> {
        match self {
            Hotkey::DEFAULT_POPUP => Some(Hotkey::FALLBACK_POPUP),
            Hotkey::DEFAULT_SHOT => Some(Hotkey::FALLBACK_SHOT),
            _ => None,
        }
    }
}

fn key_name(vk: u32) -> String {
    let named = match vk {
        0x70..=0x87 => return format!("F{}", vk - 0x6F),
        0x60..=0x69 => return format!("Num {}", vk - 0x60),
        v if v == VK_SNAPSHOT.0 as u32 => "PrtSc",
        v if v == VK_SPACE.0 as u32 => "Espaço",
        v if v == VK_RETURN.0 as u32 => "Enter",
        v if v == VK_TAB.0 as u32 => "Tab",
        v if v == VK_BACK.0 as u32 => "Backspace",
        v if v == VK_INSERT.0 as u32 => "Insert",
        v if v == VK_DELETE.0 as u32 => "Delete",
        v if v == VK_HOME.0 as u32 => "Home",
        v if v == VK_END.0 as u32 => "End",
        v if v == VK_PRIOR.0 as u32 => "PgUp",
        v if v == VK_NEXT.0 as u32 => "PgDn",
        v if v == VK_PAUSE.0 as u32 => "Pause",
        v if v == VK_LEFT.0 as u32 => "←",
        v if v == VK_RIGHT.0 as u32 => "→",
        v if v == VK_UP.0 as u32 => "↑",
        v if v == VK_DOWN.0 as u32 => "↓",
        _ => "",
    };
    if !named.is_empty() {
        return named.into();
    }
    // Letras, números e pontuação: o caractere que a tecla produz no layout atual.
    let ch = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) } & 0xFFFF;
    match char::from_u32(ch).filter(|c| !c.is_control() && *c != '\0') {
        Some(c) => c.to_uppercase().collect(),
        None => format!("Tecla {vk}"),
    }
}

/// Os atalhos que ficaram ativos. Pode ser o alternativo, se o escolhido estava ocupado,
/// ou nenhum.
#[derive(Clone, Copy, Default)]
pub struct Hotkeys {
    pub popup: Option<Hotkey>,
    pub shot: Option<Hotkey>,
}

/// Onde o popup abre.
#[derive(Clone, Copy, PartialEq)]
pub enum Position {
    Caret,
    Mouse,
    Center,
}

struct Formats {
    /// Marca as cópias feitas pelo próprio Landpad, para não recapturá-las.
    own: u32,
    /// Formatos que gerenciadores de senha usam para pedir que a cópia não seja guardada.
    skip: [u32; 2],
    /// PNG registrado por navegadores, Office e ferramentas de print.
    png: u32,
}

static FORMATS: OnceLock<Formats> = OnceLock::new();
/// A janela oculta, para a thread da interface poder mandar pedidos para cá.
static MESSAGE_HWND: AtomicIsize = AtomicIsize::new(0);
/// Atalhos que a interface quer; a thread do Windows registra quando recebe WM_HOTKEYS.
static WANTED: Mutex<(Hotkey, Hotkey)> = Mutex::new((Hotkey::DEFAULT_POPUP, Hotkey::DEFAULT_SHOT));
/// Regras de captura, lidas a cada cópia.
static PAUSED: AtomicBool = AtomicBool::new(false);
static KEEP_IMAGES: AtomicBool = AtomicBool::new(true);
static IGNORED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Último app em primeiro plano que não é o Landpad nem a barra de tarefas: é para ele que
/// colamos quando o popup é aberto pela bandeja.
static LAST_APP: AtomicIsize = AtomicIsize::new(0);
/// Versão nova anunciada pelo GitHub e se ela já está baixada, pronta para instalar.
static UPDATE: Mutex<Option<(String, bool)>> = Mutex::new(None);
/// O balão a mostrar (título, texto) e o que um clique nele faz.
static BALLOON: Mutex<Option<(String, String, TrayCommand)>> = Mutex::new(None);
/// O que o clique no último balão mostrado faz.
static BALLOON_CLICK: Mutex<Option<TrayCommand>> = Mutex::new(None);

thread_local! {
    static HANDLERS: RefCell<Option<Handlers>> = const { RefCell::new(None) };
    static LAST_SEQ: Cell<u32> = const { Cell::new(0) };
    static ACTIVE: Cell<Hotkeys> = Cell::new(Hotkeys::default());
    static TRAY_ICON: Cell<Option<HICON>> = const { Cell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    /// Na thread da interface: quem recebe a tecla enquanto um atalho novo é gravado.
    static RECORDER: RefCell<Option<Box<dyn Fn(Option<Hotkey>)>>> = const { RefCell::new(None) };
}

/// Sobe a thread do Windows e devolve os atalhos que conseguiu registrar.
pub fn start(handlers: Handlers, options: Options, popup: Hotkey, shot: Hotkey) -> Result<Hotkeys, String> {
    *WANTED.lock().unwrap() = (popup, shot);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("landpad-win".into())
        .spawn(move || unsafe { run(handlers, options, tx) })
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

unsafe fn run(handlers: Handlers, options: Options, ready: mpsc::Sender<Result<Hotkeys, String>>) {
    HANDLERS.with(|h| *h.borrow_mut() = Some(handlers));
    let result = setup(&options);
    let ok = result.is_ok();
    let _ = ready.send(result);
    if !ok {
        return;
    }
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn setup(options: &Options) -> Result<Hotkeys, String> {
    FORMATS.get_or_init(|| Formats {
        own: RegisterClipboardFormatW(w!("Landpad.Own")),
        skip: [
            RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
            RegisterClipboardFormatW(w!("Clipboard Viewer Ignore")),
        ],
        png: RegisterClipboardFormatW(w!("PNG")),
    });

    allow_dark_menus();
    let instance = GetModuleHandleW(None).map_err(err)?;
    let class = w!("LandpadMessageWindow");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err("RegisterClassW falhou".into());
    }
    // Janela comum, só que nunca mostrada: uma HWND_MESSAGE não recebe o aviso de que o
    // Explorer reiniciou, e o ícone da bandeja sumiria.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class,
        w!("Landpad"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .map_err(err)?;
    MESSAGE_HWND.store(hwnd.0 as isize, Ordering::Release);

    AddClipboardFormatListener(hwnd).map_err(err)?;
    SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_FOREGROUND,
        None,
        Some(on_win_event),
        0,
        0,
        WINEVENT_OUTOFCONTEXT,
    );

    if options.tray {
        TASKBAR_CREATED.with(|c| c.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        tray_update(hwnd, true);
    }
    if options.hotkeys {
        Ok(register_hotkeys(hwnd))
    } else {
        Ok(Hotkeys::default())
    }
}

/// Registra os atalhos pedidos em WANTED. Se um padrão estiver ocupado (o PrtSc pelo
/// Lightshot, por exemplo), tenta o alternativo.
unsafe fn register_hotkeys(hwnd: HWND) -> Hotkeys {
    unregister_hotkeys(hwnd);
    let (popup, shot) = *WANTED.lock().unwrap();
    let register = |id: i32, want: Hotkey| {
        [Some(want), want.fallback()]
            .into_iter()
            .flatten()
            .find(|h| RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(h.mods) | MOD_NOREPEAT, h.vk).is_ok())
    };
    let active = Hotkeys { popup: register(HOTKEY_ID, popup), shot: register(SHOT_ID, shot) };
    ACTIVE.with(|a| a.set(active));
    active
}

unsafe fn unregister_hotkeys(hwnd: HWND) {
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), SHOT_ID);
}

/// Pede para registrar os atalhos de novo; o resultado chega em `on_hotkeys`.
pub fn set_hotkeys(popup: Hotkey, shot: Hotkey) {
    *WANTED.lock().unwrap() = (popup, shot);
    post(WM_HOTKEYS, 1);
}

/// Solta os atalhos globais enquanto um atalho novo é gravado; senão apertar o atalho
/// atual abriria o popup em vez de chegar à tela de configurações.
pub fn suspend_hotkeys() {
    post(WM_HOTKEYS, 0);
}

pub fn set_capture_rules(paused: bool, keep_images: bool, ignored: &[String]) {
    PAUSED.store(paused, Ordering::Release);
    KEEP_IMAGES.store(keep_images, Ordering::Release);
    *IGNORED.lock().unwrap() = ignored.iter().map(|a| a.to_lowercase()).collect();
    post(WM_TRAY_REFRESH, 0);
}

/// Para colar no app certo quando o popup vem da bandeja.
pub fn last_app() -> Target {
    Target(LAST_APP.load(Ordering::Acquire))
}

/// Tira o ícone da bandeja antes de o processo sair; senão ele fica lá até alguém passar o mouse.
pub fn shutdown() {
    let hwnd = MESSAGE_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            SendMessageW(HWND(hwnd as *mut c_void), WM_SHUTDOWN, None, None);
        }
    }
}

fn post(msg: u32, wparam: usize) {
    let hwnd = MESSAGE_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut c_void)), msg, WPARAM(wparam), LPARAM(0));
        }
    }
}

fn notify(f: impl FnOnce(&Handlers)) {
    HANDLERS.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            f(h);
        }
    });
}

/// Ícone na bandeja, desenhado na hora no tamanho certo para a escala da tela; fica cinza
/// com a captura pausada.
unsafe fn tray_update(hwnd: HWND, add: bool) {
    let paused = PAUSED.load(Ordering::Acquire);
    let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16) as u32;
    let icon = make_icon(size, paused);
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon.unwrap_or_default(),
        ..Default::default()
    };
    let tip = if paused { "Landpad (captura pausada)" } else { "Landpad" };
    for (dst, src) in nid.szTip.iter_mut().zip(tip.encode_utf16()) {
        *dst = src;
    }
    if add {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    } else {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
    if let Some(old) = TRAY_ICON.with(|t| t.replace(icon)) {
        let _ = DestroyIcon(old);
    }
}

/// Balão de aviso do ícone da bandeja (vira notificação no Windows 11).
unsafe fn tray_balloon(hwnd: HWND, title: &str, text: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    for (dst, src) in nid.szInfoTitle.iter_mut().take(63).zip(title.encode_utf16()) {
        *dst = src;
    }
    for (dst, src) in nid.szInfo.iter_mut().take(255).zip(text.encode_utf16()) {
        *dst = src;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Mostra um balão na bandeja; `click` é o que acontece se o usuário clicar nele.
/// Pode ser chamada de qualquer thread.
pub fn balloon(title: &str, text: &str, click: TrayCommand) {
    *BALLOON.lock().unwrap() = Some((title.to_string(), text.to_string(), click));
    post(WM_TRAY_BALLOON, 0);
}

/// Guarda a versão nova para o menu da bandeja e avisa com um balão.
pub fn announce_update(version: &str, ready: bool) {
    *UPDATE.lock().unwrap() = Some((version.to_string(), ready));
    if ready {
        balloon(
            &format!("Landpad {version} pronto"),
            "Instala sozinho na próxima vez que o Landpad abrir. Clique para atualizar agora (o histórico atual será apagado).",
            TrayCommand::Update,
        );
    } else {
        balloon(&format!("Landpad {version} disponível"), "Clique para abrir a página de download.", TrayCommand::Update);
    }
}

pub fn open_url(url: &str) {
    let url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(url.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Instalado pela Microsoft Store (MSIX): quem atualiza é a loja.
pub fn is_packaged() -> bool {
    let mut len = 0u32;
    unsafe { GetCurrentPackageFullName(&mut len, None) != APPMODEL_ERROR_NO_PACKAGE }
}

unsafe fn tray_remove(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

pub unsafe fn make_icon(size: u32, muted: bool) -> Option<HICON> {
    let rgba = logo::rgba(size, muted);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let bgra = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
    for (dst, src) in bgra.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
    }
    let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info).ok();
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    icon
}

const CMD_OPEN: usize = 1;
const CMD_SHOT: usize = 2;
const CMD_PAUSE: usize = 3;
const CMD_SETTINGS: usize = 4;
const CMD_QUIT: usize = 5;
const CMD_UPDATE: usize = 6;

unsafe fn tray_menu(hwnd: HWND, x: i32, y: i32) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let active = ACTIVE.with(Cell::get);
    let item = |text: &str, key: Option<Hotkey>| {
        let text = match key {
            Some(k) => format!("{text}\t{}", k.label()),
            None => text.to_string(),
        };
        text.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>()
    };
    let open = item("Abrir histórico", active.popup);
    let shot = item("Capturar a tela", active.shot);
    let pause = item("Pausar a captura", None);
    let settings = item("Configurações…", None);
    let quit = item("Sair", None);
    let paused = if PAUSED.load(Ordering::Acquire) { MF_CHECKED } else { MF_UNCHECKED };
    let update = UPDATE.lock().unwrap().as_ref().map(|(version, ready)| {
        let text = if *ready { format!("Atualizar para a {version} agora") } else { format!("Baixar a versão {version}…") };
        item(&text, None)
    });
    if let Some(update) = &update {
        let _ = AppendMenuW(menu, MF_STRING, CMD_UPDATE, PCWSTR(update.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    }
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, PCWSTR(open.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, CMD_SHOT, PCWSTR(shot.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING | paused, CMD_PAUSE, PCWSTR(pause.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, PCWSTR(settings.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, PCWSTR(quit.as_ptr()));
    let _ = SetMenuDefaultItem(menu, CMD_OPEN as u32, 0);
    // Sem isso o menu não fecha ao clicar fora (comportamento antigo e documentado do Windows).
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    let command = match cmd.0 as usize {
        CMD_OPEN => TrayCommand::Open,
        CMD_SHOT => TrayCommand::Shot,
        CMD_PAUSE => TrayCommand::Pause,
        CMD_SETTINGS => TrayCommand::Settings,
        CMD_QUIT => TrayCommand::Quit,
        CMD_UPDATE => TrayCommand::Update,
        _ => return,
    };
    notify(|h| (h.on_tray)(command));
}

/// Menus Win32 seguem o tema escuro do Windows só com essa função não documentada do
/// uxtheme (ordinal 135), a mesma que o Explorer e o Notepad++ usam. Se não existir, o
/// menu fica claro e nada quebra.
unsafe fn allow_dark_menus() {
    let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
    if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
        let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
        set_preferred_app_mode(1);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != 0 && msg == TASKBAR_CREATED.with(Cell::get) {
        tray_update(hwnd, true);
        return LRESULT(0);
    }
    match msg {
        WM_TRAY => {
            match (lparam.0 & 0xFFFF) as u32 {
                NIN_SELECT | NIN_KEYSELECT => notify(|h| (h.on_tray)(TrayCommand::Open)),
                NIN_BALLOONUSERCLICK => {
                    if let Some(command) = BALLOON_CLICK.lock().unwrap().take() {
                        notify(|h| (h.on_tray)(command));
                    }
                }
                WM_CONTEXTMENU => {
                    let (x, y) = ((wparam.0 & 0xFFFF) as u16 as i16 as i32, ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32);
                    tray_menu(hwnd, x, y);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY_REFRESH => {
            tray_update(hwnd, false);
            LRESULT(0)
        }
        WM_TRAY_BALLOON => {
            if let Some((title, text, click)) = BALLOON.lock().unwrap().take() {
                *BALLOON_CLICK.lock().unwrap() = Some(click);
                tray_balloon(hwnd, &title, &text);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE if lparam.0 != 0 => {
            if PCWSTR(lparam.0 as *const u16).to_string().is_ok_and(|s| s == "ImmersiveColorSet") {
                notify(|h| (h.on_theme)());
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_HOTKEYS => {
            if wparam.0 == 0 {
                unregister_hotkeys(hwnd);
            } else {
                let active = register_hotkeys(hwnd);
                notify(|h| (h.on_hotkeys)(active));
            }
            LRESULT(0)
        }
        WM_SHUTDOWN => {
            tray_remove(hwnd);
            if let Some(icon) = TRAY_ICON.with(|t| t.take()) {
                let _ = DestroyIcon(icon);
            }
            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            on_clipboard_update();
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 == HOTKEY_ID as usize => {
            let t0 = Instant::now();
            let target = Target(GetForegroundWindow().0 as isize);
            HANDLERS.with(|h| {
                if let Some(h) = h.borrow().as_ref() {
                    (h.on_hotkey)(t0, target);
                }
            });
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 == SHOT_ID as usize => {
            HANDLERS.with(|h| {
                if let Some(h) = h.borrow().as_ref() {
                    (h.on_shot)();
                }
            });
            LRESULT(0)
        }
        WM_HOTKEY if (QUICK_ID..QUICK_ID + 9).contains(&(wparam.0 as i32)) => {
            let n = wparam.0 - QUICK_ID as usize;
            HANDLERS.with(|h| {
                if let Some(h) = h.borrow().as_ref() {
                    (h.on_quick)(n);
                }
            });
            LRESULT(0)
        }
        WM_QUICK_KEYS => {
            set_quick_keys(hwnd, wparam.0 != 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Liga Ctrl+Shift+1..9 só enquanto o popup está aberto. Pelo Windows, e não pelo Slint,
/// porque com Shift o teclado manda "!", "@", "#"... e no ABNT2 o Shift+6 é o acento "¨".
pub fn quick_keys(on: bool) {
    let hwnd = MESSAGE_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut c_void)), WM_QUICK_KEYS, WPARAM(on as usize), LPARAM(0));
        }
    }
}

unsafe fn set_quick_keys(hwnd: HWND, on: bool) {
    let mut taken = Vec::new();
    for n in 0..9 {
        let id = QUICK_ID + n;
        if !on {
            let _ = UnregisterHotKey(Some(hwnd), id);
        } else if RegisterHotKey(Some(hwnd), id, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, '1' as u32 + n as u32).is_err() {
            taken.push(n + 1);
        }
    }
    if !taken.is_empty() {
        let keys: Vec<String> = taken.iter().map(|n| format!("Ctrl+Shift+{n}")).collect();
        eprintln!("[atalho] {} já está em uso por outro app", keys.join(", "));
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if is_other_app(hwnd) {
        LAST_APP.store(hwnd.0 as isize, Ordering::Release);
    }
    notify(|h| (h.on_foreground)(hwnd.0 as isize));
}

/// Uma janela de outro app, que não seja a barra de tarefas nem a área de notificação.
unsafe fn is_other_app(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == GetCurrentProcessId() {
        return false;
    }
    let mut class = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut class) as usize;
    let class = String::from_utf16_lossy(&class[..len]);
    !(class.starts_with("Shell_") || class.contains("Tray") || class.contains("Overflow"))
}

unsafe fn on_clipboard_update() {
    // Alguns apps disparam vários avisos para a mesma cópia.
    let seq = GetClipboardSequenceNumber();
    if LAST_SEQ.with(|s| s.replace(seq)) == seq {
        return;
    }
    let Some(formats) = FORMATS.get() else { return };
    if IsClipboardFormatAvailable(formats.own).is_ok() {
        return;
    }
    if formats.skip.iter().any(|&f| f != 0 && IsClipboardFormatAvailable(f).is_ok()) {
        return;
    }
    if PAUSED.load(Ordering::Acquire) {
        return;
    }
    // O app de origem é conferido antes de ler: nem chegamos a copiar o que vem de um app ignorado.
    let app = owner_app();
    if IGNORED.lock().unwrap().contains(&app.to_lowercase()) {
        return;
    }
    // Texto tem prioridade: o Excel, por exemplo, copia células como texto e como imagem.
    let captured = if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
        let Some(text) = read_text() else { return };
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return;
        }
        Captured::Text(text)
    } else if KEEP_IMAGES.load(Ordering::Acquire) && IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
        let Some(image) = read_image() else { return };
        image
    } else {
        return;
    };
    notify(|h| (h.on_clip)(captured, app));
}

/// OpenClipboard falha se outro app está com o clipboard aberto: tenta de novo algumas vezes.
unsafe fn open_clipboard() -> bool {
    for _ in 0..5 {
        if OpenClipboard(None).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

unsafe fn read_text() -> Option<String> {
    if !open_clipboard() {
        return None;
    }
    let text = read_open_clipboard_text();
    let _ = CloseClipboard();
    text
}

unsafe fn read_open_clipboard_text() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
    let mem = HGLOBAL(handle.0);
    let ptr = GlobalLock(mem) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let max = GlobalSize(mem) / 2;
    let mut len = 0;
    while len < max && *ptr.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    let _ = GlobalUnlock(mem);
    Some(text)
}

/// Copia os bytes e fecha o clipboard logo; miniatura e disco ficam para depois.
unsafe fn read_image() -> Option<Captured> {
    if !open_clipboard() {
        return None;
    }
    let dib = read_open_clipboard_bytes(CF_DIB.0 as u32);
    let png = match FORMATS.get() {
        Some(f) if f.png != 0 && IsClipboardFormatAvailable(f.png).is_ok() => read_open_clipboard_bytes(f.png),
        _ => None,
    };
    let _ = CloseClipboard();
    let dib = dib?;
    dib_info(&dib)?;
    Some(Captured::Image { dib, png })
}

unsafe fn read_open_clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    let handle = GetClipboardData(format).ok()?;
    let mem = HGLOBAL(handle.0);
    let size = GlobalSize(mem);
    if size == 0 || size > MAX_IMAGE_BYTES {
        return None;
    }
    let ptr = GlobalLock(mem) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
    let _ = GlobalUnlock(mem);
    Some(bytes)
}

/// Tamanho de uma imagem DIB e onde começam os pixels, depois do cabeçalho e da paleta.
pub struct DibInfo {
    pub width: u32,
    pub height: u32,
    pub bits_offset: usize,
}

pub fn dib_info(dib: &[u8]) -> Option<DibInfo> {
    let u32_at = |o: usize| Some(u32::from_le_bytes(dib.get(o..o + 4)?.try_into().ok()?));
    let header = u32_at(0)? as usize;
    let width = u32_at(4)? as i32;
    let height = (u32_at(8)? as i32).unsigned_abs();
    let bit_count = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = u32_at(16)?;
    let colors_used = u32_at(32)? as usize;
    if header < 40 || width <= 0 || height == 0 || (width as u64) * (height as u64) > 200_000_000 {
        return None;
    }
    // Com o cabeçalho básico, BI_BITFIELDS (3) e BI_ALPHABITFIELDS (6) trazem as máscaras logo depois.
    let masks = match (header, compression) {
        (40, 3) => 12,
        (40, 6) => 16,
        _ => 0,
    };
    let colors = if colors_used > 0 {
        colors_used
    } else if bit_count <= 8 {
        1 << bit_count
    } else {
        0
    };
    let bits_offset = header + masks + colors * 4;
    (bits_offset < dib.len()).then_some(DibInfo { width: width as u32, height, bits_offset })
}

/// Pixels RGBA de uma imagem reduzida.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Reduz um DIB com o filtro HALFTONE do GDI, que aceita qualquer profundidade de cor.
/// `fill` corta o centro para preencher `max_w`×`max_h`; sem ele, a imagem cabe inteira
/// e nunca é ampliada.
pub fn scale_dib(dib: &[u8], max_w: u32, max_h: u32, fill: bool) -> Option<Pixels> {
    let info = dib_info(dib)?;
    let (sw, sh) = (info.width as i64, info.height as i64);
    let (mut src_x, mut src_y, mut src_w, mut src_h) = (0, 0, sw, sh);
    let (dw, dh) = if fill {
        let (dw, dh) = (max_w.max(1) as i64, max_h.max(1) as i64);
        if sw * dh > sh * dw {
            src_w = (sh * dw / dh).max(1);
            src_x = (sw - src_w) / 2;
        } else {
            src_h = (sw * dh / dw).max(1);
            src_y = (sh - src_h) / 2;
        }
        (dw, dh)
    } else {
        let scale = (max_w as f64 / sw as f64).min(max_h as f64 / sh as f64).min(1.0);
        (((sw as f64 * scale).round() as i64).max(1), ((sh as f64 * scale).round() as i64).max(1))
    };
    let (dw, dh) = (dw as i32, dh as i32);

    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dw,
                biHeight: -dh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bitmap.into());
        SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
        let lines = StretchDIBits(
            dc,
            0,
            0,
            dw,
            dh,
            src_x as i32,
            src_y as i32,
            src_w as i32,
            src_h as i32,
            Some(dib.as_ptr().add(info.bits_offset) as *const c_void),
            dib.as_ptr() as *const BITMAPINFO,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        let _ = GdiFlush();
        let mut rgba = Vec::new();
        if lines > 0 && !bits.is_null() {
            let bgra = std::slice::from_raw_parts(bits as *const u8, dw as usize * dh as usize * 4);
            rgba.reserve_exact(bgra.len());
            for px in bgra.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        (!rgba.is_empty()).then_some(Pixels { width: dw as u32, height: dh as u32, rgba })
    }
}

/// Escala da tela principal (1,0 = 96 dpi), para gerar miniaturas sem reamostrar na hora de pintar.
pub fn scale() -> f32 {
    unsafe { GetDpiForSystem().max(96) as f32 / 96.0 }
}

/// Nome do .exe dono do clipboard (ou da janela em foco, se o dono não se identificar).
unsafe fn owner_app() -> String {
    let mut hwnd = GetClipboardOwner().unwrap_or_default();
    if hwnd.is_invalid() {
        hwnd = GetForegroundWindow();
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return "?".into();
    }
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return "?".into();
    };
    let mut buf = [0u16; 260];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).is_ok();
    let _ = CloseHandle(process);
    if !ok {
        return "?".into();
    }
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    let name = path.rsplit('\\').next().unwrap_or(&path);
    name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).to_string()
}

/// Escreve no clipboard, devolve o foco ao app de origem e envia Ctrl+V.
pub fn paste(clip: Captured, target: Target) {
    std::thread::spawn(move || unsafe {
        if let Err(e) = write_clipboard(&clip) {
            eprintln!("[colar] não consegui escrever no clipboard: {e}");
            return;
        }
        let hwnd = HWND(target.0 as *mut c_void);
        if !hwnd.is_invalid() {
            let _ = SetForegroundWindow(hwnd);
        }
        std::thread::sleep(Duration::from_millis(40));
        send_ctrl_v();
    });
}

/// Escreve no clipboard sem colar, marcado como cópia do Landpad (o histórico não recaptura).
pub fn set_clipboard(clip: &Captured) -> Result<(), String> {
    unsafe { write_clipboard(clip) }
}

unsafe fn write_clipboard(clip: &Captured) -> Result<(), String> {
    if !open_clipboard() {
        return Err("clipboard ocupado por outro app".into());
    }
    let result = write_open_clipboard(clip);
    let _ = CloseClipboard();
    result
}

unsafe fn write_open_clipboard(clip: &Captured) -> Result<(), String> {
    EmptyClipboard().map_err(err)?;
    let formats = FORMATS.get();
    match clip {
        Captured::Text(text) => {
            let wide: Vec<u8> = text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
            set_clipboard_bytes(CF_UNICODETEXT.0 as u32, &wide)?;
        }
        Captured::Image { dib, png } => {
            set_clipboard_bytes(CF_DIB.0 as u32, dib)?;
            if let (Some(png), Some(f)) = (png, formats) {
                let _ = set_clipboard_bytes(f.png, png);
            }
        }
    }
    if let Some(f) = formats {
        let _ = set_clipboard_bytes(f.own, &[0]);
    }
    Ok(())
}

unsafe fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(err)?;
    let ptr = GlobalLock(mem) as *mut u8;
    if ptr.is_null() {
        return Err("GlobalLock falhou".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(mem);
    SetClipboardData(format, Some(HANDLE(mem.0))).map_err(err)?;
    Ok(())
}

unsafe fn send_ctrl_v() {
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(VK_CONTROL, false), key(VK_V, false), key(VK_V, true), key(VK_CONTROL, true)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Onde abrir o popup: no cursor de texto (se o app expõe o caret; senão no mouse), no
/// mouse ou no centro do monitor do mouse. Mantém a janela inteira dentro da área útil.
pub fn popup_origin(width: i32, height: i32, position: Position) -> (i32, i32) {
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let (mut x, mut y) = (cursor.x + 4, cursor.y + 8);

        if position == Position::Caret {
            let foreground = GetForegroundWindow();
            let thread = GetWindowThreadProcessId(foreground, None);
            let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
            if GetGUIThreadInfo(thread, &mut info).is_ok() && !info.hwndCaret.is_invalid() {
                let mut caret = POINT { x: info.rcCaret.left, y: info.rcCaret.bottom };
                if ClientToScreen(info.hwndCaret, &mut caret).as_bool() {
                    x = caret.x;
                    y = caret.y + 4;
                }
            }
        }

        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            let work = mi.rcWork;
            if position == Position::Center {
                x = (work.left + work.right - width) / 2;
                y = (work.top + work.bottom - height) / 2;
            } else if y + height > work.bottom {
                y -= height + 28;
            }
            x = x.clamp(work.left, (work.right - width).max(work.left));
            y = y.clamp(work.top, (work.bottom - height).max(work.top));
        }
        (x, y)
    }
}

/// Cantos arredondados do Windows 11, fora da barra de tarefas e do Alt+Tab, e foco no popup.
pub fn prepare_popup(hwnd: isize, focus: bool) {
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let wanted = (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
        if wanted != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted);
        }
        if focus {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Memória do processo em MB: (privada, working set).
pub fn memory_mb() -> (f64, f64) {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let ok = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        )
        .is_ok();
        if !ok {
            return (0.0, 0.0);
        }
        const MB: f64 = 1024.0 * 1024.0;
        (counters.PrivateUsage as f64 / MB, counters.WorkingSetSize as f64 / MB)
    }
}

fn err(e: windows::core::Error) -> String {
    e.to_string()
}

/// Grava o próximo atalho apertado numa janela (a de configurações). As teclas chegam
/// como teclas virtuais, sem depender do layout: com Shift o Slint veria "!" em vez de "1".
/// `on_key` recebe None se o usuário apertar Esc. Roda na thread da interface.
pub fn start_recording(hwnd: isize, on_key: impl Fn(Option<Hotkey>) + 'static) {
    RECORDER.with(|r| *r.borrow_mut() = Some(Box::new(on_key)));
    unsafe {
        let _ = SetWindowSubclass(HWND(hwnd as *mut c_void), Some(record_proc), 1, 0);
    }
}

pub fn stop_recording(hwnd: isize) {
    unsafe {
        let _ = RemoveWindowSubclass(HWND(hwnd as *mut c_void), Some(record_proc), 1);
    }
    RECORDER.with(|r| r.borrow_mut().take());
}

unsafe extern "system" fn record_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let vk = wparam.0 as u32;
    // O Windows não manda o "apertou" do PrtSc para as janelas, só o "soltou".
    let pressed = match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => vk != VK_SNAPSHOT.0 as u32,
        WM_KEYUP | WM_SYSKEYUP => vk == VK_SNAPSHOT.0 as u32,
        _ => false,
    };
    let modifier = [VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|m| m.0 as u32 == vk);
    if !pressed || modifier {
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }
    let key = (vk != VK_ESCAPE.0 as u32).then(|| {
        let down = |k: VIRTUAL_KEY| GetKeyState(k.0 as i32) < 0;
        let mut mods = 0;
        if down(VK_CONTROL) {
            mods |= MOD_CONTROL.0;
        }
        if down(VK_MENU) {
            mods |= MOD_ALT.0;
        }
        if down(VK_SHIFT) {
            mods |= MOD_SHIFT.0;
        }
        if down(VK_LWIN) || down(VK_RWIN) {
            mods |= MOD_WIN.0;
        }
        Hotkey { mods, vk }
    });
    RECORDER.with(|r| {
        if let Some(on_key) = r.borrow().as_ref() {
            on_key(key);
        }
    });
    LRESULT(0)
}

/// O app não tem console próprio (não abre uma janela preta ao iniciar com o Windows).
/// Aberto de um terminal, pega emprestado o console dele para os logs continuarem aparecendo.
pub fn attach_console() {
    unsafe {
        let has_output = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if has_output || AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        if let Ok(out) = CreateFileW(
            w!("CONOUT$"),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, out);
            let _ = SetStdHandle(STD_ERROR_HANDLE, out);
        }
    }
}

/// Traz uma janela do app para a frente (restaurando se estiver minimizada).
///
/// O Windows só deixa trocar o primeiro plano quem já está nele; quando o pedido vem
/// depois do clique (a janela do Slint só aparece no primeiro quadro, e o popup já
/// sumiu), ele só pisca na barra de tarefas. Ligar por um instante a fila de entrada à
/// do app em primeiro plano faz o pedido valer.
pub fn bring_to_front(hwnd: isize) {
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return;
        }
        let theirs = GetWindowThreadProcessId(foreground, None);
        let ours = GetCurrentThreadId();
        let attached = theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(ours, theirs, false);
        }
    }
}

/// Mostra a janela já, sem esperar o primeiro quadro do Slint: enquanto o clique ainda é
/// recente, o Windows aceita trazê-la para a frente.
pub fn show_now(hwnd: isize) {
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut c_void), SW_SHOW);
    }
    bring_to_front(hwnd);
}

pub fn is_foreground(hwnd: isize) -> bool {
    unsafe { GetForegroundWindow().0 as isize == hwnd }
}

/// Os ícones que a janela informa ao Windows (grande, pequeno); 0 quando não tem.
pub fn window_icons(hwnd: isize) -> (isize, isize) {
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        let get = |kind: u32| SendMessageW(hwnd, WM_GETICON, Some(WPARAM(kind as usize)), None).0;
        (get(ICON_BIG), get(ICON_SMALL))
    }
}

/// Ícone na barra de título, na barra de tarefas e no Alt+Tab, no tamanho certo para a
/// escala da tela. Os ícones são feitos uma vez e reaproveitados.
pub fn set_window_icon(hwnd: isize) {
    thread_local! {
        static ICONS: Cell<Option<(HICON, HICON)>> = const { Cell::new(None) };
    }
    unsafe {
        let icons = ICONS.with(|c| {
            if c.get().is_none() {
                let dpi = GetDpiForSystem();
                let big = make_icon(GetSystemMetricsForDpi(SM_CXICON, dpi).max(32) as u32, false);
                let small = make_icon(GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16) as u32, false);
                if let (Some(big), Some(small)) = (big, small) {
                    c.set(Some((big, small)));
                }
            }
            c.get()
        });
        let Some((big, small)) = icons else { return };
        let hwnd = HWND(hwnd as *mut c_void);
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
    }
}

/// Garante um Landpad só: o segundo não conseguiria os atalhos nem o ícone da bandeja.
/// O mutex fica aberto até o processo sair.
pub fn first_instance() -> bool {
    unsafe {
        let created = CreateMutexW(None, true, w!("Local\\Landpad.Instance"));
        created.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}
