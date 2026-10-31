//! O ícone do Landpad desenhado em código: um L geométrico em duas camadas (teal e branco)
//! sobre um quadrado azul em degradê. Serve a bandeja, as janelas e o popup em qualquer
//! tamanho, sem arquivo de imagem no executável.

/// Traços do L numa grade de 64 × 64, como no canvas de design.
const TEAL_L: [(f32, f32); 3] = [(27.0, 16.0), (27.0, 40.0), (44.0, 40.0)];
const WHITE_L: [(f32, f32); 3] = [(20.0, 23.0), (20.0, 47.0), (37.0, 47.0)];
const STROKE: f32 = 9.3;
const CORNER: f32 = 16.0;

/// Pixels RGBA (alfa direto) de um ícone `size` × `size`. `muted` deixa o ícone cinza,
/// para a bandeja mostrar que a captura está pausada.
pub fn rgba(size: u32, muted: bool) -> Vec<u8> {
    // 4 × 4 amostras por pixel deixam as bordas suaves mesmo em 16 px.
    const SAMPLES: u32 = 4;
    let unit = size as f32 / 64.0;
    let (top, bottom) = if muted { ([0x9A, 0x9A, 0x9A], [0x5E, 0x5E, 0x5E]) } else { ([0x4A, 0x7B, 0xFF], [0x1C, 0x3F, 0xCF]) };
    let teal = if muted { [0xD0, 0xD0, 0xD0] } else { [0x2E, 0xCD, 0xB4] };
    let white = [0xFF, 0xFF, 0xFF];

    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0f32; 3];
            let mut hits = 0u32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    // Ponto da amostra na grade de 64.
                    let gx = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / unit;
                    let gy = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / unit;
                    if !in_rounded_square(gx, gy) {
                        continue;
                    }
                    let color = if near(gx, gy, &WHITE_L) {
                        white
                    } else if near(gx, gy, &TEAL_L) {
                        teal
                    } else {
                        // Degradê a 135°: do canto de cima à esquerda ao de baixo à direita.
                        let t = ((gx + gy) / 128.0).clamp(0.0, 1.0);
                        [0, 1, 2].map(|i| top[i] as f32 + (bottom[i] as f32 - top[i] as f32) * t).map(|v| v as u8)
                    };
                    for i in 0..3 {
                        sum[i] += color[i] as f32;
                    }
                    hits += 1;
                }
            }
            if hits > 0 {
                let i = ((y * size + x) * 4) as usize;
                for c in 0..3 {
                    out[i + c] = (sum[c] / hits as f32).round() as u8;
                }
                out[i + 3] = (hits * 255 / (SAMPLES * SAMPLES)) as u8;
            }
        }
    }
    out
}

fn in_rounded_square(x: f32, y: f32) -> bool {
    if !(0.0..64.0).contains(&x) || !(0.0..64.0).contains(&y) {
        return false;
    }
    let cx = x.clamp(CORNER, 64.0 - CORNER);
    let cy = y.clamp(CORNER, 64.0 - CORNER);
    (x - cx).powi(2) + (y - cy).powi(2) <= CORNER * CORNER
}

/// Se o ponto cai dentro do traço de uma linha quebrada.
fn near(x: f32, y: f32, line: &[(f32, f32)]) -> bool {
    line.windows(2).any(|seg| {
        let ((ax, ay), (bx, by)) = (seg[0], seg[1]);
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (px, py) = (ax + dx * t, ay + dy * t);
        (x - px).powi(2) + (y - py).powi(2) <= (STROKE / 2.0).powi(2)
    })
}

/// Arquivo .ico com uma imagem DIB de 32 bits (com alfa) por tamanho.
pub fn ico(sizes: &[u32]) -> Vec<u8> {
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
    let rgba = rgba(size, false);
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
