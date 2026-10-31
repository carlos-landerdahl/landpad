<p align="center"><img src="docs/icon.png" width="96" alt="Ícone do Landpad"></p>

<h1 align="center">Landpad</h1>

<p align="center">Histórico da área de transferência e captura de tela para Windows, leve e rápido.</p>

O Landpad guarda o que você copia, texto e imagens, acha qualquer coisa pela busca e cola de volta em qualquer app. No `PrtSc`, congela a tela para você recortar, anotar e copiar, no estilo do Lightshot, e tem um conta-gotas para pegar qualquer cor da tela.

- **Leve:** cerca de 8 MB de memória e o histórico abre em ~12 ms.
- **Um app só:** um executável, sem serviços, sem conta e sem nuvem.
- **Privado:** o histórico fica só na memória. Fechou o Landpad, ele some.

## Instalar

Baixe o `Landpad-Setup-x.y.z.exe` na página de [Releases](https://github.com/carlos-landerdahl/landpad/releases/latest) e abra. A instalação é por usuário: não pede senha de administrador e põe o Landpad no menu Iniciar. Ele fica no ícone da bandeja, perto do relógio.

Na primeira vez o Windows pode mostrar "O Windows protegeu o computador", porque o executável ainda não tem assinatura digital: clique em **Mais informações** e depois em **Executar assim mesmo**.

Também há um zip portátil (`Landpad-x.y.z-portatil.zip`), que roda sem instalar; nesse caso as atualizações não são automáticas.

### Atualizações

O Landpad instalado se atualiza sozinho. Uma vez por dia ele pergunta ao GitHub se há versão nova; se houver, baixa em segundo plano, confere o arquivo e instala na próxima vez que abrir, antes de mostrar qualquer coisa. Para não esperar, clique em **Atualizar agora** no aviso ou no menu da bandeja. A checagem não envia nada sobre você nem sobre o que você copia, e pode ser desligada em Configurações > Sobre.

## Usar

| Atalho | O que faz |
|---|---|
| `` Ctrl+` `` | Abre o histórico perto do cursor de texto |
| `PrtSc` | Captura de tela |
| `Win+Shift+C` | Conta-gotas: pega uma cor da tela (se o Windows já usar esse atalho, vale `Ctrl+Shift+PrtSc`) |

**No histórico:** digite para buscar (acentos não importam), use ↑ ↓ e `Enter` para colar no app em que você estava. `Ctrl+P` fixa um item no topo e `Tab` mostra só os fixados. `Ctrl+Shift+1` a `9` colam direto pelo número. `Esc` fecha, e `Ctrl+,` abre as configurações.

**Na captura:** arraste para selecionar. Na barra embaixo da seleção ficam seta (`A`), retângulo (`R`), marca-texto (`M`) e borrar (`B`), que pixela dados que não devem aparecer; o círculo troca a cor e `Ctrl+Z` desfaz. `Enter` copia, `Ctrl+S` também salva um PNG em `Imagens\Landpad`, e `Esc` cancela. Toda captura entra no histórico.

**No conta-gotas:** uma lupa mostra os pixels em volta do cursor e o código da cor. Clique copia em HEX (`#2E63FF`), `Shift`+clique copia em RGB (`rgb(46, 99, 255)`), as setas andam 1 pixel e `Esc` cancela. Na captura, a tecla `C` troca para o conta-gotas. Cores copiadas aparecem no histórico com uma bolinha da cor.

**Na bandeja:** um clique abre o histórico. O botão direito tem captura de tela, pausar a captura, configurações e sair.

Os atalhos, o tema, onde o histórico abre, iniciar com o Windows e o limite de itens se mudam nas configurações.

## Privacidade

- Nada do que você copia sai do computador. A única conexão é a checagem de versão nova no GitHub, que dá para desligar.
- Cópias de gerenciadores de senha (que avisam o Windows que a cópia é sensível) nunca entram no histórico, e você pode listar outros apps para ignorar.
- O histórico fica na memória. Imagens copiadas ficam em `%LOCALAPPDATA%\Landpad\imagens` só enquanto o Landpad está aberto.
- As preferências ficam em `%APPDATA%\Landpad\config.ini`.

## Compilar

Precisa do Rust estável e do Build Tools do Visual Studio com C++ e o Windows SDK.

```bash
cargo run --release
```

Testes, medições e como publicar uma versão estão em [docs/desenvolvimento.md](docs/desenvolvimento.md).

## Licença

[MIT](LICENSE) © Carlos Landerdahl
