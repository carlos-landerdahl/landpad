//! Atualização: uma vez por dia o app pergunta ao GitHub qual é a última release. A
//! pergunta não leva nada sobre o usuário nem sobre o que ele copia (só o nome e a versão
//! do app, no User-Agent que a API do GitHub exige), e dá para desligar.
//!
//! O Landpad baixa a versão nova em segundo plano e confere o SHA-256 publicado junto.
//! Instalado pelo instalador, ele a instala sozinho na próxima vez que abrir, antes de
//! mostrar qualquer coisa: nessa hora o histórico está vazio e ninguém perde nada. Rodando
//! do zip portátil, o clique no aviso abre o instalador, e daí em diante ele se atualiza
//! sozinho. Tudo fica registrado em landpad.log (ver log.rs).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

use crate::log;

const REPO: &str = "carlos-landerdahl/landpad";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Argumentos do Inno Setup para trocar a versão sem mostrar nada.
const SILENT: [&str; 3] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"];

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// Como a versão nova está, para o aviso da bandeja dizer o que o clique vai fazer.
#[derive(Clone, Copy, PartialEq)]
pub enum UpdateState {
    /// Baixada e conferida; instala sozinha na próxima abertura.
    Ready,
    /// Baixada, mas este Landpad veio do zip: o clique abre o instalador.
    Portable,
    /// O download falhou; o clique tenta de novo.
    NotDownloaded,
}

/// A versão desta cópia. `LANDPAD_TEST_VERSION` finge uma versão mais velha, para testar
/// a atualização sem publicar nada (ver docs/desenvolvimento.md).
pub fn current() -> String {
    std::env::var("LANDPAD_TEST_VERSION").unwrap_or_else(|_| VERSION.to_string())
}

/// A versão nova encontrada e, se já foi baixada e conferida, o instalador dela.
static FOUND: Mutex<Option<(Release, Option<PathBuf>)>> = Mutex::new(None);

/// Consulta numa thread à parte. Se houver versão nova, baixa e chama `on_found` com a
/// versão e o estado dela.
pub fn check(on_found: impl FnOnce(&str, UpdateState) + Send + 'static) {
    std::thread::spawn(move || {
        let current = current();
        let release = match latest() {
            Ok(Some(r)) if newer(&r.version, &current) => r,
            Ok(Some(_)) => return log::info(format!("[atualização] o Landpad {current} é a versão mais recente")),
            Ok(None) => return log::info("[atualização] ainda não há versões publicadas no GitHub"),
            Err(e) => return log::error(format!("[atualização] não consegui consultar o GitHub: {e}")),
        };
        log::info(format!("[atualização] versão {} disponível (esta é a {current})", release.version));
        let installer = match download(&release.version) {
            Ok(path) => {
                log::info(format!("[atualização] {} baixada e conferida em {}", release.version, path.display()));
                Some(path)
            }
            Err(e) => {
                log::error(format!("[atualização] não consegui baixar a {}: {e}", release.version));
                None
            }
        };
        let state = match (&installer, installed()) {
            (None, _) => UpdateState::NotDownloaded,
            (Some(_), true) => UpdateState::Ready,
            (Some(_), false) => UpdateState::Portable,
        };
        let version = release.version.clone();
        *FOUND.lock().unwrap() = Some((release, installer));
        on_found(&version, state);
    });
}

/// `--check-update`: consulta o GitHub agora, baixa o instalador da última versão e
/// confere o SHA-256, como a atualização faria, mas não guarda nem instala nada.
pub fn report() {
    match latest() {
        Ok(Some(r)) => {
            let status = if newer(&r.version, VERSION) { "mais nova que esta" } else { "esta já é a mais recente" };
            println!("[atualização] última versão publicada: {} ({status}); instalado: {}", r.version, installed());
            match fetch_verified(&r.version) {
                Ok(setup) => println!(
                    "[atualização] instalador de {:.1} MB baixado e conferido com o SHA-256 publicado",
                    setup.len() as f64 / (1024.0 * 1024.0)
                ),
                Err(e) => eprintln!("[atualização] o download falharia: {e}"),
            }
        }
        Ok(None) => println!("[atualização] ainda não há versões publicadas no GitHub"),
        Err(e) => eprintln!("[atualização] não consegui consultar o GitHub: {e}"),
    }
}

/// Clique no aviso ou no menu da bandeja. Instalado, troca a versão sem mostrar nada e
/// reabre o Landpad; vindo do zip, abre o instalador para a pessoa passar para a versão
/// instalada. Se o download tinha falhado, tenta de novo; só se não der, abre a página da
/// release. O app fecha sozinho quando o instalador começa.
pub fn apply_now() {
    let Some((release, installer)) = FOUND.lock().unwrap().clone() else { return };
    std::thread::spawn(move || {
        let path = match installer {
            Some(path) => Ok(path),
            None => download(&release.version),
        };
        let started = match path {
            Ok(path) => run_installer(&path, installed()),
            Err(e) => {
                log::error(format!("[atualização] o download falhou de novo ({e}); abrindo a página da release"));
                false
            }
        };
        if started {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        } else {
            crate::win::open_url(&release.url);
        }
    });
}

/// Na partida: se há uma versão mais nova já baixada e conferida, instala antes de abrir
/// qualquer janela. Devolve true se o instalador foi chamado e o app deve sair já.
pub fn install_pending() -> bool {
    // Instaladores de atualizações anteriores: rodaram da pasta temporária e não conseguem
    // apagar a si mesmos.
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Landpad-Setup-") && name.ends_with(".exe") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(pending_dir()) else { return false };
    let mut newest: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(version) = name.strip_prefix("Landpad-Setup-").and_then(|n| n.strip_suffix(".exe")) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if newer(version, &current()) && newest.as_ref().is_none_or(|(v, _)| newer(version, v)) {
            newest = Some((version.to_string(), path));
        } else {
            // Já instalada (ou mais velha): só ocupa espaço.
            let _ = std::fs::remove_file(&path);
        }
    }
    match newest {
        Some((version, path)) if installed() => {
            log::info(format!("[atualização] instalando a {version} antes de abrir"));
            run_installer(&path, true)
        }
        _ => false,
    }
}

/// Tira o instalador da fila antes de rodar: se a instalação falhar, o app não fica
/// tentando de novo a cada abertura; a checagem do dia seguinte baixa outra vez.
/// `silent` troca a versão sem mostrar nada; sem ele, a pessoa vê o assistente.
fn run_installer(path: &Path, silent: bool) -> bool {
    let running = std::env::temp_dir().join(path.file_name().unwrap_or_default());
    let path = if std::fs::rename(path, &running).is_ok() { running } else { path.to_path_buf() };
    // O log do instalador fica ao lado da fila, para diagnosticar se algo der errado.
    let installer_log = format!("/LOG={}", pending_dir().with_file_name("atualizacao.log").display());
    let mut command = std::process::Command::new(&path);
    if silent {
        command.args(SILENT);
    }
    match command.arg(installer_log).spawn() {
        Ok(_) => {
            let how = if silent { "em silêncio" } else { "com o assistente" };
            log::info(format!("[atualização] instalador aberto {how}: {}", path.display()));
            true
        }
        Err(e) => {
            log::error(format!("[atualização] não consegui abrir o instalador {}: {e}", path.display()));
            false
        }
    }
}

/// Instalado pelo instalador (e não rodando do zip portátil ou da pasta do projeto): só aí
/// o app se atualiza sozinho. O instalador sempre põe o desinstalador ao lado do Landpad;
/// o zip não. Olhar o arquivo, e não o caminho, funciona em qualquer pasta de usuário.
pub fn installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe")))
        .is_some_and(|uninstaller| uninstaller.exists())
}

fn pending_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Landpad").join("atualizacao")
}

fn latest() -> Result<Option<Release>, String> {
    let (status, body) = request("api.github.com", &format!("/repos/{REPO}/releases/latest"))?;
    // Sem nenhuma release publicada, a API responde 404.
    if status == 404 {
        return Ok(None);
    }
    let body = String::from_utf8(body).map_err(|e| e.to_string())?;
    let Some(tag) = json_string(&body, "tag_name") else { return Ok(None) };
    Ok(Some(Release {
        version: tag.trim_start_matches('v').to_string(),
        url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
    }))
}

/// Baixa o instalador e o .sha256 publicados na release e só guarda se baterem.
fn download(version: &str) -> Result<PathBuf, String> {
    let setup = fetch_verified(version)?;
    let dir = pending_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("Landpad-Setup-{version}.exe"));
    std::fs::write(&path, setup).map_err(|e| e.to_string())?;
    Ok(path)
}

/// O instalador da versão, já conferido com o SHA-256 publicado junto.
fn fetch_verified(version: &str) -> Result<Vec<u8>, String> {
    let name = format!("Landpad-Setup-{version}.exe");
    let base = format!("/{REPO}/releases/download/v{version}/{name}");
    let (status, setup) = request("github.com", &base)?;
    if status != 200 {
        return Err(format!("o GitHub respondeu {status} para o instalador"));
    }
    let (status, sums) = request("github.com", &format!("{base}.sha256"))?;
    if status != 200 {
        return Err(format!("o GitHub respondeu {status} para o .sha256"));
    }
    let sums = String::from_utf8_lossy(&sums);
    let expected = sums.split_whitespace().next().unwrap_or_default().to_lowercase();
    if sha256_hex(&setup)? != expected {
        return Err("o arquivo baixado não confere com o SHA-256 publicado".into());
    }
    Ok(setup)
}

fn sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut hash = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut hash) };
    if status.is_err() {
        return Err(format!("o Windows não calculou o SHA-256 ({:#x})", status.0));
    }
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// O primeiro valor de texto de uma chave; basta para o campo que lemos, sem precisar de
/// uma biblioteca de JSON.
fn json_string(body: &str, key: &str) -> Option<String> {
    let start = body.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = body[start..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// Compara "1.2.10" com "1.2.9" número a número.
fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parse(candidate) > parse(current)
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// GET por HTTPS com o WinHTTP do Windows (segue redirecionamentos, usa o proxy do
/// sistema). Devolve o código de status e o corpo, até 64 MB.
fn request(host: &str, path: &str) -> Result<(u32, Vec<u8>), String> {
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (agent, host, path) = (wide(&format!("Landpad/{VERSION}")), wide(host), wide(path));
    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen falhou".into());
        }
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 30000);
        let connect = Handle(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0));
        if connect.0.is_null() {
            return Err("sem conexão".into());
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest falhou".into());
        }
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n".encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(|e| e.to_string())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| e.to_string())?;

        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| e.to_string())?;

        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|e| e.to_string())?;
            if available == 0 {
                break;
            }
            if body.len() > 64 << 20 {
                return Err("resposta grande demais".into());
            }
            let mut chunk = vec![0u8; available as usize];
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr() as *mut c_void, available, &mut read)
                .map_err(|e| e.to_string())?;
            body.extend_from_slice(&chunk[..read as usize]);
        }
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compara_versoes() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn le_a_tag() {
        let body = r#"{"url":"x","html_url":"y","tag_name": "v0.2.0","name":"Landpad"}"#;
        assert_eq!(json_string(body, "tag_name").as_deref(), Some("v0.2.0"));
        assert_eq!(json_string(r#"{"message":"Not Found"}"#, "tag_name"), None);
    }

    #[test]
    fn sha256_conhecido() {
        assert_eq!(
            sha256_hex(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
