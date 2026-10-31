//! Captura de tela no estilo Lightshot: o atalho congela a tela, você arrasta uma região,
//! anota se quiser e Enter copia. A captura vai para o clipboard e para o histórico.
//!
//! O módulo só existe enquanto a captura está aberta: uma thread própria fotografa todos
//! os monitores, abre uma janela por cima deles e, ao terminar, libera tudo. O desenho é
//! GDI (e GDI+ nas anotações, pelo antisserrilhado): repintar a tela inteira a cada
//! movimento do mouse é trabalho para BitBlt, não para o renderizador do Slint.

use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
// Sem glob: o GDI+ exporta uma constante `Ok` que esconderia o Ok do Rust.
use windows::Win32::Graphics::GdiPlus::{
    CombineModeReplace, GdipCreateAdjustableArrowCap, GdipCreateBitmapFromScan0, GdipCreateFromHDC, GdipCreatePen1,
    GdipDeleteCustomLineCap, GdipDeleteGraphics, GdipDeletePen, GdipDisposeImage, GdipDrawLineI, GdipDrawLinesI,
    GdipDrawRectangleI, GdipSaveImageToFile, GdipSetClipRectI, GdipSetPenCustomEndCap, GdipSetPenEndCap,
    GdipSetPenLineJoin, GdipSetPenStartCap, GdipSetSmoothingMode, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
    GpAdjustableArrowCap, GpBitmap, GpCustomLineCap, GpGraphics, GpImage, GpPen, LineCapRound, LineJoinRound, Point,
    SmoothingModeAntiAlias, UnitPixel,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::{self, Captured};

static ACTIVE: AtomicBool = AtomicBool::new(false);

const PNG_ENCODER: GUID = GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);
const PIXEL_FORMAT_32BPP_RGB: i32 = 0x0002_2009;

/// Cores das anotações, na ordem do botão de cor.
const COLORS: [(u8, u8, u8); 4] = [(0xE8, 0x41, 0x4B), (0xF5, 0xA6, 0x23), (0x2E, 0x63, 0xFF), (0x3B, 0xB9, 0x81)];
const ACCENT: (u8, u8, u8) = (0x2E, 0x63, 0xFF);
const PANEL: (u8, u8, u8) = (0x20, 0x20, 0x20);
const PANEL_BORDER: (u8, u8, u8) = (0x40, 0x40, 0x40);
const PANEL_HOVER: (u8, u8, u8) = (0x3A, 0x3A, 0x3A);
const PANEL_TEXT: (u8, u8, u8) = (0xF0, 0xF0, 0xF0);

type OnDone = Box<dyn FnOnce(Vec<u8>) + Send>;

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
    static CLOSING: Cell<bool> = const { Cell::new(false) };
}

/// Abre a captura numa thread própria; ignora o atalho se já houver uma aberta.
/// `on_done` recebe a imagem final como DIB, já copiada para o clipboard.
pub fn open(on_done: impl FnOnce(Vec<u8>) + Send + 'static) {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("landpad-shot".into()).spawn(move || {
        unsafe { run(Box::new(on_done)) };
        ACTIVE.store(false, Ordering::Release);
    });
    if spawned.is_err() {
        ACTIVE.store(false, Ordering::Release);
    }
}

unsafe fn run(on_done: OnDone) {
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
    OVERLAY.with(|o| *o.borrow_mut() = Some(overlay));
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    println!("[print] tela de {width} × {height} congelada em {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    // Solta as três cópias da tela antes de desligar o GDI+.
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

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Arrow,
    Rect,
    Marker,
    Blur,
}

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Tool(Tool),
    Color,
    Undo,
    Save,
    Copy,
    Close,
}

const BUTTONS: [Button; 9] = [
    Button::Tool(Tool::Arrow),
    Button::Tool(Tool::Rect),
    Button::Tool(Tool::Marker),
    Button::Tool(Tool::Blur),
    Button::Color,
    Button::Undo,
    Button::Save,
    Button::Copy,
    Button::Close,
];

impl Button {
    /// Glifo da Segoe Fluent Icons.
    fn glyph(self) -> &'static str {
        match self {
            Button::Tool(Tool::Arrow) => "\u{E72A}",
            Button::Tool(Tool::Rect) => "\u{E739}",
            Button::Tool(Tool::Marker) => "\u{E7E6}",
            Button::Tool(Tool::Blur) => "\u{ED1A}",
            Button::Color => "",
            Button::Undo => "\u{E7A7}",
            Button::Save => "\u{E74E}",
            Button::Copy => "\u{E8C8}",
            Button::Close => "\u{E711}",
        }
    }

    /// Começa um grupo novo na barra, com um separador antes.
    fn starts_group(self) -> bool {
        matches!(self, Button::Undo | Button::Save)
    }
}

enum Shape {
    Arrow { from: POINT, to: POINT, color: usize },
    Rect { from: POINT, to: POINT, color: usize },
    Marker { points: Vec<POINT>, color: usize },
    Blur { from: POINT, to: POINT },
}

impl Shape {
    fn start(tool: Tool, p: POINT, color: usize) -> Shape {
        match tool {
            Tool::Arrow => Shape::Arrow { from: p, to: p, color },
            Tool::Rect => Shape::Rect { from: p, to: p, color },
            Tool::Marker => Shape::Marker { points: vec![p], color },
            Tool::Blur => Shape::Blur { from: p, to: p },
        }
    }

    fn extend(&mut self, p: POINT) {
        match self {
            Shape::Arrow { to, .. } | Shape::Rect { to, .. } | Shape::Blur { to, .. } => *to = p,
            Shape::Marker { points, .. } => points.push(p),
        }
    }

    /// Um clique sem arrastar não vira anotação.
    fn is_empty(&self) -> bool {
        match self {
            Shape::Arrow { from, to, .. } | Shape::Rect { from, to, .. } | Shape::Blur { from, to } => {
                (from.x - to.x).abs() + (from.y - to.y).abs() < 4
            }
            Shape::Marker { points, .. } => points.len() < 2,
        }
    }
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    Select(POINT),
    Move { start: POINT, from: RECT },
    Draw,
}

#[derive(Clone, Copy, PartialEq)]
enum Finish {
    Cancel,
    Copy,
    Save,
}

struct Overlay {
    hwnd: HWND,
    /// A tela como estava no atalho, e a mesma foto escurecida para fora da seleção.
    bright: Surface,
    dark: Surface,
    /// Onde a tela é montada antes de ir para a janela, para não piscar.
    back: Option<Surface>,
    scale: f32,
    sel: Option<RECT>,
    drag: Drag,
    tool: Option<Tool>,
    color: usize,
    shapes: Vec<Shape>,
    current: Option<Shape>,
    hover: Option<Button>,
    text_font: HFONT,
    icon_font: HFONT,
    on_done: Option<OnDone>,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.text_font.into());
            let _ = DeleteObject(self.icon_font.into());
        }
    }
}

impl Overlay {
    unsafe fn new(bright: Surface, on_done: Option<OnDone>) -> Option<Overlay> {
        let mut dark = Surface::new(bright.width, bright.height)?;
        for (d, s) in dark.pixels_mut().chunks_exact_mut(4).zip(bright.pixels().chunks_exact(4)) {
            d[0] = ((s[0] as u16 * 110) >> 8) as u8;
            d[1] = ((s[1] as u16 * 110) >> 8) as u8;
            d[2] = ((s[2] as u16 * 110) >> 8) as u8;
            d[3] = 255;
        }
        let scale = win::scale();
        Some(Overlay {
            hwnd: HWND::default(),
            bright,
            dark,
            back: None,
            scale,
            sel: None,
            drag: Drag::None,
            tool: None,
            color: 0,
            shapes: Vec::new(),
            current: None,
            hover: None,
            text_font: font((12.0 * scale) as i32, 600, w!("Segoe UI Variable Text")),
            icon_font: font((16.0 * scale) as i32, 400, w!("Segoe Fluent Icons")),
            on_done,
        })
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn clamp(&self, p: POINT) -> POINT {
        POINT { x: p.x.clamp(0, self.bright.width), y: p.y.clamp(0, self.bright.height) }
    }

    fn redraw(&self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    /// A barra de ferramentas fica embaixo da seleção, alinhada à direita; sem espaço, vai para cima.
    fn toolbar(&self) -> Option<(RECT, Vec<(Button, RECT)>)> {
        let sel = self.sel?;
        if matches!(self.drag, Drag::Select(_)) {
            return None;
        }
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

    fn button_at(&self, p: POINT) -> Option<Button> {
        self.toolbar()?.1.into_iter().find(|(_, r)| contains(*r, p)).map(|(b, _)| b)
    }

    fn press(&mut self, button: Button) -> Option<Finish> {
        match button {
            Button::Tool(t) => self.tool = if self.tool == Some(t) { None } else { Some(t) },
            Button::Color => self.color = (self.color + 1) % COLORS.len(),
            Button::Undo => {
                self.shapes.pop();
            }
            Button::Save => return Some(Finish::Save),
            Button::Copy => return Some(Finish::Copy),
            Button::Close => return Some(Finish::Cancel),
        }
        None
    }

    fn mouse_down(&mut self, p: POINT) -> Option<Finish> {
        if let Some(button) = self.button_at(p) {
            let finish = self.press(button);
            self.redraw();
            return finish;
        }
        let p = self.clamp(p);
        match self.sel {
            Some(sel) if contains(sel, p) => match self.tool {
                Some(tool) => {
                    self.current = Some(Shape::start(tool, p, self.color));
                    self.drag = Drag::Draw;
                }
                None => self.drag = Drag::Move { start: p, from: sel },
            },
            // Clicar fora começa outra seleção, do zero.
            _ => {
                self.sel = None;
                self.shapes.clear();
                self.drag = Drag::Select(p);
            }
        }
        self.redraw();
        None
    }

    fn mouse_move(&mut self, p: POINT) {
        let p = self.clamp(p);
        match self.drag {
            Drag::None => {
                let hover = self.button_at(p);
                if hover != self.hover {
                    self.hover = hover;
                    self.redraw();
                }
                return;
            }
            Drag::Select(start) => self.sel = Some(rect(start, p)),
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
            }
        }
        self.redraw();
    }

    fn mouse_up(&mut self) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Select(_) => {
                if self.sel.is_some_and(|s| s.right - s.left < 4 || s.bottom - s.top < 4) {
                    self.sel = None;
                }
            }
            Drag::Draw => {
                if let Some(shape) = self.current.take().filter(|s| !s.is_empty()) {
                    self.shapes.push(shape);
                }
            }
            _ => {}
        }
        self.redraw();
    }

    fn key(&mut self, vk: u16) -> Option<Finish> {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let has_sel = self.sel.is_some();
        let tool = |t| Button::Tool(t);
        match vk {
            k if k == VK_ESCAPE.0 => return Some(Finish::Cancel),
            k if k == VK_RETURN.0 && has_sel => return Some(Finish::Copy),
            0x43 if ctrl && has_sel => return Some(Finish::Copy),
            0x53 if ctrl && has_sel => return Some(Finish::Save),
            0x5A if ctrl => {
                self.shapes.pop();
            }
            0x41 if has_sel && !ctrl => {
                self.press(tool(Tool::Arrow));
            }
            0x52 if has_sel && !ctrl => {
                self.press(tool(Tool::Rect));
            }
            0x4D if has_sel && !ctrl => {
                self.press(tool(Tool::Marker));
            }
            0x42 if has_sel && !ctrl => {
                self.press(tool(Tool::Blur));
            }
            _ => return None,
        }
        self.redraw();
        None
    }

    fn cursor_at(&self, p: POINT) -> PCWSTR {
        if self.button_at(p).is_some() {
            IDC_HAND
        } else if self.tool.is_none() && self.sel.is_some_and(|s| contains(s, p)) && !matches!(self.drag, Drag::Select(_)) {
            IDC_SIZEALL
        } else {
            IDC_CROSS
        }
    }

    /// A seleção com as anotações, do tamanho exato da região.
    unsafe fn render(&self, sel: RECT) -> Option<Surface> {
        let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
        let out = Surface::new(w, h)?;
        BitBlt(out.dc, 0, 0, w, h, Some(self.bright.dc), sel.left, sel.top, SRCCOPY).ok()?;
        draw_shapes(out.dc, &self.bright, self.shapes.iter(), POINT { x: sel.left, y: sel.top }, sel, self.scale);
        Some(out)
    }

    unsafe fn paint(&self, hdc: HDC) {
        let _ = BitBlt(hdc, 0, 0, self.bright.width, self.bright.height, Some(self.dark.dc), 0, 0, SRCCOPY);
        let old_font = SelectObject(hdc, self.text_font.into());
        match self.sel {
            None => self.paint_hint(hdc),
            Some(sel) => self.paint_selection(hdc, sel),
        }
        SelectObject(hdc, old_font);
    }

    unsafe fn paint_hint(&self, hdc: HDC) {
        let label = "Arraste para selecionar · Esc cancela";
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut area = RECT { left: 0, top: 0, right: self.bright.width, bottom: self.bright.height };
        if !self.hwnd.is_invalid() && GetMonitorInfoW(monitor, &mut mi).as_bool() {
            let mut corner = [POINT { x: mi.rcMonitor.left, y: mi.rcMonitor.top }, POINT { x: mi.rcMonitor.right, y: mi.rcMonitor.bottom }];
            let _ = ScreenToClient(self.hwnd, &mut corner[0]);
            let _ = ScreenToClient(self.hwnd, &mut corner[1]);
            area = RECT { left: corner[0].x, top: corner[0].y, right: corner[1].x, bottom: corner[1].y };
        }
        let width = text_width(hdc, label) + self.px(32.0);
        let height = self.px(34.0);
        let left = (area.left + area.right - width) / 2;
        let top = area.top + self.px(28.0);
        let r = RECT { left, top, right: left + width, bottom: top + height };
        fill_round(hdc, r, PANEL, Some(PANEL_BORDER), height);
        text(hdc, label, r, PANEL_TEXT);
    }

    unsafe fn paint_selection(&self, hdc: HDC, sel: RECT) {
        let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
        let _ = BitBlt(hdc, sel.left, sel.top, w, h, Some(self.bright.dc), sel.left, sel.top, SRCCOPY);
        draw_shapes(hdc, &self.bright, self.shapes.iter().chain(self.current.iter()), POINT::default(), sel, self.scale);

        // Moldura branca com uma linha escura por fora, para aparecer em qualquer fundo.
        frame(hdc, RECT { left: sel.left - 2, top: sel.top - 2, right: sel.right + 2, bottom: sel.bottom + 2 }, (0x14, 0x14, 0x14));
        frame(hdc, RECT { left: sel.left - 1, top: sel.top - 1, right: sel.right + 1, bottom: sel.bottom + 1 }, (0xFF, 0xFF, 0xFF));

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
        for (button, r) in buttons {
            let inner = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
            if matches!(button, Button::Tool(t) if Some(t) == self.tool) {
                fill_round(hdc, inner, ACCENT, None, self.px(6.0));
            } else if self.hover == Some(button) {
                fill_round(hdc, inner, PANEL_HOVER, None, self.px(6.0));
            }
            if button.starts_group() {
                let x = r.left - self.px(5.0);
                fill_rect(hdc, RECT { left: x, top: r.top + self.px(7.0), right: x + 1, bottom: r.bottom - self.px(7.0) }, PANEL_BORDER);
            }
            if button == Button::Color {
                let d = self.px(14.0);
                let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                SelectObject(hdc, GetStockObject(DC_BRUSH));
                SelectObject(hdc, GetStockObject(DC_PEN));
                SetDCBrushColor(hdc, rgb(COLORS[self.color]));
                SetDCPenColor(hdc, rgb(PANEL_TEXT));
                let _ = Ellipse(hdc, cx - d / 2, cy - d / 2, cx + d / 2, cy + d / 2);
            } else {
                text(hdc, button.glyph(), r, PANEL_TEXT);
            }
        }
        SelectObject(hdc, old_font);
    }
}

/// Borrões primeiro, direto nos pixels; setas, retângulos e marca-texto por cima, com GDI+.
/// As formas estão em coordenadas da tela; `offset` desloca para a superfície de destino.
unsafe fn draw_shapes<'a>(
    hdc: HDC,
    bright: &Surface,
    shapes: impl Iterator<Item = &'a Shape> + Clone,
    offset: POINT,
    clip: RECT,
    scale: f32,
) {
    for shape in shapes.clone() {
        if let Shape::Blur { from, to } = shape {
            if let Some(r) = intersect(rect(*from, *to), clip) {
                pixelate(hdc, bright, r, offset, scale);
            }
        }
    }

    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    GdipSetClipRectI(g, clip.left - offset.x, clip.top - offset.y, clip.right - clip.left, clip.bottom - clip.top, CombineModeReplace);
    let at = |p: POINT| (p.x - offset.x, p.y - offset.y);
    for shape in shapes {
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
            Shape::Blur { .. } => {}
        }
    }
    GdipDeleteGraphics(g);
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
            with_overlay(|o| o.mouse_up());
            let _ = ReleaseCapture();
            None
        }
        WM_LBUTTONDBLCLK => with_overlay(|o| o.sel.filter(|s| contains(*s, point)).map(|_| Finish::Copy)).flatten(),
        WM_RBUTTONUP => Some(Finish::Cancel),
        WM_KEYDOWN => with_overlay(|o| o.key(wparam.0 as u16)).flatten(),
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
                o.paint(back.dc);
                let r = ps.rcPaint;
                let _ = BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(back.dc), r.left, r.top, SRCCOPY);
            }
            None => o.paint(hdc),
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
    let done = with_overlay(|o| {
        if how == Finish::Cancel {
            return None;
        }
        let image = o.render(o.sel?)?;
        Some((image, o.on_done.take()))
    })
    .flatten();

    if let Some((image, on_done)) = done {
        let (w, h) = (image.width, image.height);
        if how == Finish::Save {
            match save_png(&image, None) {
                Ok(path) => println!("[print] salvo em {}", path.display()),
                Err(e) => eprintln!("[print] não consegui salvar: {e}"),
            }
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
unsafe fn save_png(surface: &Surface, path: Option<&Path>) -> Result<PathBuf, String> {
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
        PIXEL_FORMAT_32BPP_RGB,
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
        ];
        let Some(back) = Surface::new(o.bright.width, o.bright.height) else { return };
        o.paint(back.dc);
        for (surface, name) in [(Some(back), "target/shot-overlay.png"), (o.render(sel), "target/shot-result.png")] {
            let Some(surface) = surface else { continue };
            match save_png(&surface, Some(Path::new(name))) {
                Ok(path) => println!("[print] {} salvo ({} × {})", path.display(), surface.width, surface.height),
                Err(e) => eprintln!("[print] {e}"),
            }
        }
        if let Some(out) = o.render(sel) {
            let dib = out.to_dib();
            let ok = win::dib_info(&dib).is_some_and(|i| i.width == 740 && i.height == 410);
            println!("[print] DIB para o clipboard: {} bytes, cabeçalho {}", dib.len(), if ok { "ok" } else { "inválido" });
        }
    }
}

/// Para o `--memory-test`: o caminho da captura inteiro (GDI+, as três cópias da tela,
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
        ];
        o.back = Surface::new(w, h);
        if let Some(back) = &o.back {
            o.paint(back.dc);
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

fn rgb((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn argb(alpha: u8, (r, g, b): (u8, u8, u8)) -> u32 {
    (alpha as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

unsafe fn font(px: i32, weight: i32, face: PCWSTR) -> HFONT {
    CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, face)
}

unsafe fn text(hdc: HDC, s: &str, mut r: RECT, color: (u8, u8, u8)) {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    SetTextColor(hdc, rgb(color));
    SetBkMode(hdc, TRANSPARENT);
    DrawTextW(hdc, &mut wide, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
}

unsafe fn text_width(hdc: HDC, s: &str) -> i32 {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    size.cx
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

unsafe fn fill_rect(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    SetDCBrushColor(hdc, rgb(color));
    FillRect(hdc, &r, HBRUSH(GetStockObject(DC_BRUSH).0));
}

unsafe fn frame(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    SetDCBrushColor(hdc, rgb(color));
    FrameRect(hdc, &r, HBRUSH(GetStockObject(DC_BRUSH).0));
}
