//! O que sobrevive ao fechar o Landpad: os fixados e, se a pessoa pedir, o histórico todo.
//!
//! Tudo vai num arquivo só, cifrado com a conta do Windows (DPAPI, a mesma proteção que o
//! Windows usa para as senhas do Credential Manager): outro usuário da máquina, ou um
//! backup copiado para fora, não lê nada. Imagens entram só quando fixadas, porque um
//! print pesa dezenas de MB. O formato é nosso, simples e com versão no cabeçalho.

use std::ffi::c_void;
use std::path::PathBuf;

use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};

const MAGIC: &[u8; 4] = b"LPD1";

/// Um item do histórico como vai para o disco.
pub struct SavedClip {
    /// Texto, ou (DIB, PNG opcional) de uma imagem.
    pub content: SavedContent,
    pub app: String,
    pub name: Option<String>,
    pub pinned: bool,
    /// Há quanto tempo foi copiado, para o "há 2 h" continuar certo depois de reabrir.
    pub age_secs: u64,
}

pub enum SavedContent {
    Text(String),
    Image { dib: Vec<u8>, png: Option<Vec<u8>> },
}

pub fn path() -> PathBuf {
    crate::config::local_dir().join("historico.bin")
}

/// Grava os itens, cifrados. Troca o arquivo só depois de escrever o novo inteiro, para
/// um desligamento no meio não deixar um arquivo pela metade.
pub fn save(items: &[SavedClip]) -> Result<(), String> {
    let plain = encode(items);
    let secret = protect(&plain)?;
    let path = path();
    let tmp = path.with_extension("tmp");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&tmp, secret).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Lê o arquivo, se existir e for desta conta; qualquer problema vira lista vazia com aviso.
pub fn load() -> Vec<SavedClip> {
    let path = path();
    let Ok(secret) = std::fs::read(&path) else { return Vec::new() };
    match unprotect(&secret).and_then(|plain| decode(&plain)) {
        Ok(items) => items,
        Err(e) => {
            crate::log::error(format!("[histórico] não consegui ler {}: {e}", path.display()));
            Vec::new()
        }
    }
}

pub fn remove() {
    let _ = std::fs::remove_file(path());
}

fn encode(items: &[SavedClip]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        let (is_image, has_png) = match &item.content {
            SavedContent::Text(_) => (false, false),
            SavedContent::Image { png, .. } => (true, png.is_some()),
        };
        let flags = u8::from(item.pinned) | u8::from(is_image) << 1 | u8::from(has_png) << 2 | u8::from(item.name.is_some()) << 3;
        out.push(flags);
        out.extend_from_slice(&item.age_secs.to_le_bytes());
        put_bytes(&mut out, item.app.as_bytes());
        if let Some(name) = &item.name {
            put_bytes(&mut out, name.as_bytes());
        }
        match &item.content {
            SavedContent::Text(text) => put_bytes(&mut out, text.as_bytes()),
            SavedContent::Image { dib, png } => {
                put_bytes(&mut out, dib);
                if let Some(png) = png {
                    put_bytes(&mut out, png);
                }
            }
        }
    }
    out
}

fn decode(data: &[u8]) -> Result<Vec<SavedClip>, String> {
    let mut r = Reader { data, pos: 0 };
    if r.take(4)? != MAGIC {
        return Err("formato desconhecido".into());
    }
    let count = r.u32()? as usize;
    let mut items = Vec::with_capacity(count.min(10_000));
    for _ in 0..count {
        let flags = r.take(1)?[0];
        let age_secs = u64::from_le_bytes(r.take(8)?.try_into().map_err(|_| "cabeçalho curto")?);
        let app = r.string()?;
        let name = if flags & 8 != 0 { Some(r.string()?) } else { None };
        let content = if flags & 2 != 0 {
            let dib = r.bytes()?.to_vec();
            let png = if flags & 4 != 0 { Some(r.bytes()?.to_vec()) } else { None };
            SavedContent::Image { dib, png }
        } else {
            SavedContent::Text(r.string()?)
        };
        items.push(SavedClip { content, app, name, pinned: flags & 1 != 0, age_secs });
    }
    Ok(items)
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or("arquivo truncado")?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "arquivo truncado")?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| "texto inválido".into())
    }
}

/// Cifra com a chave da conta do Windows; só esta conta, nesta máquina, decifra.
fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    unsafe { dpapi(plain, true) }
}

fn unprotect(secret: &[u8]) -> Result<Vec<u8>, String> {
    unsafe { dpapi(secret, false) }
}

unsafe fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    let blob_in = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
    let mut blob_out = CRYPT_INTEGER_BLOB::default();
    let result = if encrypt {
        CryptProtectData(&blob_in, None, None, None, None, 0, &mut blob_out)
    } else {
        CryptUnprotectData(&blob_in, None, None, None, None, 0, &mut blob_out)
    };
    result.map_err(|e| format!("DPAPI: {e}"))?;
    let out = std::slice::from_raw_parts(blob_out.pbData, blob_out.cbData as usize).to_vec();
    let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(blob_out.pbData as *mut c_void)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vai_e_volta() {
        let items = vec![
            SavedClip {
                content: SavedContent::Text("Pedido 48.001 · soja".into()),
                app: "excel".into(),
                name: Some("Pedido".into()),
                pinned: true,
                age_secs: 120,
            },
            SavedClip {
                content: SavedContent::Image { dib: vec![1, 2, 3], png: None },
                app: "captura".into(),
                name: None,
                pinned: false,
                age_secs: 0,
            },
        ];
        let back = decode(&encode(&items)).unwrap();
        assert_eq!(back.len(), 2);
        assert!(matches!(&back[0].content, SavedContent::Text(t) if t == "Pedido 48.001 · soja"));
        assert_eq!(back[0].name.as_deref(), Some("Pedido"));
        assert!(back[0].pinned && back[0].age_secs == 120);
        assert!(matches!(&back[1].content, SavedContent::Image { dib, png: None } if dib == &[1, 2, 3]));
    }

    #[test]
    fn recusa_lixo() {
        assert!(decode(b"xxxx").is_err());
        assert!(decode(b"LPD1\x05\x00\x00\x00").is_err());
    }

    #[test]
    fn cifra_e_decifra() {
        let plain = b"segredo".to_vec();
        let secret = protect(&plain).unwrap();
        assert_ne!(secret, plain);
        assert_eq!(unprotect(&secret).unwrap(), plain);
    }
}
