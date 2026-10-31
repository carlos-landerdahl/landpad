# Landpad

Gerenciador de área de transferência para Windows, na linha do Ditto: guarda o que você copia, busca na hora e cola em qualquer app.

Este é o **spike da Fase 0**: uma versão mínima para medir se a stack (Rust + Slint + Win32) cabe nas metas de leveza antes de construir o resto.

## O que o spike faz

- Escuta o clipboard sem polling (`AddClipboardFormatListener`) e guarda textos copiados, ignorando cópias marcadas como sensíveis por gerenciadores de senha.
- Guarda também imagens copiadas (prints, "Copiar imagem" do navegador). O original vai para `%LOCALAPPDATA%\Landpad\imagens` e só uma miniatura fica na memória; o item selecionado mostra uma prévia maior. Enter cola a imagem de volta, com o PNG original quando o app de origem ofereceu. Quando a cópia traz texto e imagem juntos, como células do Excel, vale o texto.
- `Ctrl+P` (ou o alfinete na linha) fixa um item no topo; fixados não saem do histórico pelo limite de 500. `Tab` (ou o alfinete ao lado da busca) mostra só os fixados.
- Com o popup aberto, `Ctrl+Shift+1` a `Ctrl+Shift+9` colam direto o item com aquele número. Esses atalhos só ficam registrados enquanto o popup está aberto.
- O histórico vive só enquanto o app está aberto, por decisão de produto: ao fechar, ele some, fixados e imagens inclusive. Se o app for encerrado à força, as imagens que sobraram são apagadas na próxima abertura.
- `Ctrl` + `` ` `` (ou `Ctrl+Alt+V`, se o primeiro estiver ocupado) abre o popup perto do cursor.
- Busca sem acento, ↑ ↓ para navegar, Enter cola no app que estava em foco, Esc limpa a busca ou fecha.
- Clicar fora fecha o popup. `Ctrl+Q` no popup encerra o app.
- O rodapé do popup e o console mostram a memória do processo e quanto tempo a janela levou para abrir.

## Captura de tela

`PrtSc` congela todos os monitores e abre a captura, no estilo do Lightshot. Se outro app já usa o `PrtSc`, o atalho vira `Ctrl+PrtSc`; o terminal mostra qual ficou.

- Arraste para selecionar a região. Arrastar dentro dela move a seleção; clicar fora começa outra.
- Ferramentas na barra ou pelo teclado: seta (`A`), retângulo (`R`), marca-texto (`M`) e borrar (`B`), que pixela a área para esconder dados. O círculo troca a cor e `Ctrl+Z` desfaz.
- `Enter`, `Ctrl+C` ou duplo clique copiam a captura. `Ctrl+S` também salva um PNG em `Imagens\Landpad`. `Esc` ou o botão direito cancelam.
- Toda captura copiada entra no histórico do popup.

Nada da captura fica carregado entre um uso e outro: cada captura roda numa thread própria, que libera a memória ao fechar. Para conferir o desenho sem abrir a captura na tela, `cargo run --release -- --shot-test` desenha sobre uma área de trabalho de mentira e salva `target/shot-overlay.png` e `target/shot-result.png`.

## Bandeja e configurações

O Landpad fica na bandeja do Windows. Um clique no ícone abre o histórico; o botão direito tem captura de tela, pausar a captura, configurações e sair. As configurações também abrem pela engrenagem do popup ou por `Ctrl+,`.

- **Geral:** iniciar com o Windows (entrada em `HKCU\...\Run`), tema (seguir o Windows, claro ou escuro) e onde o histórico abre (cursor de texto, mouse ou centro da tela).
- **Atalhos:** "Alterar" grava a próxima combinação apertada; se outro app já usa, a tela avisa. Dá para ligar e desligar o `Ctrl+Shift+1..9`.
- **Privacidade:** pausar a captura e uma lista de apps cujas cópias nunca entram no histórico.
- **Histórico:** limite de itens, guardar imagens ou não e limpar (os fixados ficam).

As preferências ficam em `%APPDATA%\Landpad\config.ini` e são salvas a cada mudança. O histórico continua só na memória.

O executável não abre janela de console. Aberto de um terminal, ele usa o console do terminal para os logs. Só pode haver um Landpad aberto por vez; os modos de teste rodam ao lado dele.

Para conferir a tela sem clicar em nada, `cargo run --release -- --settings-test` abre as configurações fora da tela, fotografa cada página em `target/settings-N.bmp` e fecha.

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
