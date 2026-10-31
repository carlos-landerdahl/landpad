<p align="center"><img src="docs/icon.png" width="96" alt="Ícone do Landpad"></p>

<h1 align="center">Landpad</h1>

<p align="center">Histórico da área de transferência e captura de tela para Windows, leve e rápido.</p>

O Landpad guarda o que você copia, texto e imagens, acha qualquer coisa pela busca e cola de volta em qualquer app. No `PrtSc`, congela a tela para você recortar, anotar e copiar, no estilo do Lightshot, e tem um conta-gotas para pegar qualquer cor da tela.

- **Leve:** cerca de 8 MB de memória e o histórico abre em ~12 ms.
- **Um app só:** um executável, sem serviços, sem conta e sem nuvem.
- **Privado:** o histórico fica na memória e some ao fechar; só os fixados voltam, cifrados com a sua conta do Windows. Nada sai do computador.

## Instalar

Baixe o `Landpad-Setup-x.y.z.exe` na página de [Releases](https://github.com/carlos-landerdahl/landpad/releases/latest) e abra. A instalação é por usuário: não pede senha de administrador e põe o Landpad no menu Iniciar. Ele fica no ícone da bandeja, perto do relógio.

Na primeira vez o Windows pode mostrar "O Windows protegeu o computador", porque o executável ainda não tem assinatura digital: clique em **Mais informações** e depois em **Executar assim mesmo**.

Também há um zip portátil (`Landpad-x.y.z-portatil.zip`), que roda sem instalar. Ele avisa quando sai versão nova, e o clique no aviso abre o instalador; daí em diante as atualizações são automáticas. Para que tudo fique na pasta do zip (pendrive, pasta sincronizada), crie um arquivo vazio chamado `config.ini` ao lado do `Landpad.exe`: as preferências vão para ele e o resto para uma pasta `dados` ali mesmo, sem nada em AppData.

### Atualizações

O Landpad instalado se atualiza sozinho. Uma vez por dia ele pergunta ao GitHub se há versão nova; se houver, baixa em segundo plano, confere o arquivo e instala na próxima vez que abrir, antes de mostrar qualquer coisa. Para não esperar, clique em **Atualizar agora** no aviso ou no menu da bandeja. A checagem não envia nada sobre você nem sobre o que você copia, e pode ser desligada em Configurações > Sobre.

## Usar

| Atalho | O que faz |
|---|---|
| `` Ctrl+` `` | Abre o histórico perto do cursor de texto |
| `PrtSc` | Captura de tela |
| `Win+Shift+C` | Conta-gotas: pega uma cor da tela (se o Windows já usar esse atalho, vale `Ctrl+Shift+PrtSc`) |
| `` Ctrl+Shift+` `` | Cola o item anterior sem abrir o histórico; apertar de novo alterna entre os dois últimos |

**No histórico:** digite para buscar (acentos não importam), use ↑ ↓ e `Enter`, ou clique no item, para colar no app em que você estava (`Shift+Enter` ou `Shift`+clique só copia, sem colar). `Ctrl+Enter` abre um link no navegador, um e-mail no seu app de e-mail ou um caminho no Explorer. `Shift`+↑ ↓ seleciona várias linhas e `Enter` cola todas juntas. `Ctrl+P` fixa um item no topo, `F2` dá um nome ao fixado, `Delete` apaga o item e `Tab` mostra só os fixados. `Ctrl+Shift+1` a `9` colam direto pelo número. `Esc` fecha, e `Ctrl+,` abre as configurações.

O botão direito num item abre o menu com tudo isso e mais: colar em MAIÚSCULAS ou minúsculas, numa linha só ou sem espaços extras; colar só os números de um CPF, CNPJ ou telefone; formatar ou compactar um JSON; salvar uma imagem em `Imagens\Landpad`.

Textos longos e códigos aparecem num cartão de prévia ao serem selecionados. Na busca, `link:`, `img:`, `cor:`, `code:`, `json:`, `email:`, `tel:`, `doc:` e `arquivo:` filtram por tipo, e `de:chrome` filtra pelo app de origem.

**Na captura:** arraste para selecionar, ou clique numa janela para pegá-la inteira; depois, arraste a seleção para mover ou as alças das bordas para ajustar. Na barra embaixo da seleção ficam seta (`A`), retângulo (`R`), linha (`L`), marca-texto (`M`), borrar (`B`), que pixela dados que não devem aparecer, texto (`T`), que vira uma legenda onde você clicar (`Enter` quebra a linha; `Ctrl+Enter` ou `Ctrl+C` concluem e copiam a captura), e numeração (`N`), que põe bolinhas 1, 2, 3… a cada clique; o círculo abre a paleta de cores (`K` troca direto) e `Ctrl+Z` desfaz. Parar o mouse sobre um botão mostra o nome e a tecla dele. `Enter` copia, `Ctrl+S` também salva um PNG em `Imagens\Landpad` (um aviso na bandeja confirma e abre a pasta), `P` fixa a região numa janelinha por cima de tudo (arraste para mover, roda do mouse muda a transparência, `Esc` ou duplo clique fecha), `O` reconhece o texto da região com o OCR do Windows e copia só ele, e `Esc` cancela. Toda captura entra no histórico.

**No conta-gotas:** uma lupa mostra os pixels em volta do cursor e o código da cor em HEX e RGB. Clique copia em HEX (`#2E63FF`), `Shift`+clique copia em RGB (`rgb(46, 99, 255)`), as setas andam 1 pixel e `Esc` cancela. Na captura, a tecla `C` troca para o conta-gotas. Cores copiadas aparecem no histórico com uma bolinha da cor.

**Na bandeja:** um clique abre o histórico. O botão direito tem captura de tela, conta-gotas, colar o item anterior, pausar a captura, configurações e sair.

Os atalhos, o tema, onde o histórico abre, iniciar com o Windows e o limite de itens se mudam nas configurações.

## Privacidade

- Nada do que você copia sai do computador. A única conexão é a checagem de versão nova no GitHub, que dá para desligar.
- Cópias de gerenciadores de senha (que avisam o Windows que a cópia é sensível) nunca entram no histórico, e você pode listar outros apps para ignorar.
- O histórico fica na memória. Imagens copiadas ficam em `%LOCALAPPDATA%\Landpad\imagens` só enquanto o Landpad está aberto.
- Os fixados voltam quando o Landpad abre de novo. Eles ficam em `%LOCALAPPDATA%\Landpad\historico.bin`, cifrado com a DPAPI do Windows: só a sua conta, neste computador, consegue ler, e um backup copiado para fora é ilegível. Em Configurações > Privacidade dá para desligar isso, ou para guardar o histórico inteiro ao fechar (textos; imagens só as fixadas).
- As preferências ficam em `%APPDATA%\Landpad\config.ini` (ou ao lado do executável, no modo portátil).
- O Landpad registra em `%LOCALAPPDATA%\Landpad\landpad.log` só o que ajuda a resolver problemas: versão, de onde abriu e o que aconteceu nas atualizações. Nada do que você copia vai para o log. Se algo der errado, é esse arquivo que vale mandar.

## Compilar

Precisa do Rust estável e do Build Tools do Visual Studio com C++ e o Windows SDK.

```bash
cargo run --release
```

Testes, medições e como publicar uma versão estão em [docs/desenvolvimento.md](docs/desenvolvimento.md).

## Licença

[MIT](LICENSE) © Carlos Landerdahl
