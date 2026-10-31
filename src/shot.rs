//! Captura de tela no estilo Lightshot: o atalho congela a tela, você arrasta uma região
//! (ou clica numa janela), anota se quiser e Enter copia. A captura vai para o clipboard e
//! para o histórico; dá também para fixá-la na tela ou copiar só o texto dela (OCR).
//!
//! O módulo só existe enquanto a captura está aberta: uma thread própria fotografa todos
//! os monitores, abre uma janela por cima deles e, ao terminar, libera tudo. O desenho é
//! GDI (e GDI+ nas anotações, pelo antisserrilhado), e só a parte da tela que mudou é
//! repintada a cada movimento do mouse: copiar a tela inteira por quadro ficaria pesado
//! em dois monitores 4K.

use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::*;
// Sem glob: o GDI+ exporta uma constante `Ok` que esconderia o Ok do Rust.
use windows::Win32::Graphics::GdiPlus::{
    CombineModeReplace, FillModeAlternate, GdipAddPathArcI, GdipClosePathFigure, GdipCreateAdjustableArrowCap,
    GdipCreateBitmapFromScan0, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush,
    GdipDeleteCustomLineCap, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDisposeImage, GdipDrawEllipseI,
    GdipDrawLineI, GdipDrawLinesI, GdipDrawRectangleI, GdipFillEllipseI, GdipFillPath, GdipSaveImageToFile,
    GdipSetClipRectI, GdipSetPenCustomEndCap, GdipSetPenEndCap, GdipSetPenLineJoin, GdipSetPenStartCap,
    GdipSetSmoothingMode, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpAdjustableArrowCap, GpBitmap, GpBrush,
    GpCustomLineCap, GpGraphics, GpImage, GpPath, GpPen, GpSolidFill, LineCapRound, LineJoinRound, Point,
    SmoothingModeAntiAlias, UnitPixel,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::{self, Captured};

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// A última captura salva com Ctrl+S, para o clique no aviso da bandeja abrir a pasta.
static LAST_SAVED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Clique no aviso "captura salva": mostra o arquivo no Explorer.
pub fn reveal_saved() {
    if let Some(path) = LAST_SAVED.lock().unwrap().clone() {
        win::reveal_in_explorer(&path);
    }
}

/// Avisa na bandeja como foi a gravação de um PNG em Imagens\Landpad e guarda o caminho
/// para o clique no aviso. Serve ao Ctrl+S da captura e ao "Salvar" do histórico.
fn announce_saved(result: Result<PathBuf, String>) {
    match result {
        Ok(path) => {
            println!("[print] salvo em {}", path.display());
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            win::balloon(
                "Imagem salva",
                &format!("{name} em Imagens\\Landpad. Clique para abrir a pasta."),
                win::TrayCommand::RevealSaved,
            );
            *LAST_SAVED.lock().unwrap() = Some(path);
        }
        Err(e) => {
            eprintln!("[print] não consegui salvar: {e}");
            win::balloon("Não consegui salvar a imagem", &e, win::TrayCommand::Open);
        }
    }
}

/// Grava uma imagem do histórico (um DIB do clipboard) como PNG em Imagens\Landpad, com
/// o mesmo aviso do Ctrl+S. Pode ser chamada de qualquer thread.
pub fn save_dib_to_pictures(dib: &[u8]) {
    let result = (|| {
        let info = win::dib_info(dib).ok_or("imagem inválida")?;
        let pixels = win::scale_dib(dib, info.width, info.height, false).ok_or("não consegui ler os pixels")?;
        unsafe {
            let _gdiplus = GdiPlus::start().ok_or("o GDI+ não iniciou")?;
            let mut surface =
                Surface::new(pixels.width as i32, pixels.height as i32).ok_or("não consegui criar a imagem")?;
            for (dst, src) in surface.pixels_mut().chunks_exact_mut(4).zip(pixels.rgba.chunks_exact(4)) {
                dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
            }
            save_png(&surface, None, PIXEL_FORMAT_32BPP_RGB)
        }
    })();
    announce_saved(result);
}

const PNG_ENCODER: GUID = GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);
const PIXEL_FORMAT_32BPP_RGB: i32 = 0x0002_2009;
const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

/// Cores das anotações, na ordem da paleta.
const COLORS: [(u8, u8, u8); 7] = [
    (0xE8, 0x41, 0x4B),
    (0xF5, 0xA6, 0x23),
    (0x2E, 0x63, 0xFF),
    (0x3B, 0xB9, 0x81),
    (0x8B, 0x5C, 0xF6),
    (0xFF, 0xFF, 0xFF),
    (0x14, 0x14, 0x14),
];
const ACCENT: (u8, u8, u8) = (0x2E, 0x63, 0xFF);
const WARNING: (u8, u8, u8) = (0xF5, 0xA6, 0x23);
const PANEL: (u8, u8, u8) = (0x20, 0x20, 0x20);
const PANEL_BORDER: (u8, u8, u8) = (0x40, 0x40, 0x40);
const PANEL_HOVER: (u8, u8, u8) = (0x3A, 0x3A, 0x3A);
const PANEL_TEXT: (u8, u8, u8) = (0xF0, 0xF0, 0xF0);
const OUTLINE: (u8, u8, u8) = (0x14, 0x14, 0x14);
/// Fora da seleção a tela fica escurecida: preto com esta opacidade por cima.
const SHADE_ALPHA: u8 = 145;

/// Id do timer que mostra a dica de um botão depois de o mouse parar sobre ele.
const TIMER_TOOLTIP: usize = 1;

type OnDone = Box<dyn FnOnce(Vec<u8>) + Send>;
/// Texto para o histórico e o nome do "app" de origem: "cor" ou "ocr".
type OnText = Box<dyn FnOnce(String, &'static str) + Send>;

/// Recortar uma região (PrtSc) ou pegar uma cor da tela (conta-gotas). Os dois congelam
/// a tela do mesmo jeito; na captura, a tecla C troca para o conta-gotas.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Shot,
    Color,
}

/// O conta-gotas mostra 13 × 13 pixels em volta do cursor, ampliados.
const LOUPE_CELLS: i32 = 13;

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
    static CLOSING: Cell<bool> = const { Cell::new(false) };
}

/// Abre a captura ou o conta-gotas numa thread própria; ignora o atalho se já houver um
/// aberto. `on_image` recebe a imagem recortada como DIB e `on_text` o texto (uma cor ou
/// o resultado do OCR) com o nome do app de origem, os dois já copiados para o clipboard.
pub fn open(
    mode: Mode,
    on_image: impl FnOnce(Vec<u8>) + Send + 'static,
    on_text: impl FnOnce(String, &'static str) + Send + 'static,
) {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("landpad-shot".into()).spawn(move || {
        unsafe { run(mode, Box::new(on_image), Box::new(on_text)) };
        ACTIVE.store(false, Ordering::Release);
    });
    if spawned.is_err() {
        ACTIVE.store(false, Ordering::Release);
    }
}

unsafe fn run(mode: Mode, on_done: OnDone, on_text: OnText) {
    // Coordenadas em pixels físicos em todos os monitores, mesmo com escalas diferentes.
    let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let t0 = Instant::now();
    let Some((origin, bright)) = capture_screen() else {
        eprintln!("[print] não consegui fotografar a tela");
        return;
    };
    let Some(_gdiplus) = GdiPlus::start() else {
        eprintln!("[print] o GDI+ não iniciou");
        return;
    };
    let (width, height) = (bright.width, bright.height);
    let Some(mut overlay) = Overlay::new(bright, Some(on_done)) else { return };
    let Some(hwnd) = create_window(origin, width, height) else {
        eprintln!("[print] não consegui abrir a janela de captura");
        return;
    };
    overlay.hwnd = hwnd;
    overlay.origin = origin;
    overlay.on_text = Some(on_text);
    // A tela está congelada: a lista de janelas vale até a captura fechar.
    overlay.windows = list_windows(origin, width, height);
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    overlay.hint_area = overlay.monitor_area(POINT { x: cursor.x - origin.x, y: cursor.y - origin.y });
    if mode == Mode::Color {
        overlay.start_color();
    }
    OVERLAY.with(|o| *o.borrow_mut() = Some(overlay));
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    println!("[print] tela de {width} × {height} congelada em {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    // Solta as cópias da tela antes de desligar o GDI+.
    OVERLAY.with(|o| o.borrow_mut().take());
}

/// Fotografa a área de trabalho inteira, todos os monitores juntos.
unsafe fn capture_screen() -> Option<(POINT, Surface)> {
    let origin = POINT { x: GetSystemMetrics(SM_XVIRTUALSCREEN), y: GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let (width, height) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
    let surface = Surface::new(width, height)?;
    let screen = GetDC(None);
    let ok = BitBlt(surface.dc, 0, 0, width, height, Some(screen), origin.x, origin.y, SRCCOPY | CAPTUREBLT).is_ok();
    ReleaseDC(None, screen);
    ok.then_some((origin, surface))
}

unsafe fn create_window(origin: POINT, width: i32, height: i32) -> Option<HWND> {
    let instance = GetModuleHandleW(None).ok()?;
    let class = w!("LandpadShot");
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        lpszClassName: class,
        ..Default::default()
    };
    // Na segunda captura a classe já existe e o registro falha; tudo bem.
    RegisterClassW(&wc);
    CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class,
        w!("Landpad captura"),
        WS_POPUP,
        origin.x,
        origin.y,
        width,
        height,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .ok()
}

/// As janelas de outros apps visíveis na hora da captura, da frente para trás, em
/// coordenadas da nossa janela. Um clique sem arrastar seleciona a primeira que contém o ponto.
unsafe fn list_windows(origin: POINT, width: i32, height: i32) -> Vec<RECT> {
    let me = GetCurrentProcessId();
    let screen = RECT { left: 0, top: 0, right: width, bottom: height };
    let mut out = Vec::new();
    let mut hwnd = GetTopWindow(None).unwrap_or_default();
    while !hwnd.is_invalid() {
        if is_app_window(hwnd, me) {
            if let Some(r) = window_bounds(hwnd) {
                let r = RECT { left: r.left - origin.x, top: r.top - origin.y, right: r.right - origin.x, bottom: r.bottom - origin.y };
                if let Some(r) = intersect(r, screen) {
                    out.push(r);
                }
            }
        }
        hwnd = GetWindow(hwnd, GW_HWNDNEXT).unwrap_or_default();
    }
    out
}

/// Uma janela de verdade de outro app: visível, não minimizada, não "cloaked" (as da
/// UWP em segundo plano ficam assim), sem ser janela de ferramenta nem a área de trabalho.
unsafe fn is_app_window(hwnd: HWND, me: u32) -> bool {
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == me {
        return false;
    }
    if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0 {
        return false;
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
    if cloaked != 0 {
        return false;
    }
    let mut class = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut class) as usize;
    let class = String::from_utf16_lossy(&class[..len]);
    !(class.starts_with("Shell_") || class == "Progman" || class == "WorkerW")
}

/// O retângulo da janela sem a sombra do Windows 11 (o DWM sabe; GetWindowRect inclui a sombra).
unsafe fn window_bounds(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    let ok = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32).is_ok()
        || GetWindowRect(hwnd, &mut r).is_ok();
    (ok && r.right > r.left && r.bottom > r.top).then_some(r)
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Arrow,
    Rect,
    Line,
    Marker,
    Blur,
    Text,
    Number,
}

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Tool(Tool),
    Color,
    Undo,
    Pin,
    Ocr,
    Save,
    Copy,
    Close,
}

const BUTTONS: [Button; 14] = [
    Button::Tool(Tool::Arrow),
    Button::Tool(Tool::Rect),
    Button::Tool(Tool::Line),
    Button::Tool(Tool::Marker),
    Button::Tool(Tool::Blur),
    Button::Tool(Tool::Text),
    Button::Tool(Tool::Number),
    Button::Color,
    Button::Undo,
    Button::Pin,
    Button::Ocr,
    Button::Save,
    Button::Copy,
    Button::Close,
];

impl Button {
    /// Glifo da Segoe Fluent Icons; vazio para os botões desenhados à mão (linha, número, cor).
    fn glyph(self) -> &'static str {
        match self {
            Button::Tool(Tool::Arrow) => "\u{E72A}",
            Button::Tool(Tool::Rect) => "\u{E739}",
            Button::Tool(Tool::Marker) => "\u{E7E6}",
            Button::Tool(Tool::Blur) => "\u{ED1A}",
            Button::Tool(Tool::Text) => "\u{E8D2}",
            Button::Tool(Tool::Line) | Button::Tool(Tool::Number) | Button::Color => "",
            Button::Undo => "\u{E7A7}",
            Button::Pin => "\u{E718}",
            Button::Ocr => "\u{E8C1}",
            Button::Save => "\u{E74E}",
            Button::Copy => "\u{E8C8}",
            Button::Close => "\u{E711}",
        }
    }

    /// A dica que aparece com o mouse parado sobre o botão.
    fn label(self) -> &'static str {
        match self {
            Button::Tool(Tool::Arrow) => "Seta (A)",
            Button::Tool(Tool::Rect) => "Retângulo (R)",
            Button::Tool(Tool::Line) => "Linha (L)",
            Button::Tool(Tool::Marker) => "Marca-texto (M)",
            Button::Tool(Tool::Blur) => "Borrar (B)",
            Button::Tool(Tool::Text) => "Texto (T)",
            Button::Tool(Tool::Number) => "Numeração (N)",
            Button::Color => "Cor (K troca)",
            Button::Undo => "Desfazer (Ctrl+Z)",
            Button::Pin => "Fixar na tela (P)",
            Button::Ocr => "Copiar o texto (O)",
            Button::Save => "Salvar PNG (Ctrl+S)",
            Button::Copy => "Copiar (Enter)",
            Button::Close => "Cancelar (Esc)",
        }
    }

    /// Começa um grupo novo na barra, com um separador antes.
    fn starts_group(self) -> bool {
        matches!(self, Button::Color | Button::Undo | Button::Pin)
    }
}

enum Shape {
    Arrow { from: POINT, to: POINT, color: usize },
    Rect { from: POINT, to: POINT, color: usize },
    Line { from: POINT, to: POINT, color: usize },
    Marker { points: Vec<POINT>, color: usize },
    Blur { from: POINT, to: POINT },
    Text { at: POINT, text: String, color: usize },
    Number { at: POINT, n: u32, color: usize },
}

impl Shape {
    fn start(tool: Tool, p: POINT, color: usize, n: u32) -> Shape {
        match tool {
            Tool::Arrow => Shape::Arrow { from: p, to: p, color },
            Tool::Rect => Shape::Rect { from: p, to: p, color },
            Tool::Line => Shape::Line { from: p, to: p, color },
            Tool::Marker => Shape::Marker { points: vec![p], color },
            Tool::Blur => Shape::Blur { from: p, to: p },
            Tool::Text => Shape::Text { at: p, text: String::new(), color },
            Tool::Number => Shape::Number { at: p, n, color },
        }
    }

    fn extend(&mut self, p: POINT) {
        match self {
            Shape::Arrow { to, .. } | Shape::Rect { to, .. } | Shape::Line { to, .. } | Shape::Blur { to, .. } => *to = p,
            Shape::Marker { points, .. } => points.push(p),
            Shape::Text { .. } | Shape::Number { .. } => {}
        }
    }

    /// Um clique sem arrastar (ou um texto em branco) não vira anotação.
    fn is_empty(&self) -> bool {
        match self {
            Shape::Arrow { from, to, .. } | Shape::Rect { from, to, .. } | Shape::Line { from, to, .. } | Shape::Blur { from, to } => {
                (from.x - to.x).abs() + (from.y - to.y).abs() < 4
            }
            Shape::Marker { points, .. } => points.len() < 2,
            Shape::Text { text, .. } => text.trim().is_empty(),
            Shape::Number { .. } => false,
        }
    }
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    Select(POINT),
    Move { start: POINT, from: RECT },
    /// Arrastando uma alça: `hx` e `hy` dizem que lado se move (-1 esquerda/cima,
    /// 1 direita/baixo, 0 fica como está).
    Resize { from: RECT, hx: i32, hy: i32 },
    Draw,
}

/// As oito alças da seleção, pelos cantos e pelo meio de cada lado.
const HANDLES: [(i32, i32); 8] = [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)];

/// Centro da alça `(hx, hy)` da seleção.
fn handle_center(sel: RECT, (hx, hy): (i32, i32)) -> POINT {
    let pick = |a: i32, b: i32, h: i32| match h {
        -1 => a,
        1 => b,
        _ => (a + b) / 2,
    };
    POINT { x: pick(sel.left, sel.right, hx), y: pick(sel.top, sel.bottom, hy) }
}

#[derive(Clone, Copy, PartialEq)]
enum Finish {
    Cancel,
    Copy,
    Save,
    /// Copia e ainda deixa a região numa janelinha por cima de tudo.
    Pin,
    /// Reconhece o texto da região e copia só ele.
    Ocr,
    /// Conta-gotas: copia a cor do pixel sob o cursor.
    PickHex,
    PickRgb,
}

/// Fontes das anotações, que vão também para a imagem final.
struct Fonts {
    /// Rótulos da ferramenta de texto.
    label: HFONT,
    /// O número dentro da bolinha da numeração.
    number: HFONT,
}

struct Overlay {
    hwnd: HWND,
    /// A tela como estava no atalho.
    bright: Surface,
    /// Um pixel preto: esticado com transparência, escurece o que está fora da seleção.
    shade: Surface,
    /// Onde a tela é montada antes de ir para a janela, para não piscar.
    back: Option<Surface>,
    scale: f32,
    sel: Option<RECT>,
    drag: Drag,
    tool: Option<Tool>,
    color: usize,
    shapes: Vec<Shape>,
    current: Option<Shape>,
    /// Um rótulo de texto está sendo digitado em `current`.
    editing: bool,
    hover: Option<Button>,
    /// A dica do botão sob o mouse está visível (o mouse parou sobre ele).
    tooltip: bool,
    /// A paleta de cores está aberta sobre a barra.
    palette: bool,
    /// Janelas de outros apps, da frente para trás, para o clique em uma selecioná-la.
    windows: Vec<RECT>,
    /// A janela sob o mouse enquanto não há seleção.
    hover_window: Option<RECT>,
    /// O monitor onde o mouse está (coordenadas da janela): é nele que a dica do topo aparece.
    hint_area: RECT,
    /// Aviso que fica embaixo da dica (OCR indisponível, por exemplo).
    notice: Option<String>,
    text_font: HFONT,
    icon_font: HFONT,
    fonts: Fonts,
    on_done: Option<OnDone>,
    on_text: Option<OnText>,
    mode: Mode,
    /// Canto da área de trabalho virtual, para converter coordenadas da janela em da tela.
    origin: POINT,
    /// Pixel sob o cursor, no conta-gotas.
    cursor: POINT,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.text_font.into());
            let _ = DeleteObject(self.icon_font.into());
            let _ = DeleteObject(self.fonts.label.into());
            let _ = DeleteObject(self.fonts.number.into());
        }
    }
}

impl Overlay {
    unsafe fn new(bright: Surface, on_done: Option<OnDone>) -> Option<Overlay> {
        let mut shade = Surface::new(1, 1)?;
        shade.pixels_mut().copy_from_slice(&[0, 0, 0, 255]);
        let scale = win::scale();
        let full = RECT { left: 0, top: 0, right: bright.width, bottom: bright.height };
        Some(Overlay {
            hwnd: HWND::default(),
            bright,
            shade,
            back: None,
            scale,
            sel: None,
            drag: Drag::None,
            tool: None,
            color: 0,
            shapes: Vec::new(),
            current: None,
            editing: false,
            hover: None,
            tooltip: false,
            palette: false,
            windows: Vec::new(),
            hover_window: None,
            hint_area: full,
            notice: None,
            text_font: font((12.0 * scale) as i32, 600, w!("Segoe UI Variable Text")),
            icon_font: font((16.0 * scale) as i32, 400, w!("Segoe Fluent Icons")),
            fonts: Fonts {
                label: font((16.0 * scale) as i32, 600, w!("Segoe UI Variable Text")),
                number: font((13.0 * scale) as i32, 700, w!("Segoe UI Variable Text")),
            },
            on_done,
            on_text: None,
            mode: Mode::Shot,
            origin: POINT::default(),
            cursor: POINT::default(),
        })
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn screen(&self) -> RECT {
        RECT { left: 0, top: 0, right: self.bright.width, bottom: self.bright.height }
    }

    fn clamp(&self, p: POINT) -> POINT {
        POINT { x: p.x.clamp(0, self.bright.width), y: p.y.clamp(0, self.bright.height) }
    }

    /// Repinta a tela inteira: só nas trocas de modo, que são raras.
    fn redraw(&self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    /// Marca uma região para repintar; o resto da tela fica como está.
    fn invalidate(&self, r: RECT) {
        if self.hwnd.is_invalid() {
            return;
        }
        if let Some(r) = intersect(r, self.screen()) {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), Some(&r), false);
            }
        }
    }

    /// Tudo que uma seleção desenha em volta de si: moldura, alças, etiqueta de tamanho e
    /// a barra com dicas e paleta. É o que precisa ser repintado quando a seleção muda.
    fn region_of(&self, sel: Option<RECT>) -> Option<RECT> {
        let sel = sel?;
        let mut r = inflate(sel, self.px(8.0));
        // Etiqueta de tamanho, acima do canto superior esquerdo (ou dentro, sem espaço).
        r.top = r.top.min(sel.top - self.px(32.0));
        r.bottom = r.bottom.max(sel.top + self.px(30.0));
        r.right = r.right.max(sel.left + self.px(140.0));
        if let Some((bar, _)) = self.toolbar_for(sel) {
            r = union(r, self.bar_region_of(bar));
        }
        Some(r)
    }

    /// A barra com folga para a dica em cima ou embaixo e para a paleta.
    fn bar_region_of(&self, bar: RECT) -> RECT {
        RECT { left: bar.left - self.px(110.0), top: bar.top - self.px(52.0), right: bar.right + self.px(110.0), bottom: bar.bottom + self.px(52.0) }
    }

    fn bar_region(&self) -> Option<RECT> {
        self.toolbar_for(self.sel?).map(|(bar, _)| self.bar_region_of(bar))
    }

    /// A faixa do topo do monitor onde ficam a dica e o aviso.
    fn hint_strip(&self, area: RECT) -> RECT {
        RECT { left: area.left, top: area.top, right: area.right, bottom: area.top + self.px(120.0) }
    }

    /// A seleção mudou de `before` para a atual: repinta o que as duas ocupavam.
    fn changed(&self, before: Option<RECT>) {
        if let Some(r) = self.region_of(before) {
            self.invalidate(r);
        }
        if let Some(r) = self.region_of(self.sel) {
            self.invalidate(r);
        }
        // A dica do topo só aparece sem seleção.
        if before.is_none() != self.sel.is_none() {
            self.invalidate(self.hint_strip(self.hint_area));
        }
    }

    /// O monitor que contém o ponto (coordenadas da janela), também em coordenadas da janela.
    unsafe fn monitor_area(&self, p: POINT) -> RECT {
        let screen_point = POINT { x: p.x + self.origin.x, y: p.y + self.origin.y };
        let monitor = MonitorFromPoint(screen_point, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            let m = mi.rcMonitor;
            RECT { left: m.left - self.origin.x, top: m.top - self.origin.y, right: m.right - self.origin.x, bottom: m.bottom - self.origin.y }
        } else {
            self.screen()
        }
    }

    /// Geometria da barra para uma seleção qualquer: embaixo dela, alinhada à direita; sem espaço, em cima.
    fn toolbar_for(&self, sel: RECT) -> Option<(RECT, Vec<(Button, RECT)>)> {
        let (b, pad, gap, margin) = (self.px(32.0), self.px(4.0), self.px(9.0), self.px(8.0));
        let groups = BUTTONS.iter().filter(|b| b.starts_group()).count() as i32;
        let width = 2 * pad + BUTTONS.len() as i32 * b + groups * gap;
        let height = b + 2 * pad;
        let left = (sel.right - width).clamp(0, (self.bright.width - width).max(0));
        let mut top = sel.bottom + margin;
        if top + height > self.bright.height {
            top = sel.top - height - margin;
        }
        if top < 0 {
            top = sel.bottom - height - margin;
        }
        let bar = RECT { left, top, right: left + width, bottom: top + height };
        let mut x = left + pad;
        let buttons = BUTTONS
            .iter()
            .map(|&button| {
                if button.starts_group() {
                    x += gap;
                }
                let r = RECT { left: x, top: top + pad, right: x + b, bottom: top + pad + b };
                x += b;
                (button, r)
            })
            .collect();
        Some((bar, buttons))
    }

    /// A barra de ferramentas como está agora: some enquanto a seleção é desenhada ou ajustada.
    fn toolbar(&self) -> Option<(RECT, Vec<(Button, RECT)>)> {
        if matches!(self.drag, Drag::Select(_) | Drag::Resize { .. }) {
            return None;
        }
        self.toolbar_for(self.sel?)
    }

    fn button_at(&self, p: POINT) -> Option<Button> {
        self.toolbar()?.1.into_iter().find(|(_, r)| contains(*r, p)).map(|(b, _)| b)
    }

    /// A paleta aberta: o painel e cada bolinha de cor. Fica acima da barra (ou abaixo, sem
    /// espaço), começando no botão de cor.
    fn palette_rects(&self) -> Option<(RECT, Vec<(usize, RECT)>)> {
        if !self.palette {
            return None;
        }
        let (bar, buttons) = self.toolbar()?;
        let anchor = buttons.iter().find(|(b, _)| *b == Button::Color).map(|(_, r)| *r)?;
        let (d, gap, pad, margin) = (self.px(22.0), self.px(6.0), self.px(7.0), self.px(6.0));
        let n = COLORS.len() as i32;
        let width = 2 * pad + n * d + (n - 1) * gap;
        let height = d + 2 * pad;
        let left = ((anchor.left + anchor.right - width) / 2).clamp(0, (self.bright.width - width).max(0));
        let mut top = bar.top - margin - height;
        if top < 0 {
            top = bar.bottom + margin;
        }
        let panel = RECT { left, top, right: left + width, bottom: top + height };
        let swatches = (0..COLORS.len())
            .map(|i| {
                let x = left + pad + i as i32 * (d + gap);
                (i, RECT { left: x, top: top + pad, right: x + d, bottom: top + pad + d })
            })
            .collect();
        Some((panel, swatches))
    }

    /// A alça sob o ponto, se houver seleção parada. A área de pegar é maior que o
    /// quadradinho desenhado, para não exigir pontaria.
    fn handle_at(&self, p: POINT) -> Option<(i32, i32)> {
        let sel = self.sel?;
        if !matches!(self.drag, Drag::None) {
            return None;
        }
        let reach = self.px(7.0);
        HANDLES.into_iter().find(|&h| {
            let c = handle_center(sel, h);
            (p.x - c.x).abs() <= reach && (p.y - c.y).abs() <= reach
        })
    }

    /// O próximo número da numeração: conta os que já estão na tela, então desfazer um
    /// reaproveita o número dele.
    fn next_number(&self) -> u32 {
        self.shapes.iter().filter(|s| matches!(s, Shape::Number { .. })).count() as u32 + 1
    }

    /// Fecha o rótulo que está sendo digitado; em branco, ele some.
    fn commit_text(&mut self) {
        if !self.editing {
            return;
        }
        self.editing = false;
        if let Some(shape) = self.current.take().filter(|s| !s.is_empty()) {
            self.shapes.push(shape);
        }
        self.invalidate_selection();
        self.invalidate(self.hint_strip(self.hint_area));
    }

    /// Repinta a seleção com uma folga: é onde as anotações aparecem.
    fn invalidate_selection(&self) {
        if let Some(sel) = self.sel {
            self.invalidate(inflate(sel, self.px(8.0)));
        }
    }

    fn invalidate_bar(&self) {
        if let Some(r) = self.bar_region() {
            self.invalidate(r);
        }
    }

    fn set_notice(&mut self, text: &str) {
        eprintln!("[print] {text}");
        self.notice = Some(text.to_string());
        self.invalidate(self.hint_strip(self.hint_area));
    }

    fn press(&mut self, button: Button) -> Option<Finish> {
        match button {
            Button::Tool(t) => {
                self.commit_text();
                self.tool = if self.tool == Some(t) { None } else { Some(t) };
                self.invalidate_bar();
            }
            Button::Color => {
                self.palette = !self.palette;
                self.tooltip = false;
                self.invalidate_bar();
            }
            Button::Undo => {
                self.commit_text();
                self.shapes.pop();
                self.invalidate_selection();
            }
            Button::Pin => return Some(Finish::Pin),
            Button::Ocr => {
                if let Err(e) = ocr_available() {
                    self.set_notice(&format!("OCR indisponível: {e}"));
                    return None;
                }
                return Some(Finish::Ocr);
            }
            Button::Save => return Some(Finish::Save),
            Button::Copy => return Some(Finish::Copy),
            Button::Close => return Some(Finish::Cancel),
        }
        None
    }

    fn mouse_down(&mut self, p: POINT) -> Option<Finish> {
        if self.mode == Mode::Color {
            self.cursor = self.clamp_pixel(p);
            return Some(if shift_down() { Finish::PickRgb } else { Finish::PickHex });
        }
        // Com a paleta aberta o clique só escolhe uma cor ou fecha a paleta.
        if let Some((_, swatches)) = self.palette_rects() {
            if let Some((i, _)) = swatches.into_iter().find(|(_, r)| contains(*r, p)) {
                self.color = i;
            }
            self.palette = false;
            self.invalidate_bar();
            return None;
        }
        self.commit_text();
        if let Some(button) = self.button_at(p) {
            return self.press(button);
        }
        if let (Some(sel), Some((hx, hy))) = (self.sel, self.handle_at(p)) {
            self.drag = Drag::Resize { from: sel, hx, hy };
            self.changed(self.sel);
            return None;
        }
        let before = self.sel;
        let p = self.clamp(p);
        match self.sel {
            Some(sel) if contains(sel, p) => match self.tool {
                Some(Tool::Text) => {
                    self.current = Some(Shape::start(Tool::Text, p, self.color, 0));
                    self.editing = true;
                    self.invalidate_selection();
                    // A dica do topo passa a explicar as teclas do texto.
                    self.invalidate(self.hint_strip(self.hint_area));
                }
                Some(Tool::Number) => {
                    let n = self.next_number();
                    self.shapes.push(Shape::start(Tool::Number, p, self.color, n));
                    self.invalidate_selection();
                }
                Some(tool) => {
                    self.current = Some(Shape::start(tool, p, self.color, 0));
                    self.drag = Drag::Draw;
                }
                None => self.drag = Drag::Move { start: p, from: sel },
            },
            // Clicar fora começa outra seleção, do zero.
            _ => {
                self.sel = None;
                self.shapes.clear();
                self.drag = Drag::Select(p);
                if let Some(w) = self.hover_window.take() {
                    self.invalidate(inflate(w, 3));
                }
                self.changed(before);
            }
        }
        None
    }

    fn mouse_move(&mut self, p: POINT) {
        if self.mode == Mode::Color {
            self.move_loupe(self.clamp_pixel(p));
            return;
        }
        let p = self.clamp(p);
        let before = self.sel;
        match self.drag {
            Drag::None => {
                unsafe { self.track_hover(p) };
                return;
            }
            Drag::Select(start) => self.sel = Some(rect(start, p)),
            Drag::Resize { from, hx, hy } => {
                // Só o lado da alça acompanha o mouse; `rect` reordena se ele cruzar o lado oposto.
                let mut r = from;
                match hx {
                    -1 => r.left = p.x,
                    1 => r.right = p.x,
                    _ => {}
                }
                match hy {
                    -1 => r.top = p.y,
                    1 => r.bottom = p.y,
                    _ => {}
                }
                self.sel = Some(rect(POINT { x: r.left, y: r.top }, POINT { x: r.right, y: r.bottom }));
            }
            Drag::Move { start, from } => {
                let (w, h) = (from.right - from.left, from.bottom - from.top);
                let left = (from.left + p.x - start.x).clamp(0, self.bright.width - w);
                let top = (from.top + p.y - start.y).clamp(0, self.bright.height - h);
                self.sel = Some(RECT { left, top, right: left + w, bottom: top + h });
            }
            Drag::Draw => {
                if let Some(shape) = &mut self.current {
                    shape.extend(p);
                }
                self.invalidate_selection();
                return;
            }
        }
        self.changed(before);
    }

    /// Mouse parado (sem arrastar): botão da barra sob ele, janela sob ele e monitor da dica.
    unsafe fn track_hover(&mut self, p: POINT) {
        let hover = self.button_at(p);
        if hover != self.hover {
            self.hover = hover;
            self.tooltip = false;
            if !self.hwnd.is_invalid() {
                let _ = KillTimer(Some(self.hwnd), TIMER_TOOLTIP);
                if hover.is_some() && !self.palette {
                    SetTimer(Some(self.hwnd), TIMER_TOOLTIP, 400, None);
                }
            }
            self.invalidate_bar();
        }
        if self.sel.is_none() {
            let window = self.windows.iter().copied().find(|w| contains(*w, p));
            if !same_rect(window, self.hover_window) {
                for w in [self.hover_window, window].into_iter().flatten() {
                    self.invalidate(inflate(w, 3));
                }
                self.hover_window = window;
            }
            let area = self.monitor_area(p);
            if !same_rect(Some(area), Some(self.hint_area)) {
                self.invalidate(self.hint_strip(self.hint_area));
                self.invalidate(self.hint_strip(area));
                self.hint_area = area;
            }
        }
    }

    /// O mouse parou sobre um botão tempo suficiente: mostra a dica dele.
    fn tooltip_timer(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_TOOLTIP);
            }
        }
        if self.hover.is_some() && !self.palette {
            self.tooltip = true;
            self.invalidate_bar();
        }
    }

    fn mouse_up(&mut self, p: POINT) {
        let before = self.sel;
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Select(start) => {
                if self.sel.is_some_and(|s| s.right - s.left < 4 || s.bottom - s.top < 4) {
                    // Um clique sem arrastar pega a janela que estava sob o mouse.
                    self.sel = self.windows.iter().copied().find(|w| contains(*w, start));
                    self.hover_window = None;
                }
            }
            Drag::Resize { .. } => {
                if self.sel.is_some_and(|s| s.right - s.left < 4 || s.bottom - s.top < 4) {
                    self.sel = None;
                }
            }
            Drag::Draw => {
                if let Some(shape) = self.current.take().filter(|s| !s.is_empty()) {
                    self.shapes.push(shape);
                }
                self.invalidate_selection();
            }
            _ => {}
        }
        self.changed(before);
        if self.sel.is_none() {
            unsafe { self.track_hover(self.clamp(p)) };
        }
    }

    fn key(&mut self, vk: u16) -> Option<Finish> {
        if self.mode == Mode::Color {
            return self.color_key(vk);
        }
        // Digitando um rótulo, as letras são texto; Enter e Esc chegam pelo WM_CHAR.
        if self.editing {
            return None;
        }
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let has_sel = self.sel.is_some();
        let tool = |t| Button::Tool(t);
        match vk {
            k if k == VK_ESCAPE.0 => {
                if self.palette {
                    self.palette = false;
                    self.invalidate_bar();
                    return None;
                }
                return Some(Finish::Cancel);
            }
            k if k == VK_RETURN.0 && has_sel => return Some(Finish::Copy),
            0x43 if ctrl && has_sel => return Some(Finish::Copy),
            0x53 if ctrl && has_sel => return Some(Finish::Save),
            0x5A if ctrl => {
                self.press(Button::Undo);
            }
            0x41 if has_sel && !ctrl => {
                self.press(tool(Tool::Arrow));
            }
            0x52 if has_sel && !ctrl => {
                self.press(tool(Tool::Rect));
            }
            0x4C if has_sel && !ctrl => {
                self.press(tool(Tool::Line));
            }
            0x4D if has_sel && !ctrl => {
                self.press(tool(Tool::Marker));
            }
            0x42 if has_sel && !ctrl => {
                self.press(tool(Tool::Blur));
            }
            0x54 if has_sel && !ctrl => {
                self.press(tool(Tool::Text));
            }
            0x4E if has_sel && !ctrl => {
                self.press(tool(Tool::Number));
            }
            // K troca a cor sem abrir a paleta.
            0x4B if has_sel && !ctrl => {
                self.color = (self.color + 1) % COLORS.len();
                self.invalidate_bar();
            }
            0x50 if has_sel && !ctrl => return Some(Finish::Pin),
            0x4F if has_sel && !ctrl => return self.press(Button::Ocr),
            // C troca a captura pelo conta-gotas.
            0x43 if !ctrl => self.start_color(),
            _ => {}
        }
        None
    }

    /// Caracteres digitados no rótulo de texto. Enter quebra a linha; Ctrl+Enter ou Ctrl+C
    /// concluem o rótulo e copiam a captura (quem quer seguir anotando clica em outro lugar,
    /// que também conclui); Esc descarta só o rótulo; Backspace apaga.
    fn char_input(&mut self, ch: u16) -> Option<Finish> {
        if !self.editing {
            return None;
        }
        match ch {
            // Ctrl+Enter chega como 0x0A e Ctrl+C como 0x03: concluem o rótulo e copiam a
            // captura. Ctrl+S (0x13) conclui e salva.
            0x0A | 0x03 => {
                self.commit_text();
                return Some(Finish::Copy);
            }
            0x13 => {
                self.commit_text();
                return Some(Finish::Save);
            }
            // Enter quebra a linha, como num editor; a legenda cresce para baixo.
            0x0D => {
                if let Some(Shape::Text { text, .. }) = &mut self.current {
                    text.push('\n');
                }
                self.invalidate_selection();
            }
            0x1B => {
                self.editing = false;
                self.current = None;
                self.invalidate_selection();
                self.invalidate(self.hint_strip(self.hint_area));
            }
            0x08 => {
                if let Some(Shape::Text { text, .. }) = &mut self.current {
                    text.pop();
                }
                self.invalidate_selection();
            }
            c if c >= 0x20 && !(0xD800..=0xDFFF).contains(&c) => {
                if let (Some(Shape::Text { text, .. }), Some(c)) = (&mut self.current, char::from_u32(c as u32)) {
                    text.push(c);
                }
                self.invalidate_selection();
            }
            _ => {}
        }
        None
    }

    /// Entra no conta-gotas, com a lupa onde o mouse está.
    fn start_color(&mut self) {
        self.commit_text();
        self.mode = Mode::Color;
        self.sel = None;
        self.shapes.clear();
        self.current = None;
        self.tool = None;
        self.drag = Drag::None;
        self.palette = false;
        self.hover_window = None;
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        self.cursor = self.clamp_pixel(POINT { x: p.x - self.origin.x, y: p.y - self.origin.y });
        self.redraw();
    }

    /// Enter copia a cor (Shift+Enter, em RGB); as setas andam 1 pixel (com Shift, 10).
    fn color_key(&mut self, vk: u16) -> Option<Finish> {
        let step = if shift_down() { 10 } else { 1 };
        let (dx, dy) = match vk {
            k if k == VK_ESCAPE.0 => return Some(Finish::Cancel),
            k if k == VK_RETURN.0 => return Some(if shift_down() { Finish::PickRgb } else { Finish::PickHex }),
            k if k == VK_LEFT.0 => (-step, 0),
            k if k == VK_RIGHT.0 => (step, 0),
            k if k == VK_UP.0 => (0, -step),
            k if k == VK_DOWN.0 => (0, step),
            _ => return None,
        };
        let p = self.clamp_pixel(POINT { x: self.cursor.x + dx, y: self.cursor.y + dy });
        // Move o ponteiro de verdade: assim o clique seguinte cai no mesmo pixel.
        unsafe {
            let _ = SetCursorPos(p.x + self.origin.x, p.y + self.origin.y);
        }
        self.move_loupe(p);
        None
    }

    /// Redesenha só onde a lupa estava e onde ela vai ficar.
    fn move_loupe(&mut self, p: POINT) {
        if p.x == self.cursor.x && p.y == self.cursor.y {
            return;
        }
        let (old, new) = (self.loupe_rect(self.cursor), self.loupe_rect(p));
        self.cursor = p;
        self.invalidate(old);
        self.invalidate(new);
    }

    fn clamp_pixel(&self, p: POINT) -> POINT {
        POINT { x: p.x.clamp(0, self.bright.width - 1), y: p.y.clamp(0, self.bright.height - 1) }
    }

    /// Cor do pixel na foto da tela, se ele estiver dentro dela.
    fn pixel(&self, x: i32, y: i32) -> Option<(u8, u8, u8)> {
        if x < 0 || y < 0 || x >= self.bright.width || y >= self.bright.height {
            return None;
        }
        let i = (y as usize * self.bright.width as usize + x as usize) * 4;
        let px = self.bright.pixels();
        Some((px[i + 2], px[i + 1], px[i]))
    }

    /// Largura da etiqueta do conta-gotas, que mostra "#2E63FF · rgb(46, 99, 255)".
    fn loupe_label_width(&self) -> i32 {
        self.px(236.0)
    }

    /// A lupa fica embaixo à direita do cursor e muda de lado perto das bordas. O retângulo
    /// inclui a etiqueta com os códigos da cor e uma margem para a moldura.
    fn loupe_rect(&self, at: POINT) -> RECT {
        let (left, top) = self.loupe_origin(at);
        let size = self.px(9.0) * LOUPE_CELLS;
        let height = size + self.px(6.0) + self.px(30.0);
        let label_left = self.loupe_label_left(left);
        let right = (left + size).max(label_left + self.loupe_label_width());
        RECT { left: left.min(label_left) - 3, top: top - 3, right: right + 3, bottom: top + height + 3 }
    }

    fn loupe_origin(&self, at: POINT) -> (i32, i32) {
        let size = self.px(9.0) * LOUPE_CELLS;
        let height = size + self.px(6.0) + self.px(30.0);
        let gap = self.px(22.0);
        let mut left = at.x + gap;
        let mut top = at.y + gap;
        if left + size > self.bright.width {
            left = at.x - gap - size;
        }
        if top + height > self.bright.height {
            top = at.y - gap - height;
        }
        (left, top)
    }

    /// A etiqueta é mais larga que a lupa: alinha pela esquerda e recua se bater na borda.
    fn loupe_label_left(&self, loupe_left: i32) -> i32 {
        loupe_left.min(self.bright.width - self.loupe_label_width()).max(0)
    }

    fn cursor_at(&self, p: POINT) -> PCWSTR {
        if self.mode == Mode::Color {
            IDC_CROSS
        } else if self.palette_rects().is_some_and(|(panel, _)| contains(panel, p)) || self.button_at(p).is_some() {
            IDC_HAND
        } else if let Some(handle) = self.resizing_handle(p) {
            match handle {
                (0, _) => IDC_SIZENS,
                (_, 0) => IDC_SIZEWE,
                (hx, hy) if hx == hy => IDC_SIZENWSE,
                _ => IDC_SIZENESW,
            }
        } else if self.sel.is_some_and(|s| contains(s, p)) && !matches!(self.drag, Drag::Select(_)) {
            match self.tool {
                None => IDC_SIZEALL,
                Some(Tool::Text) => IDC_IBEAM,
                Some(_) => IDC_CROSS,
            }
        } else if self.sel.is_none() && self.hover_window.is_some() {
            IDC_HAND
        } else {
            IDC_CROSS
        }
    }

    /// A alça em uso ou, parado, a que está sob o mouse: o cursor de redimensionar não
    /// pode mudar no meio do arrasto.
    fn resizing_handle(&self, p: POINT) -> Option<(i32, i32)> {
        match self.drag {
            Drag::Resize { hx, hy, .. } => Some((hx, hy)),
            Drag::None => self.handle_at(p),
            _ => None,
        }
    }

    /// A seleção com as anotações, do tamanho exato da região.
    unsafe fn render(&self, sel: RECT) -> Option<Surface> {
        let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
        let out = Surface::new(w, h)?;
        BitBlt(out.dc, 0, 0, w, h, Some(self.bright.dc), sel.left, sel.top, SRCCOPY).ok()?;
        draw_shapes(out.dc, &self.bright, self.shapes.iter(), POINT { x: sel.left, y: sel.top }, sel, self.scale, &self.fonts);
        Some(out)
    }

    /// Pinta a parte `dirty` da tela: fora da seleção, a foto escurecida; dentro, a foto
    /// como está, com as anotações. O resto (moldura, barra, dicas) é barato e é desenhado
    /// inteiro; o que cair fora de `dirty` não chega à janela.
    unsafe fn paint(&self, hdc: HDC, dirty: RECT) {
        let old_font = SelectObject(hdc, self.text_font.into());
        let (w, h) = (dirty.right - dirty.left, dirty.bottom - dirty.top);
        let _ = BitBlt(hdc, dirty.left, dirty.top, w, h, Some(self.bright.dc), dirty.left, dirty.top, SRCCOPY);
        if self.mode == Mode::Color {
            self.paint_hint(hdc, "Clique copia a cor · Shift+clique copia em RGB · Setas ajustam · Esc cancela", PANEL_TEXT, 0);
            self.paint_loupe(hdc);
        } else {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: SHADE_ALPHA, AlphaFormat: 0 };
            let _ = AlphaBlend(hdc, dirty.left, dirty.top, w, h, self.shade.dc, 0, 0, 1, 1, blend);
            match self.sel {
                None => {
                    if let Some(win) = self.hover_window {
                        // A janela sob o mouse aparece clara, como uma seleção que ainda não é.
                        if let Some(r) = intersect(win, dirty) {
                            let _ = BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(self.bright.dc), r.left, r.top, SRCCOPY);
                        }
                        frame(hdc, inflate(win, 2), OUTLINE);
                        frame(hdc, inflate(win, 1), ACCENT);
                    }
                    self.paint_hint(hdc, "Arraste para selecionar · Clique numa janela para pegá-la inteira · C pega uma cor · Esc cancela", PANEL_TEXT, 0);
                }
                Some(sel) => {
                    self.paint_selection(hdc, sel, dirty);
                    if self.editing {
                        self.paint_hint(hdc, "Enter quebra a linha · Ctrl+Enter ou Ctrl+C copia · Esc descarta o texto", PANEL_TEXT, 0);
                    }
                }
            }
            if let Some(notice) = &self.notice {
                self.paint_hint(hdc, notice, WARNING, 1);
            }
        }
        SelectObject(hdc, old_font);
    }

    /// A lupa: os pixels em volta do cursor ampliados numa grade, o do centro marcado, e
    /// embaixo a cor dele com os códigos em HEX e RGB.
    unsafe fn paint_loupe(&self, hdc: HDC) {
        let (left, top) = self.loupe_origin(self.cursor);
        let cell = self.px(9.0);
        let size = cell * LOUPE_CELLS;
        let half = LOUPE_CELLS / 2;
        // O fundo escuro aparece entre as células e vira a grade.
        fill_rect(hdc, RECT { left: left - 1, top: top - 1, right: left + size + 1, bottom: top + size + 1 }, OUTLINE);
        for dy in -half..=half {
            for dx in -half..=half {
                let color = self.pixel(self.cursor.x + dx, self.cursor.y + dy).unwrap_or(OUTLINE);
                let (x, y) = (left + (dx + half) * cell, top + (dy + half) * cell);
                fill_rect(hdc, RECT { left: x, top: y, right: x + cell - 1, bottom: y + cell - 1 }, color);
            }
        }
        let (cx, cy) = (left + half * cell, top + half * cell);
        frame(hdc, RECT { left: cx - 2, top: cy - 2, right: cx + cell + 1, bottom: cy + cell + 1 }, OUTLINE);
        frame(hdc, RECT { left: cx - 1, top: cy - 1, right: cx + cell, bottom: cy + cell }, (0xFF, 0xFF, 0xFF));

        let Some(color) = self.pixel(self.cursor.x, self.cursor.y) else { return };
        let label_left = self.loupe_label_left(left);
        let label = RECT {
            left: label_left,
            top: top + size + self.px(6.0),
            right: label_left + self.loupe_label_width(),
            bottom: top + size + self.px(36.0),
        };
        fill_round(hdc, label, PANEL, Some(PANEL_BORDER), self.px(8.0));
        let swatch = self.px(14.0);
        let sx = label.left + self.px(9.0);
        let sy = (label.top + label.bottom - swatch) / 2;
        fill_rect(hdc, RECT { left: sx, top: sy, right: sx + swatch, bottom: sy + swatch }, color);
        frame(hdc, RECT { left: sx, top: sy, right: sx + swatch, bottom: sy + swatch }, PANEL_TEXT);
        let text_area = RECT { left: sx + swatch, top: label.top, right: label.right, bottom: label.bottom };
        let codes = format!("{} · {}", crate::color::hex(color), crate::color::rgb(color));
        text(hdc, &codes, text_area, PANEL_TEXT);
    }

    /// Uma pílula centrada no topo do monitor do mouse; `row` 1 fica logo abaixo da primeira.
    unsafe fn paint_hint(&self, hdc: HDC, label: &str, color: (u8, u8, u8), row: i32) {
        let area = self.hint_area;
        let width = text_width(hdc, label) + self.px(32.0);
        let height = self.px(34.0);
        let left = (area.left + area.right - width) / 2;
        let top = area.top + self.px(28.0) + row * (height + self.px(8.0));
        let r = RECT { left, top, right: left + width, bottom: top + height };
        fill_round(hdc, r, PANEL, Some(PANEL_BORDER), height);
        text(hdc, label, r, color);
    }

    unsafe fn paint_selection(&self, hdc: HDC, sel: RECT, dirty: RECT) {
        let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
        if let Some(r) = intersect(sel, dirty) {
            let _ = BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(self.bright.dc), r.left, r.top, SRCCOPY);
        }
        draw_shapes(hdc, &self.bright, self.shapes.iter().chain(self.current.iter()), POINT::default(), sel, self.scale, &self.fonts);
        if self.editing {
            self.paint_caret(hdc, sel);
        }

        // Moldura branca com uma linha escura por fora, para aparecer em qualquer fundo.
        frame(hdc, inflate(sel, 2), OUTLINE);
        frame(hdc, inflate(sel, 1), (0xFF, 0xFF, 0xFF));

        // Alças de redimensionar: quadradinhos brancos com borda escura, nos cantos e no
        // meio dos lados. Somem enquanto a seleção está sendo desenhada.
        if !matches!(self.drag, Drag::Select(_)) {
            let half = self.px(4.0);
            for handle in HANDLES {
                let c = handle_center(sel, handle);
                let r = RECT { left: c.x - half, top: c.y - half, right: c.x + half + 1, bottom: c.y + half + 1 };
                fill_rect(hdc, r, (0xFF, 0xFF, 0xFF));
                frame(hdc, r, OUTLINE);
            }
        }

        let label = format!("{w} × {h}");
        let lw = text_width(hdc, &label) + self.px(16.0);
        let lh = self.px(22.0);
        let mut top = sel.top - lh - self.px(8.0);
        if top < 0 {
            top = sel.top + self.px(6.0);
        }
        let r = RECT { left: sel.left, top, right: sel.left + lw, bottom: top + lh };
        fill_round(hdc, r, PANEL, None, self.px(6.0));
        text(hdc, &label, r, PANEL_TEXT);

        let Some((bar, buttons)) = self.toolbar() else { return };
        fill_round(hdc, bar, PANEL, Some(PANEL_BORDER), self.px(10.0));
        let old_font = SelectObject(hdc, self.icon_font.into());
        let inset = self.px(2.0);
        for (button, r) in &buttons {
            let (button, r) = (*button, *r);
            let inner = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
            if matches!(button, Button::Tool(t) if Some(t) == self.tool) {
                fill_round(hdc, inner, ACCENT, None, self.px(6.0));
            } else if self.hover == Some(button) || (button == Button::Color && self.palette) {
                fill_round(hdc, inner, PANEL_HOVER, None, self.px(6.0));
            }
            if button.starts_group() {
                let x = r.left - self.px(5.0);
                fill_rect(hdc, RECT { left: x, top: r.top + self.px(7.0), right: x + 1, bottom: r.bottom - self.px(7.0) }, PANEL_BORDER);
            }
            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            match button {
                Button::Color => {
                    let d = self.px(14.0);
                    disc(hdc, cx, cy, d, COLORS[self.color], Some(PANEL_TEXT));
                }
                Button::Tool(Tool::Line) => {
                    let a = self.px(7.0);
                    aa_line(hdc, POINT { x: cx - a, y: cy + a }, POINT { x: cx + a, y: cy - a }, 2.0 * self.scale, PANEL_TEXT);
                }
                Button::Tool(Tool::Number) => {
                    let d = self.px(16.0);
                    disc(hdc, cx, cy, d, PANEL, Some(PANEL_TEXT));
                    let old = SelectObject(hdc, self.text_font.into());
                    text(hdc, "1", RECT { left: cx - d, top: cy - d, right: cx + d, bottom: cy + d }, PANEL_TEXT);
                    SelectObject(hdc, old);
                }
                _ => text(hdc, button.glyph(), r, PANEL_TEXT),
            }
        }
        SelectObject(hdc, old_font);

        // A dica do botão sob o mouse, em cima da barra (ou embaixo, quando a barra está no topo).
        if self.tooltip && !self.palette {
            if let Some((_, r)) = buttons.iter().find(|(b, _)| Some(*b) == self.hover) {
                let label = self.hover.map(Button::label).unwrap_or_default();
                let tw = text_width(hdc, label) + self.px(16.0);
                let th = self.px(24.0);
                let left = ((r.left + r.right - tw) / 2).clamp(0, (self.bright.width - tw).max(0));
                let mut top = bar.top - th - self.px(6.0);
                if top < 0 {
                    top = bar.bottom + self.px(6.0);
                }
                let tip = RECT { left, top, right: left + tw, bottom: top + th };
                fill_round(hdc, tip, PANEL, Some(PANEL_BORDER), self.px(6.0));
                text(hdc, label, tip, PANEL_TEXT);
            }
        }

        if let Some((panel, swatches)) = self.palette_rects() {
            fill_round(hdc, panel, PANEL, Some(PANEL_BORDER), self.px(10.0));
            for (i, r) in swatches {
                let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                let ring = if i == self.color { Some(PANEL_TEXT) } else { Some(PANEL_BORDER) };
                disc(hdc, cx, cy, r.right - r.left - self.px(4.0), COLORS[i], ring);
                if i == self.color {
                    disc(hdc, cx, cy, r.right - r.left, (0, 0, 0), Some(PANEL_TEXT));
                    disc(hdc, cx, cy, r.right - r.left - self.px(4.0), COLORS[i], None);
                }
            }
        }
    }

    /// A barrinha de inserção depois do texto que está sendo digitado.
    unsafe fn paint_caret(&self, hdc: HDC, sel: RECT) {
        let Some(Shape::Text { at, text: label, color }) = &self.current else { return };
        let old = SelectObject(hdc, self.fonts.label.into());
        let (lines, _, line_h) = caption_metrics(hdc, label);
        let last = lines.last().map(String::as_str).unwrap_or("");
        let width = text_width(hdc, last);
        SelectObject(hdc, old);
        // Depois da última linha, dentro da faixa da legenda.
        let (padx, pady) = caption_padding(self.scale);
        let x = at.x + padx + width + 1;
        let top = at.y + pady + (lines.len() as i32 - 1) * line_h;
        let r = RECT { left: x, top, right: x + self.px(2.0), bottom: top + line_h };
        if let Some(r) = intersect(r, sel) {
            fill_rect(hdc, r, COLORS[*color]);
        }
    }
}

/// Borrões primeiro, direto nos pixels; setas, retângulos, linhas, marca-texto e bolinhas
/// por cima, com GDI+; textos por último, com GDI. As formas estão em coordenadas da tela;
/// `offset` desloca para a superfície de destino e `clip` é a seleção.
unsafe fn draw_shapes<'a>(
    hdc: HDC,
    bright: &Surface,
    shapes: impl Iterator<Item = &'a Shape> + Clone,
    offset: POINT,
    clip: RECT,
    scale: f32,
    fonts: &Fonts,
) {
    for shape in shapes.clone() {
        if let Shape::Blur { from, to } = shape {
            if let Some(r) = intersect(rect(*from, *to), clip) {
                pixelate(hdc, bright, r, offset, scale);
            }
        }
    }

    let at = |p: POINT| (p.x - offset.x, p.y - offset.y);
    let local_clip = RECT { left: clip.left - offset.x, top: clip.top - offset.y, right: clip.right - offset.x, bottom: clip.bottom - offset.y };
    let number_d = (26.0 * scale) as i32;

    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    GdipSetClipRectI(g, local_clip.left, local_clip.top, local_clip.right - local_clip.left, local_clip.bottom - local_clip.top, CombineModeReplace);
    for shape in shapes.clone() {
        match shape {
            Shape::Arrow { from, to, color } => {
                let Some(pen) = Pen::new(argb(255, COLORS[*color]), 3.0 * scale) else { continue };
                GdipSetPenStartCap(pen.0, LineCapRound);
                let mut cap: *mut GpAdjustableArrowCap = std::ptr::null_mut();
                if GdipCreateAdjustableArrowCap(4.0, 4.0, true.into(), &mut cap).0 == 0 {
                    GdipSetPenCustomEndCap(pen.0, cap as *mut GpCustomLineCap);
                    GdipDeleteCustomLineCap(cap as *mut GpCustomLineCap);
                }
                let ((x1, y1), (x2, y2)) = (at(*from), at(*to));
                GdipDrawLineI(g, pen.0, x1, y1, x2, y2);
            }
            Shape::Line { from, to, color } => {
                let Some(pen) = Pen::new(argb(255, COLORS[*color]), 3.0 * scale) else { continue };
                GdipSetPenStartCap(pen.0, LineCapRound);
                GdipSetPenEndCap(pen.0, LineCapRound);
                let ((x1, y1), (x2, y2)) = (at(*from), at(*to));
                GdipDrawLineI(g, pen.0, x1, y1, x2, y2);
            }
            Shape::Rect { from, to, color } => {
                let Some(pen) = Pen::new(argb(255, COLORS[*color]), 3.0 * scale) else { continue };
                GdipSetPenLineJoin(pen.0, LineJoinRound);
                let r = rect(*from, *to);
                let (x, y) = at(POINT { x: r.left, y: r.top });
                GdipDrawRectangleI(g, pen.0, x, y, r.right - r.left, r.bottom - r.top);
            }
            Shape::Marker { points, color } => {
                let Some(pen) = Pen::new(argb(0x70, COLORS[*color]), 14.0 * scale) else { continue };
                GdipSetPenStartCap(pen.0, LineCapRound);
                GdipSetPenEndCap(pen.0, LineCapRound);
                GdipSetPenLineJoin(pen.0, LineJoinRound);
                let pts: Vec<Point> = points.iter().map(|&p| at(p)).map(|(x, y)| Point { X: x, Y: y }).collect();
                GdipDrawLinesI(g, pen.0, pts.as_ptr(), pts.len() as i32);
            }
            Shape::Number { at: p, color, .. } => {
                // Bolinha cheia com um anel claro, para destacar sobre qualquer fundo.
                let (x, y) = at(*p);
                let mut brush: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(argb(255, COLORS[*color]), &mut brush).0 == 0 {
                    GdipFillEllipseI(g, brush as *mut GpBrush, x - number_d / 2, y - number_d / 2, number_d, number_d);
                    GdipDeleteBrush(brush as *mut GpBrush);
                }
                if let Some(pen) = Pen::new(argb(255, (0xFF, 0xFF, 0xFF)), 2.0 * scale) {
                    GdipDrawEllipseI(g, pen.0, x - number_d / 2, y - number_d / 2, number_d, number_d);
                }
            }
            Shape::Blur { .. } | Shape::Text { .. } => {}
        }
    }
    GdipDeleteGraphics(g);

    // Textos com GDI (ClearType), recortados à seleção pelo clip do DC.
    let saved = SaveDC(hdc);
    IntersectClipRect(hdc, local_clip.left, local_clip.top, local_clip.right, local_clip.bottom);
    SetBkMode(hdc, TRANSPARENT);
    for shape in shapes {
        match shape {
            Shape::Text { at: p, text: label, color } => {
                let (x, y) = at(*p);
                SelectObject(hdc, fonts.label.into());
                // Estilo legenda: o texto em cor sólida sobre uma faixa arredondada
                // semitransparente, escura (ou clara, quando o texto é preto). Fica legível em
                // qualquer fundo sem o serrilhado de um contorno.
                let (lines, w, line_h) = caption_metrics(hdc, label);
                let (padx, pady) = caption_padding(scale);
                let h = line_h * lines.len() as i32;
                let pill = RECT { left: x, top: y, right: x + w + 2 * padx, bottom: y + h + 2 * pady };
                let dark_text = *color == COLORS.len() - 1;
                let fill = if dark_text { argb(0xD4, (0xFF, 0xFF, 0xFF)) } else { argb(0xB4, PANEL) };
                fill_round_alpha(hdc, pill, (6.0 * scale) as i32, fill);
                for (i, line) in lines.iter().enumerate() {
                    draw_text_at(hdc, line, x + padx, y + pady + i as i32 * line_h, COLORS[*color]);
                }
            }
            Shape::Number { at: p, n, color } => {
                let (x, y) = at(*p);
                SelectObject(hdc, fonts.number.into());
                let ink = if *color == COLORS.len() - 2 { OUTLINE } else { (0xFF, 0xFF, 0xFF) };
                text(hdc, &n.to_string(), RECT { left: x - number_d, top: y - number_d, right: x + number_d, bottom: y + number_d }, ink);
            }
            _ => {}
        }
    }
    let _ = RestoreDC(hdc, saved);
}

/// Pixela a região em blocos com a cor média de cada um: esconde texto sem borrar a imagem toda.
unsafe fn pixelate(hdc: HDC, bright: &Surface, r: RECT, offset: POINT, scale: f32) {
    let block = ((10.0 * scale) as i32).max(6);
    let px = bright.pixels();
    let stride = bright.width as usize * 4;
    let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    let mut y = r.top;
    while y < r.bottom {
        let y2 = (y + block).min(r.bottom);
        let mut x = r.left;
        while x < r.right {
            let x2 = (x + block).min(r.right);
            let (mut sum, mut n) = ([0u32; 3], 0u32);
            for yy in y..y2 {
                let row = yy as usize * stride;
                for xx in x..x2 {
                    let i = row + xx as usize * 4;
                    sum[0] += px[i] as u32;
                    sum[1] += px[i + 1] as u32;
                    sum[2] += px[i + 2] as u32;
                    n += 1;
                }
            }
            let (b, g, red) = (sum[0] / n, sum[1] / n, sum[2] / n);
            SetDCBrushColor(hdc, COLORREF(red | g << 8 | b << 16));
            FillRect(hdc, &RECT { left: x - offset.x, top: y - offset.y, right: x2 - offset.x, bottom: y2 - offset.y }, brush);
            x = x2;
        }
        y = y2;
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = POINT { x: (lparam.0 & 0xFFFF) as u16 as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32 };
    let finish = match msg {
        WM_PAINT => {
            paint_window(hwnd);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let _ = ScreenToClient(hwnd, &mut p);
            let id = with_overlay(|o| o.cursor_at(p)).unwrap_or(IDC_CROSS);
            if let Ok(cursor) = LoadCursorW(None, id) {
                SetCursor(Some(cursor));
            }
            return LRESULT(1);
        }
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            with_overlay(|o| o.mouse_down(point)).flatten()
        }
        WM_MOUSEMOVE => {
            with_overlay(|o| o.mouse_move(point));
            None
        }
        WM_LBUTTONUP => {
            with_overlay(|o| o.mouse_up(point));
            let _ = ReleaseCapture();
            None
        }
        // Duplo clique dentro da seleção copia; com uma ferramenta ativa, é só mais um clique
        // (dois números seguidos, por exemplo).
        WM_LBUTTONDBLCLK => with_overlay(|o| {
            if o.mode == Mode::Shot && o.tool.is_none() && o.sel.is_some_and(|s| contains(s, point)) {
                return Some(Finish::Copy);
            }
            SetCapture(hwnd);
            o.mouse_down(point)
        })
        .flatten(),
        WM_RBUTTONUP => Some(Finish::Cancel),
        WM_KEYDOWN => with_overlay(|o| o.key(wparam.0 as u16)).flatten(),
        WM_CHAR => with_overlay(|o| o.char_input(wparam.0 as u16)).flatten(),
        WM_TIMER if wparam.0 == TIMER_TOOLTIP => {
            with_overlay(|o| o.tooltip_timer());
            None
        }
        // Alt+Tab ou clique em outra janela: a captura não pode ficar presa por cima de tudo.
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => Some(Finish::Cancel),
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    };
    if let Some(how) = finish {
        finish_capture(hwnd, how);
    }
    LRESULT(0)
}

/// Acesso ao estado da captura; devolve None em vez de entrar em pânico se o Windows
/// chamar a janela de novo no meio de outra mensagem.
fn with_overlay<R>(f: impl FnOnce(&mut Overlay) -> R) -> Option<R> {
    OVERLAY.with(|o| {
        let mut guard = o.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

unsafe fn paint_window(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    with_overlay(|o| {
        if o.back.is_none() {
            o.back = Surface::new(o.bright.width, o.bright.height);
        }
        match &o.back {
            Some(back) => {
                let r = ps.rcPaint;
                o.paint(back.dc, r);
                let _ = BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(back.dc), r.left, r.top, SRCCOPY);
            }
            None => o.paint(hdc, ps.rcPaint),
        }
    });
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn finish_capture(hwnd: HWND, how: Finish) {
    // Esconder a janela a desativa, o que chamaria esta função de novo.
    if CLOSING.with(|c| c.replace(true)) {
        return;
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
    if matches!(how, Finish::PickHex | Finish::PickRgb) {
        pick_color(how);
        let _ = DestroyWindow(hwnd);
        return;
    }
    let done = with_overlay(|o| {
        if how == Finish::Cancel {
            return None;
        }
        o.commit_text();
        let sel = o.sel?;
        let image = o.render(sel)?;
        let at = POINT { x: sel.left + o.origin.x, y: sel.top + o.origin.y };
        Some((image, at, o.on_done.take(), o.on_text.take()))
    })
    .flatten();

    if let Some((image, at, on_done, on_text)) = done {
        let (w, h) = (image.width, image.height);
        if how == Finish::Ocr {
            // O reconhecimento leva um instante: roda à parte, com a captura já fechada.
            let pixels = image.pixels().to_vec();
            std::thread::spawn(move || match recognize(pixels, w, h) {
                Ok(text) if text.trim().is_empty() => {
                    eprintln!("[ocr] nenhum texto reconhecido na região");
                    win::balloon("Nenhum texto encontrado", "O OCR não reconheceu texto na região selecionada.", win::TrayCommand::Open);
                }
                Ok(text) => {
                    match win::set_clipboard(&Captured::Text(text.clone())) {
                        Ok(()) => println!("[ocr] {} caracteres copiados", text.chars().count()),
                        Err(e) => eprintln!("[ocr] não consegui copiar: {e}"),
                    }
                    if let Some(on_text) = on_text {
                        on_text(text, "ocr");
                    }
                }
                Err(e) => {
                    eprintln!("[ocr] {e}");
                    win::balloon("O OCR falhou", &e, win::TrayCommand::Open);
                }
            });
            let _ = DestroyWindow(hwnd);
            return;
        }
        if how == Finish::Save {
            announce_saved(save_png(&image, None, PIXEL_FORMAT_32BPP_RGB));
        }
        if how == Finish::Pin {
            pin_to_screen(image.pixels().to_vec(), w, h, at);
        }
        let clip = Captured::Image { dib: image.to_dib(), png: None };
        match win::set_clipboard(&clip) {
            Ok(()) => println!("[print] {w} × {h} copiado"),
            Err(e) => eprintln!("[print] não consegui copiar: {e}"),
        }
        if let (Some(on_done), Captured::Image { dib, .. }) = (on_done, clip) {
            on_done(dib);
        }
    }
    let _ = DestroyWindow(hwnd);
}

/// Copia a cor sob o cursor, em HEX ou RGB, e manda para o histórico.
unsafe fn pick_color(how: Finish) {
    let picked = with_overlay(|o| {
        let color = o.pixel(o.cursor.x, o.cursor.y)?;
        let text = if how == Finish::PickRgb { crate::color::rgb(color) } else { crate::color::hex(color) };
        Some((text, o.on_text.take()))
    })
    .flatten();
    let Some((text, on_text)) = picked else { return };
    match win::set_clipboard(&Captured::Text(text.clone())) {
        Ok(()) => println!("[cor] {text} copiado"),
        Err(e) => eprintln!("[cor] não consegui copiar: {e}"),
    }
    if let Some(on_text) = on_text {
        on_text(text, "cor");
    }
}

fn shift_down() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

// ---------------------------------------------------------------------------------------
// OCR com o motor do próprio Windows (Windows.Media.Ocr): offline, nos idiomas instalados.

/// Confere se há um motor de OCR para os idiomas do usuário, antes de fechar a captura.
fn ocr_available() -> Result<(), String> {
    windows::Media::Ocr::OcrEngine::TryCreateFromUserProfileLanguages()
        .map(|_| ())
        .map_err(|_| "instale um pacote de idioma em Configurações > Hora e idioma".to_string())
}

/// Reconhece o texto de uma imagem BGRA de cima para baixo e devolve as linhas, uma por linha.
fn recognize(mut pixels: Vec<u8>, width: i32, height: i32) -> Result<String, String> {
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;

    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| format!("o OCR do Windows não está disponível ({e})"))?;
    // O motor tem um tamanho máximo de imagem; acima dele, reduzimos antes.
    let max = OcrEngine::MaxImageDimension().unwrap_or(2600) as i32;
    let (pixels, width, height) = if width > max || height > max {
        unsafe { shrink(&pixels, width, height, max).ok_or("não consegui reduzir a imagem para o OCR")? }
    } else {
        for px in pixels.chunks_exact_mut(4) {
            px[3] = 255;
        }
        (pixels, width, height)
    };
    let writer = DataWriter::new().map_err(|e| e.to_string())?;
    writer.WriteBytes(&pixels).map_err(|e| e.to_string())?;
    let buffer = writer.DetachBuffer().map_err(|e| e.to_string())?;
    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(&buffer, BitmapPixelFormat::Bgra8, width, height, BitmapAlphaMode::Ignore)
        .map_err(|e| e.to_string())?;
    let result = engine.RecognizeAsync(&bitmap).map_err(|e| e.to_string())?.get().map_err(|e| e.to_string())?;
    let lines = result.Lines().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for line in &lines {
        if let Ok(text) = line.Text() {
            out.push(text.to_string());
        }
    }
    Ok(out.join("\n"))
}

/// Reduz uma imagem BGRA para caber em `max` × `max`, com o filtro HALFTONE do GDI.
unsafe fn shrink(pixels: &[u8], width: i32, height: i32, max: i32) -> Option<(Vec<u8>, i32, i32)> {
    let scale = max as f64 / width.max(height) as f64;
    let (dw, dh) = (((width as f64 * scale) as i32).max(1), ((height as f64 * scale) as i32).max(1));
    let mut src = Surface::new(width, height)?;
    src.pixels_mut().copy_from_slice(pixels);
    let dst = Surface::new(dw, dh)?;
    SetStretchBltMode(dst.dc, HALFTONE);
    let _ = SetBrushOrgEx(dst.dc, 0, 0, None);
    if !StretchBlt(dst.dc, 0, 0, dw, dh, Some(src.dc), 0, 0, width, height, SRCCOPY).as_bool() {
        return None;
    }
    let mut out = dst.pixels().to_vec();
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some((out, dw, dh))
}

// ---------------------------------------------------------------------------------------
// Fixar na tela: a região capturada vira uma janelinha por cima de tudo, no lugar de origem.

struct Pinned {
    surface: Surface,
    alpha: u8,
}

thread_local! {
    static PINNED: RefCell<Option<Pinned>> = const { RefCell::new(None) };
}

/// Abre a janela fixada numa thread própria, que vive depois de a captura fechar.
fn pin_to_screen(pixels: Vec<u8>, width: i32, height: i32, at: POINT) {
    let spawned = std::thread::Builder::new().name("landpad-pin".into()).spawn(move || unsafe { run_pin(pixels, width, height, at) });
    if spawned.is_err() {
        eprintln!("[fixar] não consegui abrir a janela fixada");
    }
}

unsafe fn run_pin(pixels: Vec<u8>, width: i32, height: i32, at: POINT) {
    let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let Some(mut surface) = Surface::new(width, height) else { return };
    surface.pixels_mut().copy_from_slice(&pixels);
    PINNED.with(|p| *p.borrow_mut() = Some(Pinned { surface, alpha: 255 }));
    let Ok(instance) = GetModuleHandleW(None) else { return };
    let class = w!("LandpadPin");
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(pin_proc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_SIZEALL).unwrap_or_default(),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        class,
        w!("Landpad fixado"),
        WS_POPUP,
        at.x,
        at.y,
        width,
        height,
        None,
        None,
        Some(instance.into()),
        None,
    ) else {
        eprintln!("[fixar] não consegui criar a janela fixada");
        return;
    };
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    println!("[fixar] {width} × {height} fixado na tela em ({}, {})", at.x, at.y);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    PINNED.with(|p| p.borrow_mut().take());
}

/// A janela fixada: arrasta com o mouse, roda do mouse muda a transparência, e Esc, duplo
/// clique ou botão direito fecham.
unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            PINNED.with(|p| {
                if let Some(p) = p.borrow().as_ref() {
                    let (w, h) = (p.surface.width, p.surface.height);
                    let _ = BitBlt(hdc, 0, 0, w, h, Some(p.surface.dc), 0, 0, SRCCOPY);
                    // Uma linha fina em volta mostra que isso é uma janela, não parte da tela.
                    frame(hdc, RECT { left: 0, top: 0, right: w, bottom: h }, PANEL_BORDER);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            // O jeito clássico de deixar arrastar uma janela sem título: fingir que o clique foi nele.
            let _ = ReleaseCapture();
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
            let alpha = PINNED.with(|p| {
                let mut guard = p.borrow_mut();
                let Some(p) = guard.as_mut() else { return None };
                let step: i32 = if delta > 0 { 25 } else { -25 };
                p.alpha = (p.alpha as i32 + step).clamp(40, 255) as u8;
                Some(p.alpha)
            });
            if let Some(alpha) = alpha {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK | WM_RBUTTONUP => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Bitmap de 32 bits de cima para baixo, com um DC próprio para desenhar nele.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Surface {
    unsafe fn new(width: i32, height: i32) -> Option<Surface> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bitmap.into());
        Some(Surface { dc, bitmap, old, bits: bits as *mut u8, width, height })
    }

    fn len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Pixels BGRA; espera o GDI terminar de desenhar antes.
    fn pixels(&self) -> &[u8] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, self.len())
        }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, self.len())
        }
    }

    /// CF_DIB: cabeçalho de 40 bytes e linhas de baixo para cima, como o clipboard espera.
    fn to_dib(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let row = w as usize * 4;
        let mut dib = Vec::with_capacity(40 + self.len());
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&w.to_le_bytes());
        dib.extend_from_slice(&h.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&(self.len() as u32).to_le_bytes());
        dib.extend_from_slice(&[0; 16]);
        let px = self.pixels();
        for y in (0..h as usize).rev() {
            for p in px[y * row..(y + 1) * row].chunks_exact(4) {
                dib.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        dib
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct GdiPlus(usize);

impl GdiPlus {
    unsafe fn start() -> Option<GdiPlus> {
        let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        let mut token = 0usize;
        (GdiplusStartup(&mut token, &input, std::ptr::null_mut()).0 == 0).then_some(GdiPlus(token))
    }
}

impl Drop for GdiPlus {
    fn drop(&mut self) {
        unsafe { GdiplusShutdown(self.0) };
    }
}

struct Pen(*mut GpPen);

impl Pen {
    unsafe fn new(color: u32, width: f32) -> Option<Pen> {
        let mut pen = std::ptr::null_mut();
        (GdipCreatePen1(color, width, UnitPixel, &mut pen).0 == 0).then_some(Pen(pen))
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { GdipDeletePen(self.0) };
    }
}

/// Salva como PNG; sem caminho, vai para Imagens\Landpad com data e hora no nome.
unsafe fn save_png(surface: &Surface, path: Option<&Path>, format: i32) -> Result<PathBuf, String> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = pictures_dir().join("Landpad");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let t = GetLocalTime();
            dir.join(format!(
                "Captura {:04}-{:02}-{:02} {:02}-{:02}-{:02}.png",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            ))
        }
    };
    let px = surface.pixels();
    let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
    let status = GdipCreateBitmapFromScan0(
        surface.width,
        surface.height,
        surface.width * 4,
        format,
        Some(px.as_ptr()),
        &mut bitmap,
    );
    if status.0 != 0 {
        return Err(format!("GDI+ não criou a imagem (status {})", status.0));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let status = GdipSaveImageToFile(bitmap as *mut GpImage, PCWSTR(wide.as_ptr()), &PNG_ENCODER, std::ptr::null());
    GdipDisposeImage(bitmap as *mut GpImage);
    if status.0 != 0 {
        return Err(format!("GDI+ não salvou {} (status {})", path.display(), status.0));
    }
    Ok(path)
}

/// Grava pixels RGBA, com transparência, como PNG. Serve para exportar o ícone.
pub fn write_png(rgba: &[u8], width: u32, height: u32, path: &Path) -> Result<(), String> {
    unsafe {
        let _gdiplus = GdiPlus::start().ok_or("o GDI+ não iniciou")?;
        let mut surface = Surface::new(width as i32, height as i32).ok_or("não consegui criar a imagem")?;
        for (dst, src) in surface.pixels_mut().chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
        save_png(&surface, Some(path), PIXEL_FORMAT_32BPP_ARGB).map(|_| ())
    }
}

fn pictures_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Pictures, KNOWN_FOLDER_FLAG(0), None) {
            let dir = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            if let Some(dir) = dir {
                return PathBuf::from(dir);
            }
        }
    }
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Pictures")
}

/// `--shot-test`: fotografa a tela só para medir (a foto não é salva) e desenha a
/// sobreposição com anotações sobre uma área de trabalho de mentira, sem abrir janela.
pub fn self_test() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let t0 = Instant::now();
        match capture_screen() {
            Some((origin, s)) => println!(
                "[print] tela inteira {} × {} a partir de ({}, {}), fotografada em {:.0} ms",
                s.width,
                s.height,
                origin.x,
                origin.y,
                t0.elapsed().as_secs_f64() * 1000.0
            ),
            None => eprintln!("[print] não consegui fotografar a tela"),
        }
        let Some(_gdiplus) = GdiPlus::start() else {
            eprintln!("[print] o GDI+ não iniciou");
            return;
        };
        let Some(mut desk) = Surface::new(1280, 720) else { return };
        fake_desktop(&mut desk);
        let Some(mut o) = Overlay::new(desk, None) else { return };
        let p = |x, y| POINT { x, y };
        let sel = RECT { left: 240, top: 150, right: 980, bottom: 560 };
        o.sel = Some(sel);
        o.tool = Some(Tool::Arrow);
        o.color = 0;
        o.shapes = vec![
            Shape::Blur { from: p(300, 330), to: p(620, 380) },
            Shape::Rect { from: p(290, 200), to: p(560, 250), color: 0 },
            Shape::Arrow { from: p(760, 420), to: p(580, 240), color: 0 },
            Shape::Marker { points: vec![p(300, 445), p(500, 446), p(700, 444)], color: 1 },
            Shape::Line { from: p(300, 520), to: p(700, 520), color: 3 },
            Shape::Number { at: p(275, 225), n: 1, color: 2 },
            Shape::Number { at: p(770, 440), n: 2, color: 2 },
            Shape::Text { at: p(640, 470), text: "Entrega até 14h\nPortão 3".into(), color: 4 },
        ];
        let full = RECT { left: 0, top: 0, right: o.bright.width, bottom: o.bright.height };
        // A barra com a paleta aberta e, numa segunda imagem, a dica de um botão.
        o.palette = true;
        let Some(back) = Surface::new(o.bright.width, o.bright.height) else { return };
        o.paint(back.dc, full);
        o.palette = false;
        o.hover = Some(Button::Tool(Tool::Blur));
        o.tooltip = true;
        let tooltip_back = Surface::new(o.bright.width, o.bright.height);
        if let Some(tb) = &tooltip_back {
            o.paint(tb.dc, full);
        }
        // O estado que derrubava o app na 0.2.0: ferramenta de texto recém-clicada, ainda sem
        // nenhuma letra. Pintar isso chamava o DrawTextW com zero caracteres e um ponteiro
        // que não apontava para nada.
        o.hover = None;
        o.tooltip = false;
        o.tool = Some(Tool::Text);
        o.current = Some(Shape::start(Tool::Text, p(400, 300), 0, 0));
        o.editing = true;
        if let Some(cb) = Surface::new(o.bright.width, o.bright.height) {
            o.paint(cb.dc, full);
            println!("[print] ferramenta de texto sem letras pintada sem erro");
        }
        o.editing = false;
        o.current = None;
        o.tool = Some(Tool::Arrow);
        // Sem seleção, com uma "janela" sob o mouse destacada.
        let window_back = Surface::new(o.bright.width, o.bright.height);
        if let Some(wb) = &window_back {
            let sel_backup = o.sel.take();
            o.hover_window = Some(RECT { left: 180, top: 90, right: 1100, bottom: 640 });
            o.paint(wb.dc, full);
            o.hover_window = None;
            o.sel = sel_backup;
        }
        // O conta-gotas sobre a mesma tela, com a lupa na borda da "janela" e do texto.
        let mut picker = Overlay::new(Surface::new(full.right, full.bottom).unwrap(), None);
        if let Some(picker) = &mut picker {
            BitBlt(picker.bright.dc, 0, 0, full.right, full.bottom, Some(o.bright.dc), 0, 0, SRCCOPY).ok();
            picker.mode = Mode::Color;
            picker.cursor = p(226, 167);
        }
        let color_back = picker.as_ref().and_then(|picker| {
            let back = Surface::new(full.right, full.bottom)?;
            picker.paint(back.dc, full);
            let color = picker.pixel(picker.cursor.x, picker.cursor.y)?;
            println!("[cor] pixel sob o cursor de teste: {} / {}", crate::color::hex(color), crate::color::rgb(color));
            Some(back)
        });
        for (surface, name) in [
            (Some(back), "target/shot-overlay.png"),
            (tooltip_back, "target/shot-tooltip.png"),
            (window_back, "target/shot-window.png"),
            (o.render(sel), "target/shot-result.png"),
            (color_back, "target/color-overlay.png"),
        ] {
            let Some(surface) = surface else { continue };
            match save_png(&surface, Some(Path::new(name)), PIXEL_FORMAT_32BPP_RGB) {
                Ok(path) => println!("[print] {} salvo ({} × {})", path.display(), surface.width, surface.height),
                Err(e) => eprintln!("[print] {e}"),
            }
        }
        if let Some(out) = o.render(sel) {
            let dib = out.to_dib();
            let ok = win::dib_info(&dib).is_some_and(|i| i.width == 740 && i.height == 410);
            println!("[print] DIB para o clipboard: {} bytes, cabeçalho {}", dib.len(), if ok { "ok" } else { "inválido" });
        }
        match ocr_available() {
            Ok(()) => println!("[ocr] motor de OCR disponível para os idiomas do usuário"),
            Err(e) => println!("[ocr] indisponível: {e}"),
        }
    }
}

/// Para o `--memory-test`: o caminho da captura inteiro (GDI+, as cópias da tela,
/// recorte com anotações e o DIB final) sobre uma área de trabalho de mentira do tamanho
/// da tela de verdade. Não abre janela nem lê o que está na tela.
pub fn memory_probe() -> Option<Vec<u8>> {
    unsafe {
        let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let _gdiplus = GdiPlus::start()?;
        let mut desk = Surface::new(w, h)?;
        fake_desktop(&mut desk);
        let mut o = Overlay::new(desk, None)?;
        let p = |x, y| POINT { x, y };
        let sel = RECT { left: 200, top: 120, right: 1000.min(w), bottom: 640.min(h) };
        o.sel = Some(sel);
        o.shapes = vec![
            Shape::Blur { from: p(260, 300), to: p(600, 360) },
            Shape::Rect { from: p(250, 180), to: p(560, 240), color: 0 },
            Shape::Arrow { from: p(760, 420), to: p(580, 220), color: 0 },
            Shape::Marker { points: vec![p(260, 440), p(700, 442)], color: 1 },
            Shape::Number { at: p(300, 500), n: 1, color: 2 },
            Shape::Text { at: p(400, 500), text: "teste".into(), color: 0 },
        ];
        o.back = Surface::new(w, h);
        if let Some(back) = &o.back {
            o.paint(back.dc, RECT { left: 0, top: 0, right: w, bottom: h });
        }
        Some(o.render(sel)?.to_dib())
    }
}

/// Um fundo em degradê com uma "janela" clara e linhas de texto, para o teste ter o que mostrar.
fn fake_desktop(s: &mut Surface) {
    let (w, h) = (s.width, s.height);
    let px = s.pixels_mut();
    for y in 0..h {
        for x in 0..w {
            let t = x as f32 / w as f32;
            let mix = |a: f32, b: f32| (a + (b - a) * t) as u8;
            let (mut r, mut g, mut b) = (mix(30.0, 20.0), mix(60.0, 130.0), mix(150.0, 140.0));
            if (180..1100).contains(&x) && (90..640).contains(&y) {
                (r, g, b) = (249, 250, 252);
                if y < 126 {
                    (r, g, b) = (229, 231, 235);
                } else if y > 160 && y % 32 < 12 && x < 220 + (y * 37) % 640 && x > 220 {
                    (r, g, b) = (51, 65, 85);
                }
            }
            let i = ((y * w + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
}

fn rect(a: POINT, b: POINT) -> RECT {
    RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x), bottom: a.y.max(b.y) }
}

fn contains(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let r = RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) };
    (r.left < r.right && r.top < r.bottom).then_some(r)
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}

fn inflate(r: RECT, by: i32) -> RECT {
    RECT { left: r.left - by, top: r.top - by, right: r.right + by, bottom: r.bottom + by }
}

fn same_rect(a: Option<RECT>, b: Option<RECT>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom,
        _ => false,
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn argb(alpha: u8, (r, g, b): (u8, u8, u8)) -> u32 {
    (alpha as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

unsafe fn font(px: i32, weight: i32, face: PCWSTR) -> HFONT {
    CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, face)
}

/// Texto centrado no retângulo, numa linha só.
unsafe fn text(hdc: HDC, s: &str, mut r: RECT, color: (u8, u8, u8)) {
    if s.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    SetTextColor(hdc, rgb(color));
    SetBkMode(hdc, TRANSPARENT);
    DrawTextW(hdc, &mut wide, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
}

/// Texto a partir do canto superior esquerdo, sem recorte (o clip do DC cuida disso).
///
/// Com a string vazia não chama o Windows: com zero caracteres o DrawTextW ainda lê o
/// ponteiro, e o de um Vec vazio não aponta para memória nenhuma. Era isso que derrubava o
/// app no clique da ferramenta de texto, antes da primeira letra.
unsafe fn draw_text_at(hdc: HDC, s: &str, x: i32, y: i32, color: (u8, u8, u8)) {
    if s.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut r = RECT { left: x, top: y, right: x, bottom: y };
    SetTextColor(hdc, rgb(color));
    DrawTextW(hdc, &mut wide, &mut r, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
}

unsafe fn text_width(hdc: HDC, s: &str) -> i32 {
    text_extent(hdc, s).0
}

/// Largura e altura do texto na fonte selecionada. Vazio mede largura zero, mas a altura
/// de uma linha, para a faixa da legenda já aparecer enquanto se digita a primeira letra.
unsafe fn text_extent(hdc: HDC, s: &str) -> (i32, i32) {
    let wide: Vec<u16> = if s.is_empty() { "Ag".encode_utf16().collect() } else { s.encode_utf16().collect() };
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    (if s.is_empty() { 0 } else { size.cx }, size.cy)
}

/// As linhas de uma legenda, a largura da mais larga e a altura de uma linha, na fonte
/// selecionada. Uma legenda vazia tem uma linha vazia, para a faixa e a barrinha aparecerem.
unsafe fn caption_metrics(hdc: HDC, label: &str) -> (Vec<String>, i32, i32) {
    let lines: Vec<String> = label.split('\n').map(str::to_string).collect();
    let line_h = text_extent(hdc, "").1;
    let w = lines.iter().map(|l| text_width(hdc, l)).max().unwrap_or(0);
    (lines, w, line_h)
}

/// Folga da faixa da legenda em volta do texto: horizontal e vertical.
fn caption_padding(scale: f32) -> (i32, i32) {
    ((8.0 * scale) as i32, (4.0 * scale) as i32)
}

/// Retângulo arredondado com transparência por cima do que já está no DC, com GDI+.
unsafe fn fill_round_alpha(hdc: HDC, r: RECT, radius: i32, color: u32) {
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let mut path: *mut GpPath = std::ptr::null_mut();
    if GdipCreatePath(FillModeAlternate, &mut path).0 == 0 {
        let d = (radius * 2).min(r.right - r.left).min(r.bottom - r.top).max(1);
        GdipAddPathArcI(path, r.left, r.top, d, d, 180.0, 90.0);
        GdipAddPathArcI(path, r.right - d, r.top, d, d, 270.0, 90.0);
        GdipAddPathArcI(path, r.right - d, r.bottom - d, d, d, 0.0, 90.0);
        GdipAddPathArcI(path, r.left, r.bottom - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        if GdipCreateSolidFill(color, &mut brush).0 == 0 {
            GdipFillPath(g, brush as *mut GpBrush, path);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
        GdipDeletePath(path);
    }
    GdipDeleteGraphics(g);
}

unsafe fn fill_round(hdc: HDC, r: RECT, fill: (u8, u8, u8), border: Option<(u8, u8, u8)>, radius: i32) {
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SetDCBrushColor(hdc, rgb(fill));
    match border {
        Some(b) => {
            SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCPenColor(hdc, rgb(b));
        }
        None => {
            SelectObject(hdc, GetStockObject(NULL_PEN));
        }
    }
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
}

/// Bolinha cheia de diâmetro `d` centrada em (cx, cy), com ou sem contorno.
unsafe fn disc(hdc: HDC, cx: i32, cy: i32, d: i32, fill: (u8, u8, u8), border: Option<(u8, u8, u8)>) {
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SetDCBrushColor(hdc, rgb(fill));
    match border {
        Some(b) => {
            SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCPenColor(hdc, rgb(b));
        }
        None => {
            SelectObject(hdc, GetStockObject(NULL_PEN));
        }
    }
    let _ = Ellipse(hdc, cx - d / 2, cy - d / 2, cx + d / 2, cy + d / 2);
}

/// Uma linha com antisserrilhado, para o ícone da ferramenta de linha.
unsafe fn aa_line(hdc: HDC, from: POINT, to: POINT, width: f32, color: (u8, u8, u8)) {
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    if let Some(pen) = Pen::new(argb(255, color), width) {
        GdipSetPenStartCap(pen.0, LineCapRound);
        GdipSetPenEndCap(pen.0, LineCapRound);
        GdipDrawLineI(g, pen.0, from.x, from.y, to.x, to.y);
    }
    GdipDeleteGraphics(g);
}

unsafe fn fill_rect(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    SetDCBrushColor(hdc, rgb(color));
    FillRect(hdc, &r, HBRUSH(GetStockObject(DC_BRUSH).0));
}

unsafe fn frame(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    SetDCBrushColor(hdc, rgb(color));
    FrameRect(hdc, &r, HBRUSH(GetStockObject(DC_BRUSH).0));
}
