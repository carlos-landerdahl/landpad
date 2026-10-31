//! Integração com o Windows: escuta do clipboard, atalho global, colar e medição de memória.
//!
//! Tudo que espera mensagens do Windows roda numa thread própria com uma janela
//! invisível (HWND_MESSAGE). Nada fica consultando o clipboard em loop.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY_ID: i32 = 1;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// A janela que estava em foco quando o atalho foi apertado: é para ela que colamos.
#[derive(Clone, Copy, Default)]
pub struct Target(isize);

pub struct Handlers {
    pub on_clip: Box<dyn Fn(String, String) + Send>,
    pub on_hotkey: Box<dyn Fn(Instant, Target) + Send>,
    pub on_foreground: Box<dyn Fn(isize) + Send>,
}

struct Formats {
    /// Marca as cópias feitas pelo próprio Landpad, para não recapturá-las.
    own: u32,
    /// Formatos que gerenciadores de senha usam para pedir que a cópia não seja guardada.
    skip: [u32; 2],
}

static FORMATS: OnceLock<Formats> = OnceLock::new();

thread_local! {
    static HANDLERS: RefCell<Option<Handlers>> = const { RefCell::new(None) };
    static LAST_SEQ: Cell<u32> = const { Cell::new(0) };
}

/// Sobe a thread do Windows e devolve o atalho que conseguiu registrar.
pub fn start(handlers: Handlers) -> Result<Option<&'static str>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("landpad-win".into())
        .spawn(move || unsafe { run(handlers, tx) })
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

unsafe fn run(handlers: Handlers, ready: mpsc::Sender<Result<Option<&'static str>, String>>) {
    HANDLERS.with(|h| *h.borrow_mut() = Some(handlers));
    let result = setup();
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

unsafe fn setup() -> Result<Option<&'static str>, String> {
    FORMATS.get_or_init(|| Formats {
        own: RegisterClipboardFormatW(w!("Landpad.Own")),
        skip: [
            RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
            RegisterClipboardFormatW(w!("Clipboard Viewer Ignore")),
        ],
    });

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
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!("Landpad"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(instance.into()),
        None,
    )
    .map_err(err)?;

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

    // Ctrl+` como o Ditto; se outro app já usa, cai para Ctrl+Alt+V.
    let combos = [
        (MOD_CONTROL | MOD_NOREPEAT, VK_OEM_3.0 as u32, "Ctrl+`"),
        (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_V.0 as u32, "Ctrl+Alt+V"),
    ];
    for (mods, vk, label) in combos {
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, mods, vk).is_ok() {
            return Ok(Some(label));
        }
    }
    Ok(None)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
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
    HANDLERS.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            (h.on_foreground)(hwnd.0 as isize);
        }
    });
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
    if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
        return;
    }
    let Some(text) = read_text() else { return };
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        return;
    }
    let app = owner_app();
    HANDLERS.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            (h.on_clip)(text, app);
        }
    });
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

/// Escreve o texto no clipboard, devolve o foco ao app de origem e envia Ctrl+V.
pub fn paste(text: String, target: Target) {
    std::thread::spawn(move || unsafe {
        if let Err(e) = write_text(&text) {
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

unsafe fn write_text(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    if !open_clipboard() {
        return Err("clipboard ocupado por outro app".into());
    }
    let result = write_open_clipboard(&wide);
    let _ = CloseClipboard();
    result
}

unsafe fn write_open_clipboard(wide: &[u16]) -> Result<(), String> {
    EmptyClipboard().map_err(err)?;
    let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).map_err(err)?;
    let ptr = GlobalLock(mem) as *mut u16;
    if ptr.is_null() {
        return Err("GlobalLock falhou".into());
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
    let _ = GlobalUnlock(mem);
    SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))).map_err(err)?;
    if let Some(formats) = FORMATS.get() {
        if let Ok(marker) = GlobalAlloc(GMEM_MOVEABLE, 1) {
            let _ = SetClipboardData(formats.own, Some(HANDLE(marker.0)));
        }
    }
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

/// Onde abrir o popup: no cursor de texto se o app expõe o caret, senão no mouse.
/// Mantém a janela inteira dentro da área útil do monitor.
pub fn popup_origin(width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let (mut x, mut y) = (cursor.x + 4, cursor.y + 8);

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

        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            let work = mi.rcWork;
            if y + height > work.bottom {
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
