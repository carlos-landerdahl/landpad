# Landpad

Gerenciador de área de transferência para Windows, na linha do Ditto: guarda o que você copia, busca na hora e cola em qualquer app.

Este é o **spike da Fase 0**: uma versão mínima para medir se a stack (Rust + Slint + Win32) cabe nas metas de leveza antes de construir o resto.

## O que o spike faz

- Escuta o clipboard sem polling (`AddClipboardFormatListener`) e guarda textos copiados, ignorando cópias marcadas como sensíveis por gerenciadores de senha.
- Guarda também imagens copiadas (prints, "Copiar imagem" do navegador). O original vai para `%LOCALAPPDATA%\Landpad\imagens` e só uma miniatura fica na memória; o item selecionado mostra uma prévia maior. Enter cola a imagem de volta, com o PNG original quando o app de origem ofereceu. Quando a cópia traz texto e imagem juntos, como células do Excel, vale o texto.
- Como o histórico ainda não sobrevive ao fechar o app, a pasta de imagens é limpa ao abrir.
- `Ctrl` + `` ` `` (ou `Ctrl+Alt+V`, se o primeiro estiver ocupado) abre o popup perto do cursor.
- Busca sem acento, ↑ ↓ para navegar, Enter cola no app que estava em foco, Esc limpa a busca ou fecha.
- Clicar fora fecha o popup. `Ctrl+Q` no popup encerra o app.
- O rodapé do popup e o console mostram a memória do processo e quanto tempo a janela levou para abrir.

## Rodar

```bash
cargo run --release
```

## Medir

Cada abertura imprime no console a memória do processo e quanto a janela levou para abrir.

Para medir sem apertar o atalho, o modo `--bench` enche o histórico com 300 itens, abre e fecha o popup 10 vezes sem roubar o foco, salva um print em `target/popup.bmp` e encerra:

```bash
cargo run --release -- --bench
```

## Resultado do spike (30/10/2026)

Build release, 300 itens no histórico, medido com `--bench`:

| | Renderizador por software | OpenGL (femtovg) |
|---|---|---|
| Memória privada parado | 5,9 MB | 49 MB |
| Memória privada depois de abrir | 7–10 MB | 132 MB |
| Abertura (mediana, sem contar a 1ª) | 11,5 ms¹ | 14,7 ms |
| 1ª abertura | 35–47 ms¹ | 45 ms |

¹ No software, medido até `show()`: esse renderizador não avisa quando o frame é pintado.

| Meta | Alvo | Resultado |
|---|---|---|
| Memória privada parado | < 15 MB | Passou: 6–10 MB |
| Abrir o popup | < 50 ms | Passou: ~12 ms |
| Executável | < 5 MB | Não passou: 7,3 MB com `opt-level = "z"` |

Decisão: seguir com Rust + Slint usando só o renderizador por software, que já é o único compilado. O tamanho do executável fica para investigar com `cargo bloat` na Fase 1.
