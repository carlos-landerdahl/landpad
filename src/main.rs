//! Landpad — gerenciador de área de transferência para Windows.
//!
//! Fica na bandeja; o popup do histórico nasce oculto e só aparece no atalho. A janela
//! de configurações e a captura de tela existem só enquanto estão abertas.

// Sem janela de console ao iniciar com o Windows; aberto de um terminal, os logs vão para ele.
#![windows_subsystem = "windows"]

mod assets;
mod color;
mod config;
mod images;
mod log;
mod logo;
mod settings;
mod shot;
mod store;
mod update;
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
    /// O texto já em minúsculas e sem acento, para a busca não refazer isso a cada tecla.
    /// Limitado: um texto enorme é achado pelo começo.
    search: String,
    /// text, link, code, image, color, email, phone, doc, path ou json; decidido uma vez.
    kind: &'static str,
    /// Nome dado a um fixado, que aparece no lugar do texto.
    name: Option<String>,
}

/// Quanto do texto entra na busca.
const SEARCH_CHARS: usize = 20_000;
/// Teto de memória para os textos do histórico (fixados fora); imagens ficam em disco.
const MAX_TEXT_TOTAL: usize = 48 * 1024 * 1024;
/// Por quanto tempo "Limpar histórico" pode ser desfeito.
const UNDO_WINDOW: Duration = Duration::from_secs(20);

impl Clip {
    fn new(content: Content, app: String, pinned: Option<u64>) -> Clip {
        let kind = match &content {
            Content::Text(text) => kind_of(text),
            Content::Image(_) => "image",
        };
        let mut clip = Clip { content, app, copied: Instant::now(), pinned, search: String::new(), kind, name: None };
        clip.reindex();
        clip
    }

    /// Refaz o texto de busca: o nome, se houver, e o começo do conteúdo.
    fn reindex(&mut self) {
        let body = match &self.content {
            Content::Text(text) => fold(&text.chars().take(SEARCH_CHARS).collect::<String>()),
            Content::Image(p) => fold(&image_label(p)),
        };
        self.search = match &self.name {
            Some(name) => format!("{} {body}", fold(name)),
            None => body,
        };
    }

    /// O texto do item, se for texto.
    fn text(&self) -> Option<&str> {
        match &self.content {
            Content::Text(text) => Some(text),
            Content::Image(_) => None,
        }
    }

    /// Quanto o item ocupa na memória, mais ou menos.
    fn bytes(&self) -> usize {
        match &self.content {
            Content::Text(text) => text.len() + self.search.len(),
            // A miniatura, em pixels lógicos; o original está em disco.
            Content::Image(_) => 36 * 22 * 4 * 4,
        }
    }
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
    /// O que "Limpar histórico" tirou, enquanto ainda dá para desfazer; o número diz qual
    /// limpeza foi, para o timer não apagar a sobra de uma limpeza mais nova.
    cleared: Option<(u64, Vec<Clip>)>,
    clear_seq: u64,
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
    /// Pausa antes de ler a imagem da prévia; reiniciar o timer cancela a leitura anterior.
    static PREVIEW_TIMER: slint::Timer = slint::Timer::default();
    /// Junta várias mudanças seguidas numa gravação só do histórico guardado.
    static PERSIST_TIMER: slint::Timer = slint::Timer::default();
    /// Os modos de teste não leem nem gravam o histórico guardado de verdade.
    static PERSIST: Cell<bool> = const { Cell::new(false) };
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
    if flag("--check-update") {
        update::report();
        return Ok(());
    }
    if let Some(dir) = std::env::args().skip_while(|a| a != "--export-assets").nth(1) {
        if let Err(e) = assets::export(std::path::Path::new(&dir)) {
            eprintln!("[assets] {e}");
        }
        return Ok(());
    }
    // Os modos de teste rodam ao lado do app de verdade, com uma pasta de imagens só deles.
    let testing = flag("--bench") || flag("--settings-test") || flag("--memory-test") || flag("--shot-now");
    if testing {
        images::use_test_dir();
    } else {
        if !win::first_instance() {
            eprintln!("O Landpad já está aberto: procure o ícone na bandeja.");
            return Ok(());
        }
        let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default();
        log::info(format!("Landpad {} aberto de {exe}; instalado: {}", update::current(), update::installed()));
        if update::install_pending() {
            // A versão nova baixada ontem é instalada agora, com o histórico ainda vazio;
            // o instalador abre o Landpad de novo quando terminar.
            return Ok(());
        }
    }
    // Sobra de uma execução encerrada à força.
    images::clear();
    let first_run = !config::exists();
    let config = Config::load();
    let ui = AppWindow::new()?;
    ui.set_logo(logo_image(24.0, win::scale()));
    ui.set_dark_mode(config.dark());
    ui.set_quick_paste(config.quick_paste);
    let model = Rc::new(VecModel::<ClipRow>::default());
    ui.set_rows(ModelRc::from(model.clone()));
    win::set_capture_rules(false, config.keep_images, &config.ignored_apps);
    let hotkey_set = (config.popup_hotkey, config.shot_hotkey, config.color_hotkey, config.prev_hotkey);
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
            cleared: None,
            clear_seq: 0,
        });
    });
    warm_up(&ui);
    if !testing {
        PERSIST.with(|p| p.set(true));
        with_state(restore_saved);
    }
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
    ui.on_remove(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(next) = with_state(|st| remove_clip(st, index as usize)) else { return };
        // Fora do with_state: mudar a seleção dispara selection-changed, que usa o estado.
        ui.invoke_select(next as i32);
        with_state(update_preview);
    });

    let weak = ui.as_weak();
    ui.on_activate(move |index, copy_only| {
        if let Some(ui) = weak.upgrade() {
            activate(&ui, index as usize, copy_only, None);
        }
    });

    let weak = ui.as_weak();
    ui.on_paste_as(move |index, mode| {
        if let Some(ui) = weak.upgrade() {
            activate(&ui, index as usize, false, Some(mode.as_str()));
        }
    });

    let weak = ui.as_weak();
    ui.on_open_link(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let target = with_state(|st| {
            let k = *st.shown.get(index as usize)?;
            let clip = &st.clips[k];
            let text = clip.text()?.trim();
            match clip.kind {
                "link" if text.starts_with("www.") => Some(Open::Url(format!("https://{text}"))),
                "link" => Some(Open::Url(text.to_string())),
                "email" => Some(Open::Url(format!("mailto:{text}"))),
                "path" => Some(Open::Path(text.to_string())),
                _ => None,
            }
        });
        match target {
            Some(Open::Url(url)) => {
                hide(&ui);
                win::open_url(&url);
            }
            Some(Open::Path(path)) => {
                hide(&ui);
                win::reveal_in_explorer(std::path::Path::new(&path));
            }
            // Ctrl+Enter num item sem nada para abrir faz o mesmo que Enter.
            None => activate(&ui, index as usize, false, None),
        }
    });

    let weak = ui.as_weak();
    ui.on_activate_range(move |lo, hi, copy_only| {
        let Some(ui) = weak.upgrade() else { return };
        let joined = with_state(|st| {
            let texts: Vec<&str> = (lo.max(0) as usize..=hi.max(0) as usize)
                .filter_map(|i| st.shown.get(i))
                .filter_map(|&k| st.clips[k].text())
                .collect();
            (texts.join("\n"), st.target)
        });
        hide(&ui);
        let (text, target) = joined;
        if text.is_empty() {
            return;
        }
        if copy_only {
            win::copy(win::Captured::Text(text));
        } else {
            win::paste(win::Captured::Text(text), target);
        }
    });

    ui.on_save_image(|index| {
        let picture = with_state(|st| {
            let k = *st.shown.get(index as usize)?;
            match &st.clips[k].content {
                Content::Image(p) => Some((p.hash, p.has_png)),
                Content::Text(_) => None,
            }
        });
        let Some((hash, has_png)) = picture else { return };
        std::thread::spawn(move || match images::load(hash, has_png) {
            Ok((dib, _)) => shot::save_dib_to_pictures(&dib),
            Err(e) => eprintln!("[imagem] não consegui ler a imagem guardada: {e}"),
        });
    });

    ui.on_rename(move |index, name| {
        with_state(|st| {
            let Some(&k) = st.shown.get(index as usize) else { return };
            let name = name.trim();
            st.clips[k].name = (!name.is_empty()).then(|| name.to_string());
            st.clips[k].reindex();
            refresh(st);
            update_preview(st);
            persist(st);
        });
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
    let notifier = ui.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, slint::RenderingState::AfterRendering) {
            return;
        }
        if let Some(t0) = OPENED_AT.with(|c| c.take()) {
            let ms = elapsed_ms(t0);
            let _ = slint::invoke_from_event_loop(move || report_open(ms, "até o primeiro frame"));
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
        on_color: Box::new(start_color),
        on_prev: Box::new(|target| {
            let _ = slint::invoke_from_event_loop(move || paste_previous(target));
        }),
        on_tray: Box::new(move |command| {
            let weak = on_tray.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                match command {
                    win::TrayCommand::Open => toggle(&ui, Instant::now(), win::last_app()),
                    win::TrayCommand::Update => update::apply_now(),
                    win::TrayCommand::Shot => start_shot(),
                    win::TrayCommand::Color => start_color(),
                    win::TrayCommand::PastePrevious => paste_previous(win::last_app()),
                    win::TrayCommand::Pause => {
                        with_state(|st| {
                            st.paused = !st.paused;
                            let config = st.config.clone();
                            apply_config(st, config);
                        });
                        settings::sync();
                    }
                    win::TrayCommand::Settings => settings::open(),
                    win::TrayCommand::RevealSaved => shot::reveal_saved(),
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
    match win::start(handlers, options, hotkey_set) {
        Ok(keys) => {
            with_state(|st| st.hotkeys = keys);
            report_hotkeys(keys);
            if first_run && !testing {
                welcome(keys);
            }
        }
        Err(e) => eprintln!("Falha ao ligar o Landpad ao Windows: {e}"),
    }
    let (private, working) = win::memory_mb();
    println!("[memória] parado, sem janela: {private:.1} MB privados · {working:.1} MB working set");

    if !testing && !win::is_packaged() {
        schedule_update_checks();
    }
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
    if flag("--shot-now") {
        shot_now();
    }

    let result = slint::run_event_loop_until_quit();
    win::shutdown();
    // Os fixados (e o histórico, se pedido) vão para o disco cifrados; o resto some com as imagens.
    if let Some(items) = with_state(snapshot) {
        write_snapshot(items);
    }
    images::clear();
    result
}

/// Dez segundos depois de abrir e depois uma vez por dia, se estiver ligado.
fn schedule_update_checks() {
    slint::Timer::single_shot(Duration::from_secs(10), check_for_update);
    let daily = Box::leak(Box::new(slint::Timer::default()));
    daily.start(slint::TimerMode::Repeated, Duration::from_secs(24 * 60 * 60), check_for_update);
}

fn check_for_update() {
    if with_state(|st| st.config.check_updates) {
        update::check(|version, state| {
            win::announce_update(version, state);
            // A página Sobre, se estiver aberta, passa a mostrar a versão e o botão de atualizar.
            let _ = slint::invoke_from_event_loop(settings::sync);
        });
    }
}

/// Na primeira vez, o Landpad abre direto na bandeja: um balão conta onde ele está e os
/// atalhos. Gravar as preferências marca que a apresentação já foi feita.
fn welcome(keys: win::Hotkeys) {
    let label = |k: Option<win::Hotkey>, fallback: &str| k.map_or(fallback.to_string(), |k| k.label());
    let text = format!(
        "{} abre o histórico e {} captura a tela. Clique no ícone para abrir agora.",
        label(keys.popup, "O atalho"),
        label(keys.shot, "o outro atalho")
    );
    win::balloon("O Landpad está aqui", &text, win::TrayCommand::Open);
    with_state(|st| st.config.save());
}

fn start_shot() {
    open_capture(shot::Mode::Shot);
}

fn start_color() {
    open_capture(shot::Mode::Color);
}

/// Captura e conta-gotas abrem a mesma tela congelada; na captura, C troca para o
/// conta-gotas. O que sair dali entra no histórico.
fn open_capture(mode: shot::Mode) {
    shot::open(
        mode,
        |dib| store_image(dib, None, "captura".into()),
        // Uma cor do conta-gotas ou o texto reconhecido pelo OCR, com o "app" de origem.
        |text, source| {
            let _ = slint::invoke_from_event_loop(move || add_clip(Content::Text(text), source.into()));
        },
    );
}

/// `--shot-now`: abre a captura de verdade meio segundo depois de iniciar, sem atalho nem
/// bandeja, para um script dirigir o mouse e o teclado e reproduzir um fluxo. O app fecha
/// quando a captura termina, ou em 60 s.
fn shot_now() {
    slint::Timer::single_shot(Duration::from_millis(500), || {
        shot::open(
            shot::Mode::Shot,
            |dib| {
                println!("[shot-now] captura concluída: {} bytes", dib.len());
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            },
            |text, source| println!("[shot-now] texto de {source}: {text:?}"),
        );
    });
    let watchdog = Box::leak(Box::new(slint::Timer::default()));
    watchdog.start(slint::TimerMode::Repeated, Duration::from_secs(60), || {
        println!("[shot-now] tempo esgotado");
        let _ = slint::quit_event_loop();
    });
}

fn report_hotkeys(keys: win::Hotkeys) {
    let label = |k: Option<win::Hotkey>, action: &str, what: &str| match k {
        Some(k) => format!("{} {action}", k.label()),
        None => format!("sem atalho para {what}"),
    };
    println!(
        "Landpad rodando. {}, {}, {}, {}; o ícone na bandeja tem o resto.",
        label(keys.popup, "abre o histórico", "o histórico"),
        label(keys.shot, "captura a tela", "a captura"),
        label(keys.color, "pega uma cor", "o conta-gotas"),
        label(keys.prev, "cola o item anterior", "colar o anterior")
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
        ui.set_quick_paste(st.config.quick_paste);
    }
    evict(st);
    refresh(st);
    if changed {
        st.config.save();
        persist(st);
    }
}

/// O que vai para o disco, decidido na thread da interface; as imagens são lidas depois.
struct Pending {
    content: PendingContent,
    app: String,
    name: Option<String>,
    pinned: bool,
    age_secs: u64,
}

enum PendingContent {
    Text(String),
    Image { hash: u64, has_png: bool },
}

/// Os itens a guardar, pelas preferências: fixados primeiro, do mais antigo fixado ao mais
/// novo, para a ordem voltar igual; depois os demais, se o histórico inteiro for guardado.
/// Imagens só entram fixadas. `None` quando nada deve ser gravado (modos de teste).
fn snapshot(st: &mut State) -> Option<Vec<Pending>> {
    if !PERSIST.with(Cell::get) {
        return None;
    }
    let c = &st.config;
    let now = Instant::now();
    let pending = |clip: &Clip| Pending {
        content: match &clip.content {
            Content::Text(text) => PendingContent::Text(text.clone()),
            Content::Image(p) => PendingContent::Image { hash: p.hash, has_png: p.has_png },
        },
        app: clip.app.clone(),
        name: clip.name.clone(),
        pinned: clip.pinned.is_some(),
        age_secs: now.duration_since(clip.copied).as_secs(),
    };
    let mut items = Vec::new();
    if c.persist_pinned {
        let mut pinned: Vec<&Clip> = st.clips.iter().filter(|clip| clip.pinned.is_some()).collect();
        pinned.sort_by_key(|clip| clip.pinned);
        items.extend(pinned.into_iter().map(pending));
    }
    if c.persist_history {
        items.extend(st.clips.iter().filter(|clip| clip.pinned.is_none() && clip.text().is_some()).map(pending));
    }
    Some(items)
}

/// Agenda a gravação para daqui a um segundo: fixar três itens seguidos grava uma vez.
fn persist(_st: &State) {
    if !PERSIST.with(Cell::get) {
        return;
    }
    PERSIST_TIMER.with(|t| {
        t.start(slint::TimerMode::SingleShot, Duration::from_secs(1), || {
            if let Some(items) = with_state(snapshot) {
                std::thread::spawn(move || write_snapshot(items));
            }
        });
    });
}

/// Lê as imagens do disco, monta o arquivo e grava. Fora da thread da interface.
fn write_snapshot(items: Vec<Pending>) {
    if items.is_empty() {
        store::remove();
        return;
    }
    let t0 = Instant::now();
    let saved: Vec<store::SavedClip> = items
        .into_iter()
        .filter_map(|p| {
            let content = match p.content {
                PendingContent::Text(text) => store::SavedContent::Text(text),
                PendingContent::Image { hash, has_png } => {
                    let (dib, png) = images::load(hash, has_png).ok()?;
                    store::SavedContent::Image { dib, png }
                }
            };
            Some(store::SavedClip { content, app: p.app, name: p.name, pinned: p.pinned, age_secs: p.age_secs })
        })
        .collect();
    let n = saved.len();
    match store::save(&saved) {
        Ok(()) => println!("[histórico] {n} itens guardados em {:.0} ms", elapsed_ms(t0)),
        Err(e) => log::error(format!("[histórico] não consegui guardar em {}: {e}", store::path().display())),
    }
}

/// Na partida: o que foi guardado na sessão anterior volta para o histórico, respeitando
/// as preferências de agora (desligar "guardar o histórico" descarta o que não é fixado).
fn restore_saved(st: &mut State) {
    let saved = store::load();
    if saved.is_empty() {
        return;
    }
    let (persist_pinned, persist_history) = (st.config.persist_pinned, st.config.persist_history);
    let now = Instant::now();
    let scale = win::scale();
    let mut restored = 0;
    for item in saved {
        if (item.pinned && !persist_pinned) || (!item.pinned && !persist_history) {
            continue;
        }
        let content = match item.content {
            store::SavedContent::Text(text) => Content::Text(text),
            store::SavedContent::Image { dib, png } => {
                let Some(info) = win::dib_info(&dib) else { continue };
                let (w, h) = ((THUMB_SIZE.0 * scale).round() as u32, (THUMB_SIZE.1 * scale).round() as u32);
                let Some(thumb) = win::scale_dib(&dib, w, h, true) else { continue };
                let hash = images::hash(&dib);
                if images::save(hash, &dib, info.bits_offset, png.as_deref()).is_err() {
                    continue;
                }
                let picture =
                    Picture { hash, width: info.width, height: info.height, has_png: png.is_some(), thumb: image_from(&thumb) };
                Content::Image(picture)
            }
        };
        let pinned = item.pinned.then(|| {
            st.pin_seq += 1;
            st.pin_seq
        });
        let mut clip = Clip::new(content, item.app, pinned);
        clip.name = item.name;
        clip.reindex();
        clip.copied = now.checked_sub(Duration::from_secs(item.age_secs)).unwrap_or(now);
        st.clips.push(clip);
        restored += 1;
    }
    st.clips.sort_by(|a, b| b.copied.cmp(&a.copied));
    evict(st);
    println!("[histórico] {restored} itens restaurados da sessão anterior");
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
            report_open(elapsed_ms(t0), "até show(); este renderizador não avisa o frame");
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

fn report_open(ms: f64, how: &str) {
    let first = FIRST_OPEN.with(|c| c.replace(false));
    let (private, working) = win::memory_mb();
    let label = if first { "1ª abertura" } else { "abertura" };
    println!("[{label}] {ms:.1} ms ({how}) · {private:.1} MB privados · {working:.1} MB working set");
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
    // Cores aparecem com a bolinha da cor no lugar do ícone.
    add_clip(Content::Text("rgb(46, 205, 180)".into()), "cor".into());
    add_clip(Content::Text("#2E63FF".into()), "cor".into());
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
                // O print do popup sai com o código SQL selecionado, para mostrar a prévia de texto.
                if n == 4 {
                    ui.invoke_select(1);
                }
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

/// A primeira abertura do popup pagava 35 a 47 ms carregando fontes e rasterizando glifos.
/// Um quadro fora da tela logo depois de iniciar faz esse trabalho antes do primeiro atalho.
fn warm_up(ui: &AppWindow) {
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(300), move || {
        let Some(ui) = weak.upgrade() else { return };
        let t0 = Instant::now();
        match ui.window().take_snapshot() {
            Ok(_) => println!("[aquecimento] fontes e glifos prontos em {:.0} ms", elapsed_ms(t0)),
            Err(e) => eprintln!("[aquecimento] não consegui desenhar o primeiro quadro: {e}"),
        }
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
        st.clips.insert(0, Clip::new(content, app, pinned));
        evict(st);
        refresh(st);
        if st.config.persist_history || pinned.is_some() {
            persist(st);
        }
    });
}

/// Tira os itens mais antigos além do limite de quantidade ou de memória; fixados não
/// contam e nunca saem.
fn evict(st: &mut State) {
    let max = st.config.max_items;
    let (mut kept, mut bytes) = (0, 0);
    let (keep, dropped): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut st.clips).into_iter().partition(|c| {
        if c.pinned.is_some() {
            return true;
        }
        kept += 1;
        bytes += c.bytes();
        kept <= max && bytes <= MAX_TEXT_TOTAL
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

/// "Limpar histórico" das configurações: tudo menos os fixados. O que saiu fica guardado
/// por alguns segundos para o botão "Desfazer"; só depois as imagens são apagadas do disco.
fn clear_history(st: &mut State) {
    let (keep, dropped): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut st.clips).into_iter().partition(|c| c.pinned.is_some());
    st.clips = keep;
    println!("[histórico] {} itens apagados; {} fixados mantidos", dropped.len(), st.clips.len());
    // Uma limpeza em cima de outra ainda desfazível: a anterior vai embora de vez.
    if let Some((_, older)) = st.cleared.take() {
        forget(older);
    }
    st.clear_seq += 1;
    let seq = st.clear_seq;
    st.cleared = Some((seq, dropped));
    refresh(st);
    persist(st);
    slint::Timer::single_shot(UNDO_WINDOW, move || {
        let expired = with_state(|st| match st.cleared.take() {
            Some((s, clips)) if s == seq => Some(clips),
            other => {
                st.cleared = other;
                None
            }
        });
        if let Some(clips) = expired {
            forget(clips);
            settings::sync();
        }
    });
}

/// "Desfazer" depois de limpar: os itens voltam para onde estavam.
fn undo_clear(st: &mut State) {
    let Some((_, clips)) = st.cleared.take() else { return };
    println!("[histórico] {} itens restaurados", clips.len());
    st.clips.extend(clips);
    st.clips.sort_by(|a, b| b.copied.cmp(&a.copied));
    evict(st);
    refresh(st);
    persist(st);
}

/// Quantos itens estão esperando o "Desfazer".
fn cleared_count(st: &State) -> usize {
    st.cleared.as_ref().map_or(0, |(_, clips)| clips.len())
}

/// Delete no popup: tira o item `index` da lista (fixado ou não) e devolve onde a seleção
/// deve ficar: na mesma posição, que agora é do vizinho de baixo, ou na última.
fn remove_clip(st: &mut State, index: usize) -> Option<usize> {
    let k = *st.shown.get(index)?;
    let removed = st.clips.remove(k);
    if removed.pinned.is_some() || st.config.persist_history {
        persist(st);
    }
    forget(vec![removed]);
    refresh(st);
    Some(index.min(st.shown.len().saturating_sub(1)))
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
    persist(st);
    let next = st.shown.iter().position(|&i| i == k).unwrap_or(index);
    Some(next.min(st.shown.len().saturating_sub(1)))
}

/// O que a busca pede além do texto: `link:`, `img:`, `cor:`, `code:`, `texto:`, `email:`,
/// `tel:`, `doc:`, `arquivo:`, `json:` filtram por tipo; `de:chrome` filtra pelo app.
#[derive(Default)]
struct Query {
    text: String,
    kinds: Vec<&'static str>,
    apps: Vec<String>,
}

fn parse_query(raw: &str) -> Query {
    let mut q = Query::default();
    let mut words = Vec::new();
    let folded = fold(raw);
    for word in folded.split_whitespace() {
        let kind = match word {
            "link:" | "links:" => "link",
            "img:" | "imagem:" | "imagens:" => "image",
            "cor:" | "cores:" => "color",
            "code:" | "codigo:" => "code",
            "texto:" => "text",
            "email:" | "e-mail:" => "email",
            "tel:" | "fone:" => "phone",
            "doc:" | "cpf:" | "cnpj:" => "doc",
            "arquivo:" | "pasta:" => "path",
            "json:" => "json",
            _ => "",
        };
        if !kind.is_empty() {
            q.kinds.push(kind);
        } else if let Some(app) = word.strip_prefix("de:").or_else(|| word.strip_prefix("app:")) {
            if !app.is_empty() {
                q.apps.push(app.to_string());
            }
        } else {
            words.push(word);
        }
    }
    q.text = words.join(" ");
    q
}

enum Open {
    Url(String),
    Path(String),
}

fn refresh(st: &mut State) {
    let q = parse_query(&st.query);
    let query = q.text.clone();
    let clips = &st.clips;
    let matches = |c: &Clip| {
        if st.only_pinned && c.pinned.is_none() {
            return false;
        }
        if !q.kinds.is_empty() && !q.kinds.contains(&c.kind) {
            return false;
        }
        let app = fold(&c.app);
        if !q.apps.is_empty() && !q.apps.iter().any(|a| app.contains(a.as_str())) {
            return false;
        }
        query.is_empty() || c.search.contains(&query) || app.contains(&query)
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
            let (text, thumb) = match &clip.content {
                Content::Text(text) => (preview(text), slint::Image::default()),
                Content::Image(p) => (image_label(p), p.thumb.clone()),
            };
            let swatch = match clip.text() {
                Some(text) if clip.kind == "color" => color::parse(text).map(|(r, g, b)| slint::Color::from_rgb_u8(r, g, b)),
                _ => None,
            };
            ClipRow {
                text: clip.name.clone().unwrap_or(text).into(),
                meta: format!("{} · {}", clip.app, ago(now.duration_since(clip.copied))).into(),
                kind: clip.kind.into(),
                thumb,
                swatch: swatch.unwrap_or_default(),
                pinned: clip.pinned.is_some(),
                name: clip.name.clone().unwrap_or_default().into(),
            }
        })
        .collect();
    st.model.set_vec(rows);

    if let Some(ui) = st.ui.upgrade() {
        let total = st.clips.len();
        let footer = if !st.query.trim().is_empty() {
            format!("{} de {total} itens · filtros: link: img: cor: code: de:app", st.shown.len())
        } else if st.only_pinned {
            let n = st.shown.len();
            format!("{n} {} · Tab mostra todos", if n == 1 { "fixado" } else { "fixados" })
        } else {
            format!("{total} itens · Enter cola · Shift+Enter copia · Del apaga · Ctrl+P fixa")
        };
        ui.set_footer(footer.into());
    }
}

/// Enter, clique ou menu: põe o item no topo e no clipboard. Sem `copy_only`, cola no app
/// de origem. `transform` cola uma variação do texto ("upper", "lower", "oneline", "trim")
/// sem mudar o item guardado.
fn activate(ui: &AppWindow, index: usize, copy_only: bool, transform: Option<&str>) {
    let picked = with_state(|st| {
        let k = *st.shown.get(index)?;
        Some((take_to_top(st, k, transform), st.target))
    });
    hide(ui);
    match picked {
        Some((Ok(payload), _)) if copy_only => win::copy(payload),
        Some((Ok(payload), target)) => win::paste(payload, target),
        Some((Err(e), _)) => eprintln!("[colar] não consegui ler a imagem guardada: {e}"),
        None => {}
    }
}

/// O item `k` vira o mais recente e volta como carga para o clipboard.
fn take_to_top(st: &mut State, k: usize, transform: Option<&str>) -> std::io::Result<win::Captured> {
    let mut clip = st.clips.remove(k);
    clip.copied = Instant::now();
    let payload = match &clip.content {
        Content::Text(text) => Ok(win::Captured::Text(match transform {
            Some(mode) => transform_text(mode, text),
            None => text.clone(),
        })),
        Content::Image(p) => images::load(p.hash, p.has_png).map(|(dib, png)| win::Captured::Image { dib, png }),
    };
    st.clips.insert(0, clip);
    refresh(st);
    payload
}

/// Atalho ou menu da bandeja: cola o item anterior ao que está no clipboard, sem abrir o
/// popup. Ele vira o mais recente, então apertar de novo alterna entre os dois últimos.
fn paste_previous(target: win::Target) {
    let payload = with_state(|st| (st.clips.len() >= 2).then(|| take_to_top(st, 1, None)));
    match payload {
        Some(Ok(payload)) => win::paste(payload, target),
        Some(Err(e)) => eprintln!("[colar] não consegui ler a imagem guardada: {e}"),
        None => println!("[colar] não há item anterior para colar"),
    }
}

/// As variações do menu "Colar como".
fn transform_text(mode: &str, text: &str) -> String {
    match mode {
        "upper" => text.to_uppercase(),
        "lower" => text.to_lowercase(),
        "oneline" => text.split_whitespace().collect::<Vec<_>>().join(" "),
        "trim" => text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        "digits" => text.chars().filter(char::is_ascii_digit).collect(),
        "json-pretty" => reformat_json(text, true),
        "json-min" => reformat_json(text, false),
        _ => text.to_string(),
    }
}

/// Reindenta (ou compacta) um JSON olhando só a estrutura, sem interpretar os valores:
/// o que está entre aspas passa intacto, e o resto ganha ou perde quebras e espaços.
fn reformat_json(text: &str, pretty: bool) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    let newline = |out: &mut String, depth: usize| {
        if pretty {
            out.push('\n');
            out.extend(std::iter::repeat_n("  ", depth));
        }
    };
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // Coleção vazia fica numa linha só.
                while chars.peek().is_some_and(|n| n.is_whitespace()) {
                    chars.next();
                }
                if chars.peek().is_some_and(|&n| n == '}' || n == ']') {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => {
                out.push(c);
                if pretty {
                    out.push(' ');
                }
            }
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// Cartão de prévia do item selecionado: o começo de um texto longo, na hora; ou a imagem,
/// lida do disco numa thread à parte depois de uma pausa curta, para segurar a seta não
/// ler um arquivo por item. Enquanto a nova não chega, a anterior continua na tela.
fn update_preview(st: &mut State) {
    let Some(ui) = st.ui.upgrade() else { return };
    let clip = usize::try_from(ui.get_selected()).ok().and_then(|i| st.shown.get(i)).map(|&k| &st.clips[k]);
    match clip.map(|c| (&c.content, c)) {
        None => clear_preview(&ui),
        Some((Content::Text(text), clip)) => {
            PREVIEW_OF.with(|c| c.set(None));
            ui.set_preview(slint::Image::default());
            ui.set_preview_mono(matches!(clip.kind, "code" | "json"));
            // Um fixado com nome não mostra o texto na linha: a prévia mostra sempre.
            ui.set_preview_text(text_preview(text, clip.name.is_some()).into());
        }
        Some((Content::Image(p), _)) => {
            ui.set_preview_text("".into());
            let hash = p.hash;
            if PREVIEW_OF.with(|c| c.replace(Some(hash))) == Some(hash) {
                return;
            }
            let weak = st.ui.clone();
            PREVIEW_TIMER.with(|t| {
                t.start(slint::TimerMode::SingleShot, Duration::from_millis(50), move || load_preview(weak.clone(), hash));
            });
        }
    }
}

fn load_preview(weak: slint::Weak<AppWindow>, hash: u64) {
    // O item pode ter mudado durante a pausa.
    if PREVIEW_OF.with(Cell::get) != Some(hash) {
        return;
    }
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

/// O começo de um texto que não cabe na linha: até 12 linhas de até 200 caracteres.
/// Vazio quando a linha já mostra tudo, a não ser com `always`.
fn text_preview(text: &str, always: bool) -> String {
    let fits = !text.trim().contains('\n') && text.trim().chars().count() <= 50;
    if fits && !always {
        return String::new();
    }
    let mut lines: Vec<String> = text
        .lines()
        .take(12)
        .map(|l| {
            let l = l.trim_end();
            if l.chars().count() > 200 {
                l.chars().take(200).chain("…".chars()).collect()
            } else {
                l.to_string()
            }
        })
        .collect();
    if text.lines().count() > 12 {
        lines.push("…".into());
    }
    lines.join("\n")
}

fn clear_preview(ui: &AppWindow) {
    PREVIEW_OF.with(|c| c.set(None));
    ui.set_preview(slint::Image::default());
    ui.set_preview_text("".into());
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

/// Que tipo de texto é: decide o ícone, as ações do menu e os filtros da busca.
fn kind_of(text: &str) -> &'static str {
    let t = text.trim();
    let one_word = !t.is_empty() && !t.contains(char::is_whitespace);
    if color::parse(t).is_some() {
        "color"
    } else if one_word && (t.starts_with("http://") || t.starts_with("https://") || t.starts_with("www.")) {
        "link"
    } else if one_word && is_email(t) {
        "email"
    } else if is_doc(t) {
        "doc"
    } else if is_phone(t) {
        "phone"
    } else if is_path(t) {
        "path"
    } else if is_json(t) {
        "json"
    } else if t.contains('\n') && (t.contains('{') || t.contains(';') || t.contains("    ")) {
        "code"
    } else {
        "text"
    }
}

/// nome@dominio.algo, sem espaços; o suficiente para o botão "Escrever e-mail".
fn is_email(t: &str) -> bool {
    let Some((user, domain)) = t.split_once('@') else { return false };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '-')
}

/// CPF (11 dígitos) ou CNPJ (14), com ou sem pontuação: 000.000.000-00, 00.000.000/0000-00.
fn is_doc(t: &str) -> bool {
    let digits = t.chars().filter(char::is_ascii_digit).count();
    (digits == 11 || digits == 14) && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '/'))
}

/// Telefone: só dígitos, espaços e + ( ) -, com 8 a 15 dígitos.
fn is_phone(t: &str) -> bool {
    let digits = t.chars().filter(char::is_ascii_digit).count();
    (8..=15).contains(&digits) && t.chars().all(|c| c.is_ascii_digit() || matches!(c, ' ' | '+' | '(' | ')' | '-'))
}

/// Caminho do Windows: C:\pasta\arquivo ou \\servidor\pasta, numa linha só.
fn is_path(t: &str) -> bool {
    if t.contains('\n') || t.len() > 1000 {
        return false;
    }
    let drive = t.len() >= 3 && t.as_bytes()[0].is_ascii_alphabetic() && &t[1..3] == ":\\";
    drive || t.starts_with("\\\\")
}

/// Um objeto ou lista JSON: começa e termina com os colchetes certos e tem uma chave entre aspas.
fn is_json(t: &str) -> bool {
    let obj = t.starts_with('{') && t.ends_with('}');
    let list = t.starts_with('[') && t.ends_with(']');
    (obj || list) && t.contains("\":") || (obj || list) && t.contains("\" :")
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

