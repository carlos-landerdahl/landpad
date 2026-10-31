//! Preferências do Landpad. Ficam num arquivo pequeno em %APPDATA%\Landpad e são salvas a
//! cada mudança. O histórico, de propósito, não vai para o disco: só as preferências.

use std::ffi::c_void;
use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use crate::win::{Hotkey, Position};

pub const MIN_ITEMS: usize = 50;
pub const MAX_ITEMS: usize = 5000;

#[derive(Clone, Copy, PartialEq)]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Clone, PartialEq)]
pub struct Config {
    pub theme: Theme,
    pub position: Position,
    pub popup_hotkey: Hotkey,
    pub shot_hotkey: Hotkey,
    /// Ctrl+Shift+1..9 com o popup aberto.
    pub quick_paste: bool,
    /// Nomes de processo (sem .exe) cujas cópias nunca entram no histórico.
    pub ignored_apps: Vec<String>,
    pub max_items: usize,
    pub keep_images: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            theme: Theme::System,
            position: Position::Caret,
            popup_hotkey: Hotkey::DEFAULT_POPUP,
            shot_hotkey: Hotkey::DEFAULT_SHOT,
            quick_paste: true,
            ignored_apps: Vec::new(),
            max_items: 500,
            keep_images: true,
        }
    }
}

fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Landpad").join("config.ini")
}

/// Se os apps do Windows estão no tema escuro (Configurações > Personalização > Cores).
pub fn system_dark() -> bool {
    let (mut light, mut size) = (1u32, 4u32);
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut light as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && light == 0
}

impl Config {
    pub fn dark(&self) -> bool {
        match self.theme {
            Theme::System => system_dark(),
            Theme::Light => false,
            Theme::Dark => true,
        }
    }

    /// Lê o arquivo; o que faltar ou não fizer sentido fica no padrão.
    pub fn load() -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(path()) else { return c };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "tema" => {
                    c.theme = match value {
                        "claro" => Theme::Light,
                        "escuro" => Theme::Dark,
                        _ => Theme::System,
                    }
                }
                "posicao" => {
                    c.position = match value {
                        "mouse" => Position::Mouse,
                        "centro" => Position::Center,
                        _ => Position::Caret,
                    }
                }
                "atalho_historico" => c.popup_hotkey = Hotkey::parse(value).unwrap_or(c.popup_hotkey),
                "atalho_captura" => c.shot_hotkey = Hotkey::parse(value).unwrap_or(c.shot_hotkey),
                "colar_por_numero" => c.quick_paste = value != "nao",
                "apps_ignorados" => {
                    c.ignored_apps = value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "max_itens" => {
                    if let Ok(n) = value.parse::<usize>() {
                        c.max_items = n.clamp(MIN_ITEMS, MAX_ITEMS);
                    }
                }
                "guardar_imagens" => c.keep_images = value != "nao",
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        let yes_no = |b: bool| if b { "sim" } else { "nao" };
        let text = format!(
            "# Preferências do Landpad. O app regrava este arquivo a cada mudança.\n\
             tema = {}\nposicao = {}\natalho_historico = {}\natalho_captura = {}\n\
             colar_por_numero = {}\napps_ignorados = {}\nmax_itens = {}\nguardar_imagens = {}\n",
            match self.theme {
                Theme::System => "sistema",
                Theme::Light => "claro",
                Theme::Dark => "escuro",
            },
            match self.position {
                Position::Caret => "cursor",
                Position::Mouse => "mouse",
                Position::Center => "centro",
            },
            self.popup_hotkey.to_config(),
            self.shot_hotkey.to_config(),
            yes_no(self.quick_paste),
            self.ignored_apps.join(", "),
            self.max_items,
            yes_no(self.keep_images),
        );
        let path = path();
        let result = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, text));
        if let Err(e) = result {
            eprintln!("[config] não consegui salvar {}: {e}", path.display());
        }
    }
}

/// "Iniciar com o Windows" é uma entrada em HKCU\...\Run, a mesma que o Gerenciador de
/// Tarefas mostra em Aplicativos de inicialização. O registro é a fonte da verdade.
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Landpad");

pub fn autostart() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    unsafe {
        let status = if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(std::iter::once(0)).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(value.as_ptr() as *const c_void),
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE)
        };
        if status == ERROR_SUCCESS || (!on && status == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(format!("o registro do Windows recusou (erro {})", status.0))
        }
    }
}
