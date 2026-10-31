//! Imagens do histórico: o original fica em disco e só a miniatura fica na memória.
//!
//! Cada imagem vira um .bmp (o DIB do clipboard com cabeçalho de arquivo, abre em
//! qualquer visualizador) e, se o app de origem ofereceu, um .png ao lado. O nome é o
//! hash do DIB, então copiar a mesma imagem de novo reaproveita o arquivo.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const FILE_HEADER: usize = 14;

pub fn dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("Landpad").join("imagens")
    })
}

/// O histórico vive só enquanto o app está aberto: apaga as imagens ao sair e,
/// se o app foi encerrado à força, ao abrir de novo.
pub fn clear() {
    match std::fs::remove_dir_all(dir()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            eprintln!("[imagem] não consegui apagar {}: {e}", dir().display())
        }
        _ => {}
    }
}

pub fn hash(dib: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    dib.hash(&mut h);
    h.finish()
}

fn path(hash: u64, ext: &str) -> PathBuf {
    dir().join(format!("{hash:016x}.{ext}"))
}

pub fn save(hash: u64, dib: &[u8], bits_offset: usize, png: Option<&[u8]>) -> io::Result<()> {
    let mut bmp = Vec::with_capacity(FILE_HEADER + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((FILE_HEADER + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&((FILE_HEADER + bits_offset) as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    std::fs::create_dir_all(dir())?;
    std::fs::write(path(hash, "bmp"), bmp)?;
    if let Some(png) = png {
        std::fs::write(path(hash, "png"), png)?;
    }
    Ok(())
}

/// Devolve o DIB e, se houver, o PNG guardados para essa imagem.
pub fn load(hash: u64, with_png: bool) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
    let mut dib = std::fs::read(path(hash, "bmp"))?;
    if dib.len() <= FILE_HEADER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bmp vazio"));
    }
    dib.drain(..FILE_HEADER);
    let png = if with_png { std::fs::read(path(hash, "png")).ok() } else { None };
    Ok((dib, png))
}

pub fn remove(hash: u64) {
    let _ = std::fs::remove_file(path(hash, "bmp"));
    let _ = std::fs::remove_file(path(hash, "png"));
}
