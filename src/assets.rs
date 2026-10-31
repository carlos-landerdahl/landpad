//! `--export-assets <pasta>`: grava o ícone em PNG nos tamanhos que o README e o pacote
//! da Microsoft Store (MSIX) pedem, a partir do mesmo desenho do app (ver logo.rs).

use std::path::Path;

use crate::{logo, shot};

/// Nome do arquivo, tamanho da tela em pixels e tamanho do ícone centralizado nela.
const ASSETS: &[(&str, u32, u32, u32)] = &[
    // Lista de apps do menu Iniciar e barra de tarefas.
    ("Square44x44Logo.png", 44, 44, 44),
    // Tamanhos exatos que o Windows usa na barra de tarefas e no Explorer, com e sem a
    // placa de fundo (unplated): o ícone já tem o próprio quadrado azul.
    ("Square44x44Logo.targetsize-16.png", 16, 16, 16),
    ("Square44x44Logo.targetsize-24.png", 24, 24, 24),
    ("Square44x44Logo.targetsize-32.png", 32, 32, 32),
    ("Square44x44Logo.targetsize-48.png", 48, 48, 48),
    ("Square44x44Logo.targetsize-256.png", 256, 256, 256),
    ("Square44x44Logo.targetsize-16_altform-unplated.png", 16, 16, 16),
    ("Square44x44Logo.targetsize-24_altform-unplated.png", 24, 24, 24),
    ("Square44x44Logo.targetsize-32_altform-unplated.png", 32, 32, 32),
    ("Square44x44Logo.targetsize-48_altform-unplated.png", 48, 48, 48),
    ("Square44x44Logo.targetsize-256_altform-unplated.png", 256, 256, 256),
    // Blocos do menu Iniciar e página da loja.
    ("Square150x150Logo.png", 150, 150, 96),
    ("Wide310x150Logo.png", 310, 150, 96),
    ("StoreLogo.png", 50, 50, 50),
    // O ícone do README.
    ("icon-128.png", 128, 128, 128),
];

pub fn export(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for &(name, width, height, size) in ASSETS {
        let icon = logo::rgba(size, false);
        let mut canvas = vec![0u8; (width * height * 4) as usize];
        let (left, top) = ((width - size) / 2, (height - size) / 2);
        for y in 0..size {
            let src = (y * size * 4) as usize;
            let dst = (((top + y) * width + left) * 4) as usize;
            canvas[dst..dst + (size * 4) as usize].copy_from_slice(&icon[src..src + (size * 4) as usize]);
        }
        shot::write_png(&canvas, width, height, &dir.join(name))?;
    }
    // Ícone do instalador e do desinstalador: aqui cabe o de 256 px, que o .exe dispensa.
    let ico = logo::ico(&[16, 20, 24, 32, 40, 48, 64, 96, 128, 256]);
    std::fs::write(dir.join("landpad.ico"), ico).map_err(|e| e.to_string())?;
    println!("[assets] {} imagens e o landpad.ico gravados em {}", ASSETS.len(), dir.display());
    Ok(())
}
