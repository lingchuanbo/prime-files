<div align="center">
  <img src="assets/icon/prime-files.png" alt="" width="112" height="112">

  # Prime Files

  Explorador de arquivos para Windows, rápido, escrito em Rust.
</div>

Barra de título com abas, miniaturas e ícones do próprio Windows, menu do botão direito do Explorador, visualizador de quase tudo e um modo compacto no estilo do Zen Browser.

## Baixar

Pegue o instalador na página de [Releases](https://github.com/CakeDev4k/prime-files/releases/latest) (`PrimeFiles-Setup-<versão>.exe`) e execute. A instalação é só para o seu usuário e não pede administrador. O instalador baixa o que faltar:

- o **Visual C++ Runtime** da Microsoft, se o computador não tiver;
- o **7-Zip**, se você marcar a opção, para ver `.rar`, `.7z` e `.iso` no visualizador;
- no Windows edição "N", o **Media Feature Pack**, sem o qual o programa não abre.

Depois de instalado, o Prime Files se atualiza sozinho: procura uma versão nova ao abrir, baixa em segundo plano e instala quando você fecha o programa (ou na hora, em Configurações › Sobre). Dá para desligar em Configurações › Sobre.

O instalador não é assinado digitalmente: o Windows pode mostrar "O Windows protegeu o computador". Clique em **Mais informações → Executar assim mesmo**. Cada release traz um `SHA256SUMS.txt` para conferir o arquivo.

Requer Windows 10 (1903) ou mais novo, 64 bits.

## O que tem

- **Abas** na barra de título, divisão em dois painéis, abas restauradas ao reabrir
- **Vistas** Detalhes, Lista, Árvore e Ícones, com zoom contínuo (Ctrl + roda) e a ordem e o zoom lembrados por pasta
- **Visualizador** de imagens, GIF, vídeo e áudio (com forma de onda), PDF, modelos 3D (OBJ, STL, glTF, FBX), texto, Markdown, pastas e arquivos compactados
- **Integração com o Windows**: menu do botão direito do Explorador, arrastar e soltar com outros programas, miniaturas do Shell, "mostrar na pasta" de outros programas abrindo aqui
- **Busca em todo o computador** no estilo do Everything (lupa da barra de baixo ou Ctrl+E), nas subpastas (Ctrl+F) e filtro rápido (é só digitar na lista)
- **Ouvir sons passando o mouse** sobre arquivos de áudio, com o tempo de espera ajustável
- **Armazenamento** de cada unidade em Este Computador e na barra lateral
- **Terminal** embutido com abas (Ctrl+')
- **Nove idiomas**: português, inglês, espanhol, francês, alemão, italiano, russo, japonês e chinês simplificado
- **Modo compacto** no estilo do Zen Browser: abas, barra de rota, barra lateral, visualizador e barra de baixo escondidos até o mouse encostar na borda, por cima da lista ou empurrando-a, numa moldura de cantos arredondados
- **Explorador padrão** do Windows, opcional (Configurações › Integração com o Windows): pastas, drives e Win+E abrem no Prime Files
- **Listary**, opcional (Configurações › Integração com o Windows): pastas abertas pelo Listary abrem no Prime Files; o desinstalador devolve ao Explorador
- **Atualização automática** pelas releases deste repositório

## Compilar

Precisa do [Rust](https://rustup.rs) (stable) e do Windows.

```bash
cargo build --release -p nohrs
```

O programa fica em `target/release/nohrs.exe` (o código usa internamente o nome `nohrs`: crates `nohrs-*` e a pasta de configuração). Para compilar também o serviço de indexação, `cargo build --release -p nohrs -p nohrs-indexer`.

No Windows, `compilar.bat` faz os dois de uma vez: `compilar.bat` para o uso do dia a dia (perfil `release`, recompila rápido), `compilar.bat dist` para o que o instalador empacota (`target/dist`). O instalador sai de `installer/prime-files.iss` com o [Inno Setup 6](https://jrsoftware.org/isinfo.php); o workflow `.github/workflows/release.yml` faz os dois e publica a release a cada tag `v*`. O ícone é gerado por `assets/icon/make_icon.py`.

Inclui uma cópia do [gpui](https://github.com/zed-industries/zed) 0.2.2 com correções para o Windows, descritas em [`third_party/gpui/NOHRS.md`](third_party/gpui/NOHRS.md).

## Licença

MIT, veja [`LICENSE`](LICENSE).
