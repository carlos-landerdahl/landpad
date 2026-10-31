# Publicar o Landpad na Microsoft Store

Na loja o Landpad sai assinado pela Microsoft: sem o aviso do SmartScreen, com instalação de um clique e atualizações automáticas feitas pela própria loja. Nessa versão o Landpad não procura atualizações no GitHub, e "Iniciar com o Windows" fica em Configurações > Aplicativos > Inicialização do Windows.

## Uma vez só

1. Crie a conta de desenvolvedor individual em <https://storedeveloper.microsoft.com>. Para pessoa física a inscrição é gratuita desde 2024; confira as condições na hora.
2. No Partner Center, em **Aplicativos e jogos**, crie um app e reserve o nome **Landpad**.
3. Em **Gerenciamento de produto > Identidade do produto**, copie os três valores:
   - `Package/Identity/Name`
   - `Package/Identity/Publisher` (começa com `CN=`)
   - `Package/Properties/PublisherDisplayName`

## A cada versão

1. Monte o pacote com os valores acima:

   ```powershell
   .\packaging\msix\build.ps1 -IdentityName "<Name>" -Publisher "<Publisher>" -PublisherDisplayName "<PublisherDisplayName>"
   ```

   O resultado é `target\msix\Landpad-x.y.z.msix`, sem assinatura (a loja assina).
2. No Partner Center, crie um envio e suba o `.msix` em **Pacotes**.
3. Na primeira vez, preencha também:
   - **Listagem da loja:** descrição, capturas de tela do popup, da captura de tela e das configurações.
   - **Política de privacidade:** pode apontar para a seção Privacidade do README.
   - **Classificação etária** e categoria **Produtividade**.
   - **Recursos restritos:** o pacote declara `runFullTrust`. Justificativa sugerida: "O Landpad é um app de desktop (Win32) que escuta a área de transferência, registra atalhos globais, cola no app em uso e captura a tela."
4. Envie para certificação. Costuma levar de algumas horas a poucos dias.

## Testar o pacote antes de enviar

Um `.msix` sem assinatura não instala no seu PC. Para testar localmente seria preciso assiná-lo com um certificado de teste instalado como confiável. Até lá, o que dá para conferir aqui é que o `makeappx` aceita o manifesto e os ícones, que é o que o `build.ps1` já faz.
