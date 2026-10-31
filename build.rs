use std::path::PathBuf;

// O mesmo desenho do ícone que o app usa em tempo de execução.
#[path = "src/logo.rs"]
mod logo;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("falha ao compilar ui/app.slint");
    embed_icon();
}

/// Embute o ícone no .exe, que é o que o Explorer, o menu Iniciar e a barra de tarefas
/// usam antes de o app abrir qualquer janela. Tamanhos até 96 px: o de 256 sozinho
/// somaria 256 KB ao executável.
fn embed_icon() {
    println!("cargo:rerun-if-changed=src/logo.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("landpad.ico");
    std::fs::write(&ico, ico_bytes(&[16, 20, 24, 32, 40, 48, 64, 96])).expect("falha ao gravar o ícone");
    let rc = out.join("landpad.rc");
    let ico_path = ico.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{ico_path}\"\n")).expect("falha ao gravar o .rc");
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("falha ao embutir o ícone");
}

/// Arquivo .ico com uma imagem DIB de 32 bits (com alfa) por tamanho.
fn ico_bytes(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| dib(s)).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        let byte = if size >= 256 { 0 } else { size as u8 };
        ico.extend_from_slice(&[byte, byte, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(image.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        ico.extend_from_slice(&image);
    }
    ico
}

/// Cabeçalho com altura dobrada (cor + máscara), pixels BGRA de baixo para cima e a
/// máscara AND zerada: quem manda é o alfa.
fn dib(size: u32) -> Vec<u8> {
    let rgba = logo::rgba(size, false);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    out
}
