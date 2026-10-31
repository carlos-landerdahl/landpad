//! Cores em texto: o conta-gotas copia `#2E63FF` ou `rgb(46, 99, 255)`, e o histórico
//! reconhece esses formatos (também `#26F`) para mostrar a cor ao lado do código.

pub fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

pub fn rgb((r, g, b): (u8, u8, u8)) -> String {
    format!("rgb({r}, {g}, {b})")
}

/// Reconhece `#RRGGBB`, `#RGB` e `rgb(r, g, b)`; qualquer outra coisa não é cor.
pub fn parse(text: &str) -> Option<(u8, u8, u8)> {
    let t = text.trim();
    if let Some(digits) = t.strip_prefix('#') {
        let value = |s: &str| u8::from_str_radix(s, 16).ok();
        return match digits.len() {
            6 if digits.is_ascii() => Some((value(&digits[0..2])?, value(&digits[2..4])?, value(&digits[4..6])?)),
            3 if digits.is_ascii() => {
                let short = |i: usize| value(&digits[i..i + 1]).map(|v| v * 17);
                Some((short(0)?, short(1)?, short(2)?))
            }
            _ => None,
        };
    }
    let inner = t.strip_prefix("rgb(").or_else(|| t.strip_prefix("RGB("))?.strip_suffix(')')?;
    let mut parts = inner.split(',').map(|p| p.trim().parse::<u8>().ok());
    let color = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formata() {
        assert_eq!(hex((46, 99, 255)), "#2E63FF");
        assert_eq!(rgb((46, 99, 255)), "rgb(46, 99, 255)");
    }

    #[test]
    fn reconhece() {
        assert_eq!(parse("#2E63FF"), Some((46, 99, 255)));
        assert_eq!(parse(" #2e63ff "), Some((46, 99, 255)));
        assert_eq!(parse("#26F"), Some((0x22, 0x66, 0xFF)));
        assert_eq!(parse("rgb(46, 99, 255)"), Some((46, 99, 255)));
        assert_eq!(parse("rgb(46,99,255)"), Some((46, 99, 255)));
    }

    #[test]
    fn recusa() {
        assert_eq!(parse("#2E63F"), None);
        assert_eq!(parse("#GGGGGG"), None);
        assert_eq!(parse("rgb(300, 0, 0)"), None);
        assert_eq!(parse("rgb(1, 2, 3, 4)"), None);
        assert_eq!(parse("Pedido #123"), None);
        assert_eq!(parse("#ééé"), None);
    }
}
