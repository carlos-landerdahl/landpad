//! Landpad — gerenciador de área de transferência para Windows.
//!
//! Fica na bandeja; o popup do histórico nasce oculto e só aparece no atalho. A janela
//! de configurações e a captura de tela existem só enquanto estão abertas.

// Sem janela de console ao iniciar com o Windows; aberto de um terminal, os logs vão para ele.
#![windows_subsystem = "windows"]

mod config;
mod images;
mod logo;
mod settings;
mod shot;
mod win;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use config::Config;

slint::include_modules!();

const MAX_ROWS: usize = 200;
/// Miniatura da linha e área da prévia, em pixels lógicos; precisam bater com o app.slint.
const THUMB_SIZE: (f32, f32) = (36.0, 22.0);
const PREVIEW_SIZE: (f32, f32) = (372.0, 152.0);

enum Content {
    Text(String),
    Image(Picture),
}

/// Imagem do histórico. O original fica em disco (ver images.rs); aqui só a miniatura.
struct Picture {
    hash: u64,
    width: u32,
    height: u32,
    has_png: bool,
    thumb: slint::Image,
}

struct Clip {
    content: Content,
    app: String,
    copied: Instant,
    /// Ordem em que foi fixado; fixados ficam no topo, o mais recente primeiro,
    /// e não contam no limite do histórico.
    pinned: Option<u64>,
}

impl Content {
    fn same(&self, other: &Content) -> bool {
        match (self, other) {
            (Content::Text(a), Content::Text(b)) => a == b,
            (Content::Image(a), Content::Image(b)) => a.hash == b.hash,
            _ => false,
        }
    }
}

struct State {
    ui: slint::Weak<AppWindow>,
    clips: Vec<Clip>,
    query: String,
    shown: Vec<usize>,
    model: Rc<VecModel<ClipRow>>,
    target: win::Target,
    only_pinned: bool,
    pin_seq: u64,
    config: Config,
    /// Pausa vale só até fechar o app; não vai para o arquivo de preferências.
    paused: bool,
    /// Atalhos que o Windows aceitou de fato.
    hotkeys: win::Hotkeys,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static OPENED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static FIRST_OPEN: Cell<bool> = const { Cell::new(true) };
    static FRAME_TIMING: Cell<bool> = const { Cell::new(false) };
    static BENCH: Cell<bool> = const { Cell::new(false) };
    static TIMINGS: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
    /// Hash da imagem cuja prévia está na tela (ou sendo gerada).
    static PREVIEW_OF: Cell<Option<u64>> = const { Cell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("estado não iniciado")))
}

fn main() -> Result<(), slint::PlatformError> {
    win::attach_console();
    let flag = |name: &str| std::env::args().any(|a| a == name);
    if flag("--shot-test") {
        shot::self_test();
        return Ok(());
    }
    // Os modos de teste rodam ao lado do app de verdade, com uma pasta de imagens só deles.
    let testing = flag("--bench") || flag("--settings-test") || flag("--memory-test");
    if testing {
        images::use_test_dir();
    } else if !win::first_instance() {
        eprintln!("O Landpad já está aberto: procure o ícone na bandeja.");
        return Ok(());
    }
    // Sobra de uma execução encerrada à força.
    images::clear();
    let config = Config::load();
    let ui = AppWindow::new()?;
    ui.set_logo(logo_image(24.0, win::scale()));
    ui.set_dark_mode(config.dark());
    let model = Rc::new(VecModel::<ClipRow>::default());
    ui.set_rows(ModelRc::from(model.clone()));
    win::set_capture_rules(false, config.keep_images, &config.ignored_apps);
    let (popup_key, shot_key) = (config.popup_hotkey, config.shot_hotkey);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ui: ui.as_weak(),
            clips: Vec::new(),
            query: String::new(),
            shown: Vec::new(),
            model,
            target: win::Target::default(),
            only_pinned: false,
            pin_seq: 0,
            config,
            paused: false,
            hotkeys: win::Hotkeys::default(),
        });
    });
    with_state(refresh);

    let weak = ui.as_weak();
    ui.on_open_settings(move || {
        // Abre antes de esconder o popup: enquanto o popup tem o foco, o Windows deixa a
        // janela nova vir para a frente.
        settings::open();
        if let Some(ui) = weak.upgrade() {
            hide(&ui);
        }
    });

    ui.on_query_edited(|query| {
        with_state(|st| {
            st.query = query.to_string();
            refresh(st);
            update_preview(st);
        })
    });

    ui.on_selection_changed(|| with_state(update_preview));

    ui.on_pinned_filter(|only_pinned| {
        with_state(|st| {
            st.only_pinned = only_pinned;
            refresh(st);
            update_preview(st);
        })
    });

    let weak = ui.as_weak();
    ui.on_toggle_pin(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(next) = with_state(|st| toggle_pin(st, index as usize)) else { return };
        // Fora do with_state: mudar a seleção dispara selection-changed, que usa o estado.
        ui.invoke_select(next as i32);
        with_state(update_preview);
    });

    let weak = ui.as_weak();
    ui.on_activate(move |index, _plain| {
        let Some(ui) = weak.upgrade() else { return };
        let picked = with_state(|st| {
            let k = *st.shown.get(index as usize)?;
            let mut clip = st.clips.remove(k);
            clip.copied = Instant::now();
            let payload = match &clip.content {
                Content::Text(text) => Ok(win::Captured::Text(text.clone())),
                Content::Image(p) => {
                    images::load(p.hash, p.has_png).map(|(dib, png)| win::Captured::Image { dib, png })
                }
            };
            st.clips.insert(0, clip);
            Some((payload, st.target))
        });
        hide(&ui);
        match picked {
            Some((Ok(payload), target)) => win::paste(payload, target),
            Some((Err(e), _)) => eprintln!("[colar] não consegui ler a imagem guardada: {e}"),
            None => {}
        }
    });

    let weak = ui.as_weak();
    ui.on_dismiss(move || {
        if let Some(ui) = weak.upgrade() {
            hide(&ui);
        }
    });

    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Mede do atalho até o primeiro frame pintado, quando o renderizador permite.
    let weak = ui.as_weak();
    let notifier = ui.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, slint::RenderingState::AfterRendering) {
            return;
        }
        if let Some(t0) = OPENED_AT.with(|c| c.take()) {
            let ms = elapsed_ms(t0);
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    report_open(&ui, ms, "até o primeiro frame");
                }
            });
        }
    });
    FRAME_TIMING.with(|c| c.set(notifier.is_ok()));

    let on_hotkey = ui.as_weak();
    let on_foreground = ui.as_weak();
    let on_quick = ui.as_weak();
    let on_tray = ui.as_weak();
    let handlers = win::Handlers {
        on_clip: Box::new(|captured, app| match captured {
            win::Captured::Text(text) => {
                let _ = slint::invoke_from_event_loop(move || add_clip(Content::Text(text), app));
            }
            // Miniatura e disco numa thread à parte, para não segurar o atalho global.
            win::Captured::Image { dib, png } => {
                std::thread::spawn(move || store_image(dib, png, app));
            }
        }),
        on_hotkey: Box::new(move |t0, target| {
            let weak = on_hotkey.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    toggle(&ui, t0, target);
                }
            });
        }),
        on_foreground: Box::new(move |hwnd| {
            let weak = on_foreground.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if ui.window().is_visible() && hwnd_of(ui.window()) != Some(hwnd) {
                        hide(&ui);
                    }
                }
            });
        }),
        on_quick: Box::new(move |n| {
            let weak = on_quick.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                if ui.window().is_visible() && n < with_state(|st| st.shown.len()) {
                    ui.invoke_activate(n as i32, false);
                }
            });
        }),
        on_shot: Box::new(start_shot),
        on_tray: Box::new(move |command| {
            let weak = on_tray.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                match command {
                    win::TrayCommand::Open => toggle(&ui, Instant::now(), win::last_app()),
                    win::TrayCommand::Shot => start_shot(),
                    win::TrayCommand::Pause => {
                        with_state(|st| {
                            st.paused = !st.paused;
                            let config = st.config.clone();
                            apply_config(st, config);
                        });
                        settings::sync();
                    }
                    win::TrayCommand::Settings => settings::open(),
                    win::TrayCommand::Quit => {
                        let _ = slint::quit_event_loop();
                    }
                }
            });
        }),
        on_hotkeys: Box::new(|keys| {
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|st| st.hotkeys = keys);
                report_hotkeys(keys);
                settings::sync();
            });
        }),
        on_theme: Box::new(|| {
            let _ = slint::invoke_from_event_loop(|| {
                with_state(|st| {
                    let config = st.config.clone();
                    apply_config(st, config);
                });
                settings::sync();
            });
        }),
    };
    let options = win::Options { tray: !testing, hotkeys: !testing };
    match win::start(handlers, options, popup_key, shot_key) {
        Ok(keys) => {
            with_state(|st| st.hotkeys = keys);
            report_hotkeys(keys);
        }
        Err(e) => eprintln!("Falha ao ligar o Landpad ao Windows: {e}"),
    }
    let (private, working) = win::memory_mb();
    println!("[memória] parado, sem janela: {private:.1} MB privados · {working:.1} MB working set");

    if flag("--bench") {
        start_bench(&ui);
    }
    if flag("--settings-test") {
        settings::self_test();
    }
    if flag("--memory-test") {
        BENCH.with(|c| c.set(true));
        settings::memory_test(&ui);
    }

    let result = slint::run_event_loop_until_quit();
    win::shutdown();
    // O histórico vive só enquanto o app está aberto, imagens inclusive.
    images::clear();
    result
}

fn start_shot() {
    shot::open(|dib| store_image(dib, None, "captura".into()));
}

fn report_hotkeys(keys: win::Hotkeys) {
    let label = |k: Option<win::Hotkey>, action: &str, what: &str| match k {
        Some(k) => format!("{} {action}", k.label()),
        None => format!("sem atalho para {what}"),
    };
    println!(
        "Landpad rodando. {}, {}; o ícone na bandeja tem o resto.",
        label(keys.popup, "abre o histórico", "o histórico"),
        label(keys.shot, "captura a tela", "a captura")
    );
}

/// Aplica as preferências em tudo que já está aberto e salva se mudaram.
/// Atalhos ficam de fora: quem muda um atalho pede o registro (ver settings.rs).
fn apply_config(st: &mut State, new: Config) {
    let changed = new != st.config;
    st.config = new;
    win::set_capture_rules(st.paused, st.config.keep_images, &st.config.ignored_apps);
    if let Some(ui) = st.ui.upgrade() {
        ui.set_dark_mode(st.config.dark());
    }
    evict(st);
    refresh(st);
    if changed {
        st.config.save();
    }
}

/// O logo no tamanho exato dos pixels da tela, para não ser reamostrado na hora de pintar.
fn logo_image(logical: f32, scale: f32) -> slint::Image {
    let size = (logical * scale).round().max(16.0) as u32;
    let rgba = logo::rgba(size, false);
    slint::Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, size, size))
}

fn toggle(ui: &AppWindow, t0: Instant, target: win::Target) {
    if ui.window().is_visible() {
        hide(ui);
        return;
    }
    let (position, quick_paste) = with_state(|st| {
        st.target = target;
        st.query.clear();
        st.only_pinned = false;
        refresh(st);
        (st.config.position, st.config.quick_paste)
    });

    let scale = ui.window().scale_factor();
    let (x, y) = win::popup_origin((400.0 * scale) as i32, (440.0 * scale) as i32, position);
    ui.window().set_position(slint::PhysicalPosition::new(x, y));

    OPENED_AT.with(|c| c.set(Some(t0)));
    if let Err(e) = ui.show() {
        eprintln!("não consegui abrir a janela: {e}");
        return;
    }
    let bench = BENCH.with(Cell::get);
    with_hwnd(ui, move |hwnd| win::prepare_popup(hwnd, !bench));
    // No bench o popup não tem foco: Ctrl+Shift+número iria colar no app que você está usando.
    if !bench && quick_paste {
        win::quick_keys(true);
    }
    ui.invoke_open();
    with_state(update_preview);

    if !FRAME_TIMING.with(|c| c.get()) {
        if let Some(t0) = OPENED_AT.with(|c| c.take()) {
            report_open(ui, elapsed_ms(t0), "até show(); este renderizador não avisa o frame");
        }
    }
}

fn hide(ui: &AppWindow) {
    let _ = ui.hide();
    win::quick_keys(false);
    clear_preview(ui);
    let (private, working) = win::memory_mb();
    println!("[memória] janela oculta: {private:.1} MB privados · {working:.1} MB working set");
}

fn report_open(ui: &AppWindow, ms: f64, how: &str) {
    let first = FIRST_OPEN.with(|c| c.replace(false));
    let (private, working) = win::memory_mb();
    let label = if first { "1ª abertura" } else { "abertura" };
    println!("[{label}] {ms:.1} ms ({how}) · {private:.1} MB privados · {working:.1} MB working set");
    ui.set_stats(format!("{} MB · abriu em {:.0} ms", decimal(private), ms).into());
    TIMINGS.with(|t| t.borrow_mut().push(ms));
}

/// `--bench`: enche o histórico com 300 textos (2 fixados) e 2 imagens e abre/fecha o
/// popup sozinho, para medir sem depender do atalho. Não rouba o foco do app em uso.
fn start_bench(ui: &AppWindow) {
    BENCH.with(|c| c.set(true));
    for i in 0..300 {
        let text = match i % 4 {
            0 => format!("Pedido {} · 32 t de soja · Rio Verde → Santos", 48_000 + i),
            1 => format!("https://exemplo.com.br/fretes/{i}"),
            2 => format!("SELECT safra, SUM(volume_t)\nFROM fretes\nWHERE id = {i};"),
            _ => format!("Reunião {i}: cotações da semana e janela de coleta"),
        };
        add_clip(Content::Text(text), "bench".into());
    }
    with_state(|st| {
        for k in [7, 2] {
            st.pin_seq += 1;
            st.clips[k].pinned = Some(st.pin_seq);
        }
        refresh(st);
    });
    for (seed, (w, h)) in [(1920, 1080), (1280, 960)].into_iter().enumerate() {
        let dib = sample_dib(w, h, seed as u32);
        std::thread::spawn(move || store_image(dib, None, "bench".into()));
    }

    let weak = ui.as_weak();
    let tick = Cell::new(0u32);
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(300), move || {
        let Some(ui) = weak.upgrade() else { return };
        let n = tick.get();
        tick.set(n + 1);
        if n < 20 {
            if n % 2 == 0 {
                toggle(&ui, Instant::now(), win::Target::default());
                if n == 8 {
                    ui.set_only_pinned(true);
                    ui.invoke_pinned_filter(true);
                }
            } else {
                match n {
                    5 => save_snapshot(ui.window(), "target/popup.bmp"),
                    9 => save_snapshot(ui.window(), "target/popup-fixados.bmp"),
                    _ => {}
                }
                hide(&ui);
            }
            return;
        }
        print_bench_summary();
        let _ = slint::quit_event_loop();
    });
}

/// Imagem de teste do `--bench`: um degradê azul → teal com faixas diagonais.
fn sample_dib(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut dib = Vec::with_capacity(40 + (width * height * 4) as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(width as i32).to_le_bytes());
    dib.extend_from_slice(&(height as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&[0; 24]);
    for y in 0..height {
        for x in 0..width {
            let t = x as f32 / width as f32;
            let band = if ((x + y + seed * 90) / 90) % 2 == 0 { 1.0 } else { 0.82 };
            let mix = |a: f32, b: f32| ((a + (b - a) * t) * band) as u8;
            dib.extend_from_slice(&[mix(255.0, 180.0), mix(99.0, 205.0), mix(46.0, 46.0), 255]);
        }
    }
    dib
}

/// Salva uma janela como BMP de 32 bits, para conferir o visual sem olhar a tela.
fn save_snapshot(window: &slint::Window, path: &str) {
    let buf = match window.take_snapshot() {
        Ok(buf) => buf,
        Err(e) => {
            eprintln!("[teste] não consegui capturar a janela: {e}");
            return;
        }
    };
    let (w, h) = (buf.width(), buf.height());
    let pixels = w * h * 4;
    let mut out = Vec::with_capacity(54 + pixels as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + pixels).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&pixels.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for p in buf.as_slice() {
        out.extend_from_slice(&[p.b, p.g, p.r, p.a]);
    }
    match std::fs::write(path, out) {
        Ok(()) => println!("[teste] print salvo em {path}"),
        Err(e) => eprintln!("[teste] não consegui salvar {path}: {e}"),
    }
}

fn print_bench_summary() {
    let times = TIMINGS.with(|t| t.borrow().clone());
    if let Some((first, rest)) = times.split_first() {
        let mut rest = rest.to_vec();
        rest.sort_by(|a, b| a.total_cmp(b));
        let median = rest.get(rest.len() / 2).copied().unwrap_or(f64::NAN);
        let worst = rest.last().copied().unwrap_or(f64::NAN);
        println!(
            "[bench] 1ª abertura {first:.1} ms · demais: mediana {median:.1} ms, pior {worst:.1} ms ({} medições)",
            rest.len()
        );
    }
    let (private, working) = win::memory_mb();
    println!("[bench] memória final, janela oculta: {private:.1} MB privados · {working:.1} MB working set");
}

/// Roda fora da thread da interface: grava a imagem em disco e manda só a miniatura.
fn store_image(dib: Vec<u8>, png: Option<Vec<u8>>, app: String) {
    let t0 = Instant::now();
    let Some(info) = win::dib_info(&dib) else { return };
    let s = win::scale();
    let Some(thumb) = win::scale_dib(&dib, (THUMB_SIZE.0 * s).round() as u32, (THUMB_SIZE.1 * s).round() as u32, true)
    else {
        eprintln!("[imagem] não consegui gerar a miniatura");
        return;
    };
    let hash = images::hash(&dib);
    if let Err(e) = images::save(hash, &dib, info.bits_offset, png.as_deref()) {
        eprintln!("[imagem] não consegui guardar em {}: {e}", images::dir().display());
        return;
    }
    let has_png = png.is_some();
    println!(
        "[imagem] {} × {} de {app}{} guardada em {:.0} ms",
        info.width,
        info.height,
        if has_png { ", com PNG," } else { "" },
        elapsed_ms(t0)
    );
    let (width, height) = (info.width, info.height);
    let _ = slint::invoke_from_event_loop(move || {
        let picture = Picture { hash, width, height, has_png, thumb: image_from(&thumb) };
        add_clip(Content::Image(picture), app);
    });
}

fn add_clip(content: Content, app: String) {
    with_state(|st| {
        // Copiar de novo um item fixado não o desafixa.
        let pinned = st
            .clips
            .iter()
            .position(|c| c.content.same(&content))
            .and_then(|pos| st.clips.remove(pos).pinned);
        st.clips.insert(0, Clip { content, app, copied: Instant::now(), pinned });
        evict(st);
        refresh(st);
    });
}

/// Tira os itens mais antigos além do limite; fixados não contam e nunca saem.
fn evict(st: &mut State) {
    let max = st.config.max_items;
    let mut kept = 0;
    let (keep, dropped): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut st.clips).into_iter().partition(|c| {
        kept += usize::from(c.pinned.is_none());
        c.pinned.is_some() || kept <= max
    });
    st.clips = keep;
    forget(dropped);
}

/// Apaga do disco as imagens dos itens que saíram do histórico.
fn forget(clips: Vec<Clip>) {
    for old in clips {
        if let Content::Image(p) = old.content {
            images::remove(p.hash);
        }
    }
}

/// "Limpar histórico" das configurações: tudo menos os fixados.
fn clear_history(st: &mut State) {
    let (keep, dropped): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut st.clips).into_iter().partition(|c| c.pinned.is_some());
    st.clips = keep;
    println!("[histórico] {} itens apagados; {} fixados mantidos", dropped.len(), st.clips.len());
    forget(dropped);
    refresh(st);
}

/// Fixa ou desafixa o item `index` da lista e devolve onde ele (ou o vizinho) foi parar,
/// para a seleção acompanhar.
fn toggle_pin(st: &mut State, index: usize) -> Option<usize> {
    let k = *st.shown.get(index)?;
    st.clips[k].pinned = match st.clips[k].pinned {
        Some(_) => None,
        None => {
            st.pin_seq += 1;
            Some(st.pin_seq)
        }
    };
    refresh(st);
    let next = st.shown.iter().position(|&i| i == k).unwrap_or(index);
    Some(next.min(st.shown.len().saturating_sub(1)))
}

fn refresh(st: &mut State) {
    let query = fold(&st.query);
    let clips = &st.clips;
    let matches = |c: &Clip| {
        if st.only_pinned && c.pinned.is_none() {
            return false;
        }
        let haystack = match &c.content {
            Content::Text(text) => fold(text),
            Content::Image(p) => fold(&image_label(p)),
        };
        query.is_empty() || haystack.contains(&query) || fold(&c.app).contains(&query)
    };
    let mut pinned: Vec<usize> = (0..clips.len()).filter(|&i| clips[i].pinned.is_some() && matches(&clips[i])).collect();
    pinned.sort_by_key(|&i| std::cmp::Reverse(clips[i].pinned));
    let recent = (0..clips.len()).filter(|&i| clips[i].pinned.is_none() && matches(&clips[i]));
    st.shown = pinned.into_iter().chain(recent).take(MAX_ROWS).collect();

    let now = Instant::now();
    let rows: Vec<ClipRow> = st
        .shown
        .iter()
        .map(|&i| {
            let clip = &st.clips[i];
            let (text, kind, thumb) = match &clip.content {
                Content::Text(text) => (preview(text), kind_of(text), slint::Image::default()),
                Content::Image(p) => (image_label(p), "image", p.thumb.clone()),
            };
            ClipRow {
                text: text.into(),
                meta: format!("{} · {}", clip.app, ago(now.duration_since(clip.copied))).into(),
                kind: kind.into(),
                thumb,
                pinned: clip.pinned.is_some(),
            }
        })
        .collect();
    st.model.set_vec(rows);

    if let Some(ui) = st.ui.upgrade() {
        let total = st.clips.len();
        let footer = if !query.is_empty() {
            format!("{} de {total} itens", st.shown.len())
        } else if st.only_pinned {
            let n = st.shown.len();
            format!("{n} {} · Tab mostra todos", if n == 1 { "fixado" } else { "fixados" })
        } else {
            format!("{total} itens · Ctrl+P fixa · Tab filtra")
        };
        ui.set_footer(footer.into());
    }
}

/// Prévia grande da imagem selecionada, lida do disco numa thread à parte.
/// Enquanto a nova não chega, a anterior continua na tela, para não piscar.
fn update_preview(st: &mut State) {
    let Some(ui) = st.ui.upgrade() else { return };
    let picture = usize::try_from(ui.get_selected())
        .ok()
        .and_then(|i| st.shown.get(i))
        .and_then(|&k| match &st.clips[k].content {
            Content::Image(p) => Some(p),
            Content::Text(_) => None,
        });
    let Some(picture) = picture else {
        clear_preview(&ui);
        return;
    };
    let hash = picture.hash;
    if PREVIEW_OF.with(|c| c.replace(Some(hash))) == Some(hash) {
        return;
    }
    let weak = st.ui.clone();
    std::thread::spawn(move || {
        let s = win::scale();
        let (w, h) = ((PREVIEW_SIZE.0 * s) as u32, (PREVIEW_SIZE.1 * s) as u32);
        let Some(pixels) = images::load(hash, false).ok().and_then(|(dib, _)| win::scale_dib(&dib, w, h, false))
        else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            // Se o usuário já foi para outro item, essa prévia não serve mais.
            if PREVIEW_OF.with(Cell::get) != Some(hash) {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_preview(image_from(&pixels));
            }
        });
    });
}

fn clear_preview(ui: &AppWindow) {
    PREVIEW_OF.with(|c| c.set(None));
    ui.set_preview(slint::Image::default());
}

fn image_from(pixels: &win::Pixels) -> slint::Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&pixels.rgba, pixels.width, pixels.height);
    slint::Image::from_rgba8(buffer)
}

fn image_label(p: &Picture) -> String {
    format!("Imagem {} × {}", p.width, p.height)
}

fn hwnd_of(window: &slint::Window) -> Option<isize> {
    let handle = window.window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Chama `f` com o HWND da janela assim que ele existir. Na primeira vez que uma janela
/// é mostrada, o Slint só cria a janela nativa depois do show(), no laço de eventos.
fn with_hwnd<C: ComponentHandle + 'static>(component: &C, f: impl FnOnce(isize) + 'static) {
    match hwnd_of(component.window()) {
        Some(hwnd) => f(hwnd),
        None => wait_for_hwnd(component.as_weak(), Box::new(f), Instant::now()),
    }
}

fn wait_for_hwnd<C: ComponentHandle + 'static>(weak: slint::Weak<C>, f: Box<dyn FnOnce(isize)>, since: Instant) {
    slint::Timer::single_shot(Duration::from_millis(10), move || {
        let Some(component) = weak.upgrade() else { return };
        match hwnd_of(component.window()) {
            Some(hwnd) => f(hwnd),
            None if since.elapsed() < Duration::from_secs(2) => wait_for_hwnd(weak, f, since),
            None => eprintln!("[janela] o Windows não criou a janela a tempo"),
        }
    });
}

/// Minúsculas e sem acento, para "cotacao" achar "cotação".
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

fn preview(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut out = lines.next().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 140 {
        out = out.chars().take(140).collect();
        out.push('…');
    } else if lines.next().is_some() {
        out.push_str(" …");
    }
    out
}

fn kind_of(text: &str) -> &'static str {
    let t = text.trim();
    if (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace) {
        "link"
    } else if t.contains('\n') && (t.contains('{') || t.contains(';') || t.contains("    ")) {
        "code"
    } else {
        "text"
    }
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => "agora".into(),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86_400),
    }
}

fn elapsed_ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

fn decimal(v: f64) -> String {
    format!("{v:.1}").replace('.', ",")
}
