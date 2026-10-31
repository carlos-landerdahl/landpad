//! Landpad — spike da Fase 0.
//!
//! Mede quanto a janela Slint pesa parada na memória e quanto demora para
//! abrir pelo atalho global. A janela nasce oculta e só aparece no atalho.

mod win;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

const MAX_CLIPS: usize = 500;
const MAX_ROWS: usize = 200;

struct Clip {
    text: String,
    app: String,
    copied: Instant,
}

struct State {
    ui: slint::Weak<AppWindow>,
    clips: Vec<Clip>,
    query: String,
    shown: Vec<usize>,
    model: Rc<VecModel<ClipRow>>,
    target: win::Target,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static OPENED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static FIRST_OPEN: Cell<bool> = const { Cell::new(true) };
    static FRAME_TIMING: Cell<bool> = const { Cell::new(false) };
    static BENCH: Cell<bool> = const { Cell::new(false) };
    static TIMINGS: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("estado não iniciado")))
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let model = Rc::new(VecModel::<ClipRow>::default());
    ui.set_rows(ModelRc::from(model.clone()));
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ui: ui.as_weak(),
            clips: Vec::new(),
            query: String::new(),
            shown: Vec::new(),
            model,
            target: win::Target::default(),
        });
    });
    with_state(refresh);

    ui.on_query_edited(|query| {
        with_state(|st| {
            st.query = query.to_string();
            refresh(st);
        })
    });

    let weak = ui.as_weak();
    ui.on_activate(move |index, _plain| {
        let Some(ui) = weak.upgrade() else { return };
        let picked = with_state(|st| {
            let k = *st.shown.get(index as usize)?;
            let mut clip = st.clips.remove(k);
            clip.copied = Instant::now();
            let text = clip.text.clone();
            st.clips.insert(0, clip);
            Some((text, st.target))
        });
        hide(&ui);
        if let Some((text, target)) = picked {
            win::paste(text, target);
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
    let hotkey = win::start(win::Handlers {
        on_clip: Box::new(|text, app| {
            let _ = slint::invoke_from_event_loop(move || add_clip(text, app));
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
                    if ui.window().is_visible() && hwnd_of(&ui) != Some(hwnd) {
                        hide(&ui);
                    }
                }
            });
        }),
    });

    match hotkey {
        Ok(Some(label)) => println!("Landpad rodando. {label} abre o histórico; Ctrl+Q no popup encerra."),
        Ok(None) => println!("Landpad rodando, mas nenhum atalho global estava livre."),
        Err(e) => eprintln!("Falha ao ligar o Landpad ao Windows: {e}"),
    }
    let (private, working) = win::memory_mb();
    println!("[memória] parado, sem janela: {private:.1} MB privados · {working:.1} MB working set");

    if std::env::args().any(|a| a == "--bench") {
        start_bench(&ui);
    }

    slint::run_event_loop_until_quit()
}

fn toggle(ui: &AppWindow, t0: Instant, target: win::Target) {
    if ui.window().is_visible() {
        hide(ui);
        return;
    }
    with_state(|st| {
        st.target = target;
        st.query.clear();
        refresh(st);
    });

    let scale = ui.window().scale_factor();
    let (x, y) = win::popup_origin((400.0 * scale) as i32, (440.0 * scale) as i32);
    ui.window().set_position(slint::PhysicalPosition::new(x, y));

    OPENED_AT.with(|c| c.set(Some(t0)));
    if let Err(e) = ui.show() {
        eprintln!("não consegui abrir a janela: {e}");
        return;
    }
    if let Some(hwnd) = hwnd_of(ui) {
        win::prepare_popup(hwnd, !BENCH.with(Cell::get));
    }
    ui.invoke_open();

    if !FRAME_TIMING.with(|c| c.get()) {
        if let Some(t0) = OPENED_AT.with(|c| c.take()) {
            report_open(ui, elapsed_ms(t0), "até show(); este renderizador não avisa o frame");
        }
    }
}

fn hide(ui: &AppWindow) {
    let _ = ui.hide();
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

/// `--bench`: enche o histórico com 300 itens e abre/fecha o popup sozinho,
/// para medir sem depender do atalho. Não rouba o foco do app em uso.
fn start_bench(ui: &AppWindow) {
    BENCH.with(|c| c.set(true));
    for i in 0..300 {
        let text = match i % 4 {
            0 => format!("Pedido {} · 32 t de soja · Rio Verde → Santos", 48_000 + i),
            1 => format!("https://exemplo.com.br/fretes/{i}"),
            2 => format!("SELECT safra, SUM(volume_t)\nFROM fretes\nWHERE id = {i};"),
            _ => format!("Reunião {i}: cotações da semana e janela de coleta"),
        };
        add_clip(text, "bench".into());
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
            } else {
                if n == 5 {
                    save_snapshot(&ui, "target/popup.bmp");
                }
                hide(&ui);
            }
            return;
        }
        print_bench_summary();
        let _ = slint::quit_event_loop();
    });
}

/// Salva o popup como BMP de 32 bits, para conferir o visual sem olhar a tela.
fn save_snapshot(ui: &AppWindow, path: &str) {
    let buf = match ui.window().take_snapshot() {
        Ok(buf) => buf,
        Err(e) => {
            eprintln!("[bench] não consegui capturar o popup: {e}");
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
        Ok(()) => println!("[bench] print do popup salvo em {path}"),
        Err(e) => eprintln!("[bench] não consegui salvar {path}: {e}"),
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

fn add_clip(text: String, app: String) {
    with_state(|st| {
        if let Some(pos) = st.clips.iter().position(|c| c.text == text) {
            st.clips.remove(pos);
        }
        st.clips.insert(0, Clip { text, app, copied: Instant::now() });
        st.clips.truncate(MAX_CLIPS);
        refresh(st);
    });
}

fn refresh(st: &mut State) {
    let query = fold(&st.query);
    st.shown = st
        .clips
        .iter()
        .enumerate()
        .filter(|(_, c)| query.is_empty() || fold(&c.text).contains(&query) || fold(&c.app).contains(&query))
        .map(|(i, _)| i)
        .take(MAX_ROWS)
        .collect();

    let now = Instant::now();
    let rows: Vec<ClipRow> = st
        .shown
        .iter()
        .map(|&i| {
            let clip = &st.clips[i];
            ClipRow {
                text: preview(&clip.text).into(),
                meta: format!("{} · {}", clip.app, ago(now.duration_since(clip.copied))).into(),
                kind: kind_of(&clip.text).into(),
            }
        })
        .collect();
    st.model.set_vec(rows);

    if let Some(ui) = st.ui.upgrade() {
        let total = st.clips.len();
        let footer = if query.is_empty() {
            format!("{total} itens · Enter cola · Esc fecha")
        } else {
            format!("{} de {total} itens", st.shown.len())
        };
        ui.set_footer(footer.into());
    }
}

fn hwnd_of(ui: &AppWindow) -> Option<isize> {
    let handle = ui.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
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
