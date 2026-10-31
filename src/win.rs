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
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY_ID: i32 = 1;
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
        png: RegisterClipboardFormatW(w!("PNG")),
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
    // Texto tem prioridade: o Excel, por exemplo, copia células como texto e como imagem.
    let captured = if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
        let Some(text) = read_text() else { return };
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return;
        }
        Captured::Text(text)
    } else if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
        let Some(image) = read_image() else { return };
        image
    } else {
        return;
    };
    let app = owner_app();
    HANDLERS.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            (h.on_clip)(captured, app);
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
