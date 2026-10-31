//! A janela de configurações. É criada ao abrir e liberada ao fechar, para não pesar
//! no dia a dia; toda mudança vale na hora e é salva em seguida.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::config::{self, Theme, MAX_ITEMS, MIN_ITEMS};
use crate::win::{self, Hotkey, Position};
use crate::{
    apply_config, clear_history, cleared_count, hwnd_of, logo_image, save_snapshot, undo_clear, with_hwnd, with_state,
    SettingsWindow, State,
};

const RECORD_HINT: &str = "Aperte o atalho…";

thread_local! {
    static WINDOW: RefCell<Option<SettingsWindow>> = const { RefCell::new(None) };
    /// No `--settings-test` a janela fica fora da tela e não disputa o foco.
    static TESTING: Cell<bool> = const { Cell::new(false) };
}

fn current() -> Option<SettingsWindow> {
    WINDOW.with(|w| w.borrow().as_ref().map(|w| w.clone_strong()))
}

/// Abre as configurações, ou traz para a frente se já estiverem abertas.
pub fn open() {
    if let Some(w) = current() {
        let _ = w.show();
        if let Some(hwnd) = hwnd_of(w.window()) {
            win::bring_to_front(hwnd);
        }
        return;
    }
    let Ok(w) = SettingsWindow::new() else {
        eprintln!("[config] não consegui abrir a janela de configurações");
        return;
    };
    let scale = win::scale();
    w.set_logo(logo_image(32.0, scale));
    w.set_logo_large(logo_image(56.0, scale));
    w.set_version(env!("CARGO_PKG_VERSION").into());
    w.set_record_hint(RECORD_HINT.into());
    fill(&w);
    wire(&w);
    if let Err(e) = w.show() {
        eprintln!("[config] não consegui mostrar as configurações: {e}");
        return;
    }
    let (testing, opened) = (TESTING.with(Cell::get), Instant::now());
    with_hwnd(&w, move |hwnd| {
        win::set_window_icon(hwnd);
        if !testing {
            win::show_now(hwnd);
            keep_in_front(hwnd, opened);
        }
    });
    WINDOW.with(|slot| *slot.borrow_mut() = Some(w));
}

/// O Slint só termina de mostrar a janela depois do primeiro quadro, que na primeira
/// abertura demora (fontes, layout). Confere a cada 30 ms até ela estar na frente, por
/// no máximo 2 s, para não roubar o foco se você já tiver ido para outro app.
fn keep_in_front(hwnd: isize, opened: Instant) {
    slint::Timer::single_shot(Duration::from_millis(30), move || {
        let ms = opened.elapsed().as_millis();
        if win::is_foreground(hwnd) {
            println!("[config] configurações na frente em {ms} ms");
        } else if ms > 2000 {
            eprintln!("[config] o Windows não deixou as configurações virem para a frente");
        } else {
            win::bring_to_front(hwnd);
            keep_in_front(hwnd, opened);
        }
    });
}

/// Atualiza a janela aberta com o estado atual (pausa pela bandeja, atalhos, tema…).
pub fn sync() {
    if let Some(w) = current() {
        fill(&w);
    }
}

fn fill(w: &SettingsWindow) {
    with_state(|st| {
        let c = &st.config;
        w.set_dark_mode(c.dark());
        w.set_start_with_windows(config::autostart());
        w.set_autostart_managed(win::is_packaged());
        w.set_theme_index(match c.theme {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        });
        w.set_position_index(match c.position {
            Position::Caret => 0,
            Position::Mouse => 1,
            Position::Center => 2,
        });
        w.set_quick_paste(c.quick_paste);
        w.set_paused(st.paused);
        let apps: Vec<SharedString> = c.ignored_apps.iter().map(SharedString::from).collect();
        w.set_ignored_apps(ModelRc::new(VecModel::from(apps)));
        w.set_max_items(c.max_items as i32);
        w.set_keep_images(c.keep_images);
        w.set_check_updates(c.check_updates);
        w.set_persist_pinned(c.persist_pinned);
        w.set_persist_history(c.persist_history);
        w.set_portable(config::portable_dir().is_some());
        // Na versão da Microsoft Store quem atualiza é a loja.
        w.set_show_updates(!win::is_packaged());
        let (version, ready) = match win::pending_update() {
            Some((version, state)) => (version, state == crate::update::UpdateState::Ready),
            None => (String::new(), false),
        };
        w.set_update_version(version.into());
        w.set_update_ready(ready);
        w.set_history_status(history_status(st).into());
        w.set_can_undo(cleared_count(st) > 0);
        let (popup, shot, color, prev) = default_hotkeys();
        w.set_hotkeys_default(
            c.popup_hotkey == popup && c.shot_hotkey == shot && c.color_hotkey == color && c.prev_hotkey == prev,
        );
        w.set_default_hotkeys(
            format!("{}, {}, {} e {}", popup.label(), shot.label(), color.label(), prev.label()).into(),
        );
        let (label, status) = hotkey_text(c.prev_hotkey, st.hotkeys.prev);
        w.set_prev_hotkey(label.into());
        w.set_prev_status(status.into());
        let (label, status) = hotkey_text(c.popup_hotkey, st.hotkeys.popup);
        w.set_popup_hotkey(label.into());
        w.set_popup_status(status.into());
        let (label, status) = hotkey_text(c.shot_hotkey, st.hotkeys.shot);
        w.set_shot_hotkey(label.into());
        w.set_shot_status(status.into());
        let (label, status) = hotkey_text(c.color_hotkey, st.hotkeys.color);
        w.set_color_hotkey(label.into());
        w.set_color_status(status.into());
    });
}

fn history_status(st: &State) -> String {
    let cleared = cleared_count(st);
    if cleared > 0 {
        let items = if cleared == 1 { "item apagado" } else { "itens apagados" };
        return format!("{cleared} {items}. Dá para desfazer por alguns segundos.");
    }
    let pinned = st.clips.iter().filter(|c| c.pinned.is_some()).count();
    let total = st.clips.len();
    let items = if total == 1 { "item" } else { "itens" };
    format!("{total} {items} agora, {pinned} fixados. Os fixados são mantidos.")
}

/// Os atalhos de fábrica, para o botão "Restaurar" e a descrição dele.
fn default_hotkeys() -> win::HotkeySet {
    (Hotkey::DEFAULT_POPUP, Hotkey::DEFAULT_SHOT, Hotkey::DEFAULT_COLOR, Hotkey::DEFAULT_PREV)
}

/// O atalho escolhido e, se o Windows não aceitou, o porquê.
fn hotkey_text(wanted: Hotkey, active: Option<Hotkey>) -> (String, String) {
    let status = match active {
        Some(k) if k == wanted => String::new(),
        Some(k) => format!("Em uso por outro app. Por enquanto vale {}.", k.label()),
        None => "Em uso por outro app. Escolha outro atalho.".into(),
    };
    (wanted.label(), status)
}

fn wire(w: &SettingsWindow) {
    let weak = w.as_weak();
    w.on_edited(move || {
        let Some(w) = weak.upgrade() else { return };
        with_state(|st| {
            let mut c = st.config.clone();
            c.theme = [Theme::System, Theme::Light, Theme::Dark][w.get_theme_index().clamp(0, 2) as usize];
            c.position = [Position::Caret, Position::Mouse, Position::Center][w.get_position_index().clamp(0, 2) as usize];
            c.quick_paste = w.get_quick_paste();
            c.max_items = (w.get_max_items().max(0) as usize).clamp(MIN_ITEMS, MAX_ITEMS);
            c.keep_images = w.get_keep_images();
            c.check_updates = w.get_check_updates();
            c.persist_pinned = w.get_persist_pinned();
            c.persist_history = w.get_persist_history();
            st.paused = w.get_paused();
            apply_config(st, c);
        });
        fill(&w);
    });

    let weak = w.as_weak();
    w.on_autostart_toggled(move |on| {
        if let Err(e) = config::set_autostart(on) {
            eprintln!("[config] iniciar com o Windows: {e}");
        }
        if let Some(w) = weak.upgrade() {
            w.set_start_with_windows(config::autostart());
        }
    });

    let weak = w.as_weak();
    w.on_record(move |which| {
        let Some(w) = weak.upgrade() else { return };
        let Some(hwnd) = hwnd_of(w.window()) else { return };
        win::suspend_hotkeys();
        let weak = weak.clone();
        win::start_recording(hwnd, move |key| {
            // A tecla chega no meio do processamento da janela: o resto fica para depois.
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || finish_recording(&weak, which, key));
        });
    });

    let weak = w.as_weak();
    w.on_cancel_record(move || {
        let which = weak.upgrade().map_or(0, |w| w.get_recording());
        finish_recording(&weak, which, None);
    });

    let weak = w.as_weak();
    w.on_add_app(move |name| {
        let name = name.trim();
        let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).trim().to_string();
        if name.is_empty() {
            return;
        }
        with_state(|st| {
            if st.config.ignored_apps.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
                return;
            }
            let mut c = st.config.clone();
            c.ignored_apps.push(name);
            apply_config(st, c);
        });
        if let Some(w) = weak.upgrade() {
            fill(&w);
        }
    });

    let weak = w.as_weak();
    w.on_remove_app(move |index| {
        with_state(|st| {
            let mut c = st.config.clone();
            if (index as usize) < c.ignored_apps.len() {
                c.ignored_apps.remove(index as usize);
                apply_config(st, c);
            }
        });
        if let Some(w) = weak.upgrade() {
            fill(&w);
        }
    });

    let weak = w.as_weak();
    w.on_clear_history(move || {
        with_state(clear_history);
        if let Some(w) = weak.upgrade() {
            fill(&w);
        }
    });

    w.on_open_repo(|| win::open_url(&format!("https://github.com/{}", crate::update::REPO)));
    w.on_apply_update(crate::update::apply_now);

    let weak = w.as_weak();
    w.on_undo_clear(move || {
        with_state(undo_clear);
        if let Some(w) = weak.upgrade() {
            fill(&w);
        }
    });

    let weak = w.as_weak();
    w.on_reset_hotkeys(move || {
        let keys = default_hotkeys();
        with_state(|st| {
            let mut c = st.config.clone();
            (c.popup_hotkey, c.shot_hotkey, c.color_hotkey, c.prev_hotkey) = keys;
            apply_config(st, c);
        });
        win::set_hotkeys(keys);
        if let Some(w) = weak.upgrade() {
            fill(&w);
        }
    });

    let weak = w.as_weak();
    w.window().on_close_requested(move || {
        if let Some(w) = weak.upgrade() {
            if w.get_recording() != 0 {
                finish_recording(&weak, w.get_recording(), None);
            }
        }
        // Solta a janela depois que o Slint terminar de fechá-la.
        let _ = slint::invoke_from_event_loop(|| {
            WINDOW.with(|slot| slot.borrow_mut().take());
        });
        slint::CloseRequestResponse::HideWindow
    });
}

/// Fim da gravação: Esc ou Cancelar (`key` None) mantêm o atalho atual; uma tecla válida
/// vira o novo atalho. Nos dois casos os atalhos globais voltam a valer.
fn finish_recording(weak: &slint::Weak<SettingsWindow>, which: i32, key: Option<Hotkey>) {
    let Some(w) = weak.upgrade() else { return };
    if w.get_recording() == 0 {
        return;
    }
    if let Some(key) = key {
        let problem = with_state(|st| {
            let c = &st.config;
            let others = [(1, c.popup_hotkey), (2, c.shot_hotkey), (3, c.color_hotkey), (4, c.prev_hotkey)];
            if !key.is_valid() {
                Some("Use Ctrl, Alt, Shift ou Win junto".to_string())
            } else if others.iter().any(|&(id, k)| id != which && k == key) {
                Some(format!("{} já é outro atalho do Landpad", key.label()))
            } else {
                None
            }
        });
        // Combinação que não serve: avisa e continua esperando outra.
        if let Some(problem) = problem {
            w.set_record_hint(problem.into());
            return;
        }
        with_state(|st| {
            let mut c = st.config.clone();
            match which {
                1 => c.popup_hotkey = key,
                2 => c.shot_hotkey = key,
                3 => c.color_hotkey = key,
                _ => c.prev_hotkey = key,
            }
            apply_config(st, c);
        });
    }
    if let Some(hwnd) = hwnd_of(w.window()) {
        win::stop_recording(hwnd);
    }
    w.set_recording(0);
    w.set_record_hint(RECORD_HINT.into());
    let keys = with_state(|st| (st.config.popup_hotkey, st.config.shot_hotkey, st.config.color_hotkey, st.config.prev_hotkey));
    win::set_hotkeys(keys);
    fill(&w);
}

/// `--settings-test`: abre as configurações fora da tela, fotografa cada página em
/// target/settings-N.bmp e fecha o app.
pub fn self_test() {
    TESTING.with(|t| t.set(true));
    // Uma versão nova de mentira, para a página Sobre sair com o aviso e o botão.
    win::announce_update("9.9.9", crate::update::UpdateState::Ready);
    open();
    let Some(w) = current() else { return };
    w.window().set_position(slint::PhysicalPosition::new(-32000, -32000));
    let page = Cell::new(0);
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(w) = current() else { return };
        let n = page.get();
        if n == 0 {
            match hwnd_of(w.window()) {
                Some(hwnd) => {
                    let (big, small) = win::window_icons(hwnd);
                    let ok = |h: isize| if h != 0 { "ok" } else { "faltando" };
                    println!("[teste] ícone da janela: grande {}, pequeno {}", ok(big), ok(small));
                }
                None => println!("[teste] a janela nativa ainda não existe"),
            }
        }
        if n > 0 {
            save_snapshot(w.window(), &format!("target/settings-{}.bmp", n - 1));
        }
        if n == 5 {
            let _ = slint::quit_event_loop();
            return;
        }
        w.set_page(n);
        page.set(n + 1);
    });
}

/// `--memory-test`: abre e fecha o popup; abre e fecha as configurações 5 vezes passando
/// por todas as páginas; faz 5 capturas de tela (sem janela, ver shot::memory_probe), cada
/// uma seguida do popup com a prévia da imagem. Mede a memória a cada ciclo: crescer a
/// cada ciclo seria vazamento; subir uma vez e estabilizar é cache ou heap não devolvido.
pub fn memory_test(ui: &crate::AppWindow) {
    TESTING.with(|t| t.set(true));
    let ui = ui.as_weak();
    let tick = Cell::new(0u32);
    let closed: RefCell<Option<slint::Weak<SettingsWindow>>> = RefCell::new(None);
    let report = |what: &str| {
        let (private, working) = win::memory_mb();
        println!("[memória] {what}: {private:.1} MB privados · {working:.1} MB working set");
    };
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(ui) = ui.upgrade() else { return };
        let n = tick.get();
        tick.set(n + 1);
        match n {
            0 => crate::toggle(&ui, Instant::now(), win::Target::default()),
            1 => {
                crate::hide(&ui);
                report("popup aberto e fechado");
            }
            _ if n < 2 + 5 * 8 => {
                let (cycle, step) = ((n - 2) / 8 + 1, (n - 2) % 8);
                match step {
                    0 => {
                        open();
                        if let Some(w) = current() {
                            w.window().set_position(slint::PhysicalPosition::new(-32000, -32000));
                        }
                    }
                    1..=5 => {
                        if let Some(w) = current() {
                            w.set_page(step as i32 - 1);
                            let _ = w.window().take_snapshot();
                        }
                        if step == 5 {
                            report(&format!("configurações abertas ({cycle}ª vez)"));
                        }
                    }
                    6 => {
                        if let Some(w) = current() {
                            let _ = w.hide();
                            *closed.borrow_mut() = Some(w.as_weak());
                        }
                        WINDOW.with(|slot| slot.borrow_mut().take());
                    }
                    _ => {
                        let freed = closed.borrow().as_ref().is_some_and(|w| w.upgrade().is_none());
                        report(&format!("configurações fechadas ({cycle}ª vez, janela liberada: {})", if freed { "sim" } else { "não" }));
                    }
                }
            }
            _ if n < 42 + 5 * 3 => {
                let (cycle, step) = ((n - 42) / 3 + 1, (n - 42) % 3);
                match step {
                    // Thread própria, como a captura de verdade.
                    0 => match std::thread::spawn(crate::shot::memory_probe).join().ok().flatten() {
                        Some(dib) => crate::store_image(dib, None, "captura".into()),
                        None => eprintln!("[memória] a captura de teste falhou"),
                    },
                    1 => crate::toggle(&ui, Instant::now(), win::Target::default()),
                    _ => {
                        crate::hide(&ui);
                        report(&format!("captura + popup com a prévia ({cycle}ª vez)"));
                    }
                }
            }
            _ => {
                let _ = slint::quit_event_loop();
            }
        }
    });
}
