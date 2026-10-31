//! Registro do que o Landpad faz por trás (abertura, atualizações, erros) em
//! %LOCALAPPDATA%\Landpad\landpad.log. O app não tem console: sem isso, um problema na
//! máquina de outra pessoa seria invisível. Nada do que a pessoa copia vai para o log.
//! Guarda só as últimas linhas, para o arquivo não crescer.

use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const KEEP_LINES: usize = 300;

static LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Landpad").join("landpad.log")
}

pub fn info(message: impl AsRef<str>) {
    println!("{}", message.as_ref());
    append(message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
    append(&format!("ERRO {}", message.as_ref()));
}

fn append(message: &str) {
    let _guard = LOCK.lock();
    let path = path();
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {message}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    lines.push(&line);
    let start = lines.len().saturating_sub(KEEP_LINES);
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(&path, lines[start..].join("\r\n") + "\r\n");
}
