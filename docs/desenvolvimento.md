# Desenvolvimento do Landpad

## Stack

Rust, com a crate `windows` para a integração com o Win32 e o Slint para as janelas, usando só o renderizador por software. Não há serviços nem processos auxiliares: tudo roda num executável.

| Módulo | O que faz |
|---|---|
| `src/main.rs` | Estado do histórico, popup, busca, fixados e a ligação entre as partes |
| `src/win.rs` | Thread do Windows: clipboard, atalhos globais, bandeja, colar, gravação de atalho |
| `src/shot.rs` | Captura de tela: thread própria, desenho com GDI e GDI+ |
| `src/settings.rs` | Janela de configurações, criada ao abrir e liberada ao fechar |
| `src/config.rs` | Preferências em `%APPDATA%\Landpad\config.ini` e iniciar com o Windows |
| `src/images.rs` | Imagens do histórico em disco enquanto o app está aberto |
| `src/update.rs` | Checagem de versão nova, download conferido por SHA-256 e instalação |
| `src/logo.rs` | O ícone desenhado em código, usado no app, no `.exe` e no instalador |
| `src/assets.rs` | Exporta o ícone em PNG para o README e para o pacote da Microsoft Store |
| `ui/*.slint` | Popup e configurações |

## Modos de teste

Todos rodam com o Landpad aberto ao lado: usam uma pasta de imagens própria (`imagens-teste`) e não disputam atalhos nem a bandeja.

| Comando | O que faz |
|---|---|
| `cargo test --release` | Testes de unidade (versões, leitura da release, SHA-256) |
| `cargo run --release -- --bench` | Enche o histórico, abre e fecha o popup 10 vezes sem roubar o foco, mede e salva `target/popup.bmp` |
| `cargo run --release -- --settings-test` | Abre as configurações fora da tela e fotografa cada página em `target/settings-N.bmp` |
| `cargo run --release -- --shot-test` | Desenha a captura com anotações sobre uma área de trabalho de mentira em `target/shot-*.png` |
| `cargo run --release -- --memory-test` | Popup, configurações e capturas em ciclos, medindo a memória a cada um |
| `cargo run --release -- --check-update` | Consulta a última versão publicada no GitHub, sem baixar nada |
| `cargo run --release -- --export-assets <pasta>` | Grava o ícone em PNG e `.ico` nos tamanhos do README, do instalador e da Store |

## Publicar uma versão

1. Suba a versão no `Cargo.toml` e rode `cargo build` para atualizar o `Cargo.lock`.
2. Faça o commit e crie a tag: `git tag v0.2.0` e `git push origin main v0.2.0`.
3. O workflow `release.yml` confere se a tag bate com o `Cargo.toml`, roda os testes, compila, gera o instalador (Inno Setup, `packaging/landpad.iss`) e o zip portátil, calcula o SHA-256 de cada um e publica a release.

Os Landpads instalados acham a versão nova em até um dia, baixam `Landpad-Setup-x.y.z.exe`, conferem com `Landpad-Setup-x.y.z.exe.sha256` e instalam na próxima abertura com `/VERYSILENT`. Por isso os nomes dos arquivos da release não podem mudar.

## Microsoft Store

O pacote MSIX é montado por `packaging/msix/build.ps1`; veja o [passo a passo](../packaging/msix/LEIA-ME.md).

## Medições

### Spike da Fase 0 (30/10/2026)

Build release, 300 itens no histórico, medido com `--bench`:

| | Renderizador por software | OpenGL (femtovg) |
|---|---|---|
| Memória privada parado | 5,9 MB | 49 MB |
| Memória privada depois de abrir | 7–10 MB | 132 MB |
| Abertura (mediana, sem contar a 1ª) | 11,5 ms¹ | 14,7 ms |
| 1ª abertura | 35–47 ms¹ | 45 ms |

¹ No software, medido até `show()`: esse renderizador não avisa quando o quadro é pintado.

Decisão: Rust + Slint só com o renderizador por software.

### Memória com tudo ligado (31/10/2026)

Medido com `--memory-test`:

| Situação | Memória privada |
|---|---|
| Parado | 6,0 MB |
| Depois do popup | 7,7 MB |
| Configurações abertas | 10,3 MB, igual nas 5 vezes |
| Configurações fechadas | 8,5 MB, igual nas 5 vezes |
| Depois de 5 capturas | 9,1 MB, igual nas 5 vezes |

Não há vazamento: cada coisa custa uma vez (fontes, glifos e estilos que o Slint guarda) e o número não cresce com o uso. Durante uma captura o app usa mais (três cópias da tela, cerca de 50 MB com dois monitores Full HD) e devolve tudo ao fechar.

O executável tem cerca de 8 MB (com `opt-level = "z"`), acima da meta inicial de 5 MB. O próximo passo para reduzir é medir o que pesa com `cargo bloat`.
