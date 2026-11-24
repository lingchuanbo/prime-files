# Desempenho do Nohrs (fork Windows)

Registro das passadas de desempenho no formato de
[`OTIMIZACAO.md`](../../OTIMIZACAO.md) §11. Os números são da máquina do dono
(12 núcleos lógicos, 15 GB, Premiere aberto e ~1 GB livre), salvo onde dito.

---

### 1. Listagem, ordem, miniaturas e desenho da lista (04/11/2026)

**Sintoma.** Nenhum relatado; passada pedida pelo dono para aplicar o guia de
otimização ao Nohrs: "renderização de arquivos, memória, CPU, escalável". Por
isso cada achado abaixo começa pela medida ou pela contagem, não pela queixa.

**Base.** `crates/nohrs-services/examples/listing_ab.rs` (listagem inteira,
mediana de 7 rodadas alternadas, release) e o teste `sort_timing` de
`explorer/entries.rs` (35.000 nomes, `--release --ignored`). Antes de mudar,
o A/B rodou com os dois lados iguais e deu números iguais: o instrumento não
cria diferença sozinho.

#### Uma leitura do disco por arquivo que já tinha sido lido

`list_dir_impl` chamava `fs::symlink_metadata(path)` para cada entrada. No
Windows isso abre o arquivo de novo pelo caminho; o `DirEntry::metadata` do
`read_dir` é o que o `FindNextFileW` já devolveu junto com o nome, e, como o
`symlink_metadata`, descreve o link e não o alvo. Trocado; o exemplo confere
entrada por entrada (nome, caminho, tipo, tamanho, data) que as duas listagens
são idênticas antes de medir.

| Pasta | Itens | Antes (tudo) | Depois (tudo) |
|---|---:|---:|---:|
| `C:\Windows\WinSxS\Manifests` | 35.628 | 2.628 ms | 95 ms |
| pasta temporária | 20.000 | 1.161 ms | 23 ms |
| `C:\Windows\System32` | 5.218 | 174 ms | 5,5 ms |
| `I:\Meu Drive` (Google Drive) | 69 | 18 ms | 0,9 ms |

#### Pastas acima de 1.000 itens perdiam arquivos em silêncio

`DIR_LISTING_LIMIT = 1000` e ninguém pedia a página seguinte: System32 mostrava
os primeiros 1.000 nomes de 5.218, sem aviso. Com a listagem barata, o explorador
lê a pasta inteira (`usize::MAX`). Ler 1.000 custava 38 ms em System32; ler
todos custa 5,5.

#### A leitura saiu da thread da interface (§2.12, §3.10, §5.4)

`reload()` listava na thread do gpui. Agora a leitura roda numa thread própria
e a interface espera no máximo `LISTING_WAIT` (30 ms): pasta local chega no
mesmo quadro do clique, como antes; pasta de rede, Drive ou disco acordando
segue em segundo plano. A lista anterior fica na tela por `LISTING_HOLD`
(220 ms) e só então dá lugar a "Reading…" na barra de status. Toda leitura leva
um número de geração; resposta de geração velha é descartada, e as tarefas da
leitura anterior são canceladas. Thread própria, não o executor: uma leitura
presa num compartilhamento morto não pode levar uma thread do pool junto.
`reveal` (abrir já selecionando um arquivo) espera a leitura chegar.

#### Ordem do Explorador, com chave calculada uma vez

A comparação fazia `to_lowercase()` dos dois nomes a cada comparação (~34 por
entrada em 35 mil), e a ordem não era a do Explorador (`f10` antes de `f2`,
§7.5). Primeira tentativa: `StrCmpLogicalW` no comparador — ordem certa, mas
**140 ms** contra 46 do antigo (≈0,8 µs por chamada). Ficou a chave de
ordenação do próprio Windows (`LCMapStringW` com `LCMAP_SORTKEY |
NORM_IGNORECASE | SORT_DIGITSASNUMBERS`), uma por nome, comparada byte a byte.
A pasta também era ordenada duas vezes por navegação (`entries` e depois a
lista visível); `entries` não é mais ordenada, ninguém lê a ordem dela.

| 35.000 nomes, release | Antes | `StrCmpLogicalW` | Chave |
|---|---:|---:|---:|
| ordenar por nome | 45–46 ms | 140 ms | 39–40 ms |

#### Miniaturas e ícones

- **Busca por caminho primeiro.** O cache era um `HashMap<(caminho, tipo,
  tamanho)>`, e cada tile ainda carregando varria o mapa inteiro atrás de um
  tamanho menor do mesmo arquivo, a cada quadro. Agora é caminho → poucos slots.
- **O que saiu da tela é cancelado (§5.3).** Cada quadro do painel marca o que
  pediu; o que ficou dois quadros sem ser pedido e ainda espera um worker é
  descartado (fecha o canal, o worker pula). Antes, rolar uma pasta de vídeos
  no Drive deixava os workers lentos gerando, 2–4 s cada, tiles que já tinham
  passado.
- **Teto de memória (§6.8, lei 8).** Nada era liberado enquanto se estava na
  mesma pasta: 10.000 fotos roladas de ponta a ponta no tamanho 256 seriam
  2,6 GB de RGBA, mais a cópia no atlas da GPU. Teto de 192 MB por painel; acima
  dele saem as fora da tela há mais tempo, até 3/4 do teto (histerese), com
  `drop_image` para liberar a GPU também.
- **Ícone por classe (§6.11).** Ícone de `.txt`, `.wav`, `.pdf` é o mesmo para
  todos os arquivos da extensão: o worker pede ao Shell uma vez por extensão e
  tamanho e todas as linhas dividem a mesma imagem (uma textura). Por arquivo
  continuam pastas, `.exe`, `.lnk`, `.url`, `.ico` e afins, e toda extensão
  cujo `DefaultIcon` é `%1` ou que tem `IconHandler` registrado.

#### Desenho da lista e da grade

A cada quadro (e cada miniatura que chega é um quadro) a lista refazia dois
vetores do tamanho da pasta e a grade quebrava os nomes de **todos** os itens
em linhas, alocando uma `String` por linha. Acima de 50.000 nomes o cache de
layout era limpo a cada quadro e todos os nomes eram moldados de novo. Agora:
linhas e alturas guardadas com uma chave (geração das linhas, quantidade,
colunas, larguras, rem), refeitas só quando ela muda; contar linhas não aloca;
o teto do cache de layouts acompanha a pasta (2× o número de itens) e ele é
limpo ao trocar de pasta.

#### Filtro por nome

O padrão do filtro era passado para minúsculas e dividido em caracteres uma vez
por nome, em cada tecla. Agora `NameMatcher` prepara o padrão uma vez por
filtragem (e uma vez por busca em subpastas).

**Cobertura.**
- `listing_ab` recusa medir se as listagens antiga e nova diferirem.
- `names_sort_as_strcmplogicalw_does`: cada par vizinho da ordem por chave é
  conferido com o `StrCmpLogicalW`, em nomes difíceis e nos ~41 mil nomes reais
  de System32 e WinSxS\Manifests. `names_sort_as_explorer_sorts_them`,
  `apply_order_matches_a_plain_sort_for_many_shapes`.
- `what_left_the_screen_is_cancelled_and_the_oldest_thumbnails_go_past_the_cap`,
  `icons_are_shared_only_where_the_registry_says_so`.
- `a_slow_folder_is_read_in_the_background_and_an_overtaken_read_is_dropped`.
- Mutação: sem o cancelamento de tiles fora da tela, e sem
  `SORT_DIGITSASNUMBERS`, três testes reprovam.

**Descartado.** `StrCmpLogicalW` no comparador (140 ms, acima). Trocar
`FileEntryDto.kind` (`String`) por enum e guardar a lista filtrada como índices
em vez de cópias: economizaria memória por item (§6.12), mas toca quase todo o
explorador e os testes; fica para uma passada própria, se a memória por item
aparecer numa medida.

**Não conferido.**
- Nada foi visto na tela: o Nohrs estava aberto pelo dono durante a passada e o
  `.exe` de `target\release` não pôde ser substituído. O release compilou em
  `target\ipc\release`.
- O caminho em segundo plano não foi visto com uma pasta de rede real lenta;
  o teste o exercita com `listing_wait = 0`.
- O ganho do cache de layout da grade e da lista não foi cronometrado (é
  estrutural: O(pasta) por quadro → O(1) quando nada mudou).
- O compartilhamento de ícone por classe não foi conferido visualmente contra
  o Explorador para tipos com ícone por arquivo não listados.
- A pasta aberta não é vigiada (mudança feita por outro programa só aparece
  com F5) — fora do escopo, registrado aqui.

**Lição (passada 1).** No Windows, `DirEntry::metadata` é de graça e
`fs::symlink_metadata(entry.path())` reabre o arquivo: 28× numa pasta de 35 mil.
E a comparação "lógica" do Windows custa ~0,8 µs por chamada; ordenar com ela
precisa de chave pré-calculada (`LCMapStringW` + `SORT_DIGITSASNUMBERS`).

---

### 2. Mover no mesmo disco era copiar e apagar (04/11/2026)

**Sintoma.** Nenhum relatado; saiu da leitura do código pedida pelo dono
("listar, deletar, copiar, renomear, preview"). Recortar e colar uma pasta
dentro do mesmo disco escrevia todos os bytes de novo.

**Base.** `crates/nohrs-services/examples/move_ab.rs` (release, mediana de 5
rodadas alternadas, Premiere aberto, ~700 MB livres). O lado "antes" é a
literal do que o Windows fazia: `copy_path_no_replace` + `delete_permanent`.
Rodado com os dois lados iguais antes da mudança, deu 8,1 s contra 6,1 s nos
2.000 arquivos e 1,55 contra 1,2 s no de 512 MB: **o ruído desta máquina é de
até 35%**, então só diferença de ordem de grandeza conta.

#### `rename_no_replace` devolvia `None` no Windows

`move_path_no_replace` (o que o colar de um recorte, o arrastar, os passos de
"substituir" e a restauração da lixeira usam) tenta primeiro um rename que
recusa destino ocupado. Só havia um para Linux e macOS; no Windows a função
devolvia `None` e **todo** mover caía no fallback de copiar e apagar, mesmo
com origem e destino no mesmo disco. Agora é `MoveFileExW` sem
`MOVEFILE_REPLACE_EXISTING`: o próprio sistema recusa nome ocupado
(`ERROR_ALREADY_EXISTS`), para arquivo e pasta, e outro disco responde
`ERROR_NOT_SAME_DEVICE`, que manda para a cópia como antes. Sem
`MOVEFILE_COPY_ALLOWED`, porque a cópia do Windows entre discos não reivindica
cada entrada, e é essa garantia que o fallback dá.

Cuidado que falha em silêncio: o `windows::core::Error` carrega um HRESULT
(`0x8007xxxx`, negativo como `i32`), e o `From` do próprio crate põe o HRESULT
inteiro como "erro do SO". Sem desembrulhar o código do Win32, o
`is_cross_device` nunca reconheceria o 17 e mover para outro disco viraria
falha em vez de cópia. A primeira versão desta passada errou isso
(`u32::try_from` de um `i32` negativo); o teste de códigos pegou.

| Mover no mesmo disco | Antes | Depois |
|---|---:|---:|
| C:, 2.000 arquivos de 64 KB em 20 pastas | 3.222 ms | 3,9 ms |
| C:, 1 arquivo de 512 MB | 2.307 ms | 2,5 ms |
| D:, 2.000 arquivos de 64 KB em 20 pastas | 2.160 ms | 1,8 ms |
| D:, 1 arquivo de 512 MB | 1.385 ms | 0,5 ms |
| C:, 1 arquivo de 1 KB | 3,3 ms | 2,3 ms |

O tempo antigo crescia com o tamanho; o novo, não. Além do tempo: o disco não
precisa mais de espaço para uma segunda cópia, e o arquivo movido mantém data
de modificação e criação (a cópia por `std::io::copy` gravava "agora").

**Efeito de comportamento.** O fallback recusa links no Windows
(`symlink_no_replace`), então uma pasta com link ou junção dentro
(`node_modules`, pasta de mods) não podia ser movida nem no mesmo disco. No
mesmo disco agora pode; entre discos continua recusada.

**Cobertura.**
- `a_move_within_one_drive_is_a_rename_and_not_a_copy` (data de modificação
  antiga sobrevive; `MoveKind::Rename`),
  `a_folder_holding_a_junction_moves_within_one_drive`,
  `the_no_replace_rename_reports_windows_codes_as_plain_io_errors` (183 e
  `NotFound` crus).
- Mutação: voltar a `None` reprova os três; passar
  `MOVEFILE_REPLACE_EXISTING` reprova `move_path_no_replace_refuses_an_occupied_destination`
  e o de códigos. O de pasta ocupada passa com esse mutante, e com razão: o
  Windows não substitui pasta nem com o flag.
- `move_ab` recusa medir se algum lado devolver uma árvore diferente (caminho
  relativo, tamanho, primeiros e últimos 4 KB de cada arquivo).
- 68 testes do `nohrs-services` e 163 do `nohrs-pages` verdes.

**Não conferido.**
- Nada visto na tela: o Nohrs do dono estava aberto em `target\release`; a
  build foi em `target\ipc`.
- Mover para outro disco (o caminho que continua copiando) não foi
  cronometrado, nem pasta de rede ou o Google Drive (`I:`, FAT32 virtual).
- Pasta-mãe inexistente agora é erro (como no Linux), e o fallback a criava.
  Colar e arrastar sempre têm destino existente; a restauração pelo ledger só
  roda onde não há índice da lixeira do sistema, que não é o Windows.

**Encontrado e não corrigido (fora do escopo).** No Windows,
`entry_identity`/`handle_identity` devolvem `None`, então
`discard_partial_destination` nunca apaga nada: um colar entre discos que
falha no meio deixa a cópia pela metade no destino.

**Lição.** Fallback que funciona esconde a ausência do caminho principal
(OTIMIZACAO.md lei 9): nenhum teste via que o Windows nunca renomeava, porque
copiar e apagar dá o mesmo resultado visível. O teste novo olha o que só o
caminho principal preserva (a data do arquivo).

---

### 3. Lixeira em lote (04/11/2026)

**Sintoma.** Nenhum relatado; item 3 da leitura de 04/11. Excluir uma
seleção grande levava segundos por centena de arquivos.

**Base.** `crates/nohrs-services/examples/trash_ab.rs` (release, mediana de 3
rodadas alternadas, Lixeira real, Premiere aberto). O lado "antes" é a literal
do `trash_selection` no Windows: um `ops::trash_path(item, None)` por item. A
linha de 1 arquivo é o controle: os dois lados fazem o mesmo trabalho e deram
o mesmo tempo (29,8 contra 28,4 ms).

#### Uma operação do Shell e uma thread por arquivo

Cada `trash_path` abre uma thread própria (por causa do pânico de COM da
rodada 20), entra num apartamento COM, cria um `IFileOperation`, executa e
fecha. Agora `ops::trash_paths` entrega a seleção inteira ao
`trash::delete_all`: uma thread, um `IFileOperation`.

Os cuidados, porque o `delete_all` do crate é tudo-ou-nada no que **diz**, não
no que **faz**:
- um caminho que não abre (apagado por outro programa) derruba o lote inteiro
  antes de começar, então caminho que já não existe fica fora do lote e é
  tentado sozinho, para sair com a mensagem do sistema como antes;
- se um item falha no meio, os outros vão e o crate só diz "algumas operações
  foram abortadas"; depois de um lote que falhou, o que **ainda está no disco**
  é tentado um a um, o que dá nome e motivo à falha. O que o lote já levou não
  é tentado de novo.
- Onde o ledger é obrigatório (macOS), continua item por item, porque cada
  item é capturado e registrado antes de sair.

| Para a Lixeira | Antes | Depois |
|---|---:|---:|
| 1 arquivo (controle) | 29,8 ms | 28,4 ms |
| 10 arquivos | 300 ms | 103 ms |
| 200 arquivos | 7,0 s | 1,9 s |
| 1.000 arquivos | 35,8 s | 9,9 s |

3,6× nas seleções grandes. O que sobra, ~10 ms por item, é trabalho da
própria Lixeira do Windows (os arquivos `$I`/`$R` de cada item) e não mudou.

**Cobertura.**
- O plano é função pura (`trash_in_one_batch`) com as chamadas do sistema
  trocadas por contadores: `a_selection_goes_to_the_trash_in_one_call` (1
  chamada de lote, 0 avulsas),
  `a_failed_batch_retries_only_what_is_left_and_names_the_file_that_failed`,
  `a_path_already_gone_stays_out_of_the_batch_and_is_still_reported`,
  `nothing_on_disk_makes_no_batch_call`. E um real:
  `trash_paths_moves_real_files_and_reports_the_missing_one` (deixa 4 itens na
  Lixeira a cada execução, como o teste de COM já deixava).
- Mutação: trocar o lote por laço reprova três; tentar de novo também o que o
  lote já levou reprova um.
- O fio entre o explorador e `trash_paths` (uma linha em `file_ops.rs`) não
  tem teste próprio; está provado pelo benchmark, que chama a mesma função.
- `trash_ab` confere que todos os arquivos saíram e que nenhum lado relatou
  falha antes de aceitar o tempo, e no fim tira da Lixeira só os itens cuja
  origem é a pasta da rodada (7.266 = 3 × 1.211 × 2).

**Não conferido.**
- Não visto na tela (o Nohrs do dono aberto em `target\release`).
- Falha real no meio do lote (arquivo em uso) só no teste com contadores; não
  provocada contra o Shell.
- O `noh rm` da linha de comando continua item por item (`nohrs-cli/src/rm.rs`).
- Excluir permanentemente (Shift+Delete) não foi tocado: é `remove_dir_all`
  por item, sem Shell.

---

### 4. Colar com `CopyFileExW` (04/11/2026)

**Sintoma.** Nenhum relatado; item 2 da leitura de 04/11. Colar uma pasta de
fotos apagava a data de todas: cada cópia saía com a hora do colar.

**Base.** `crates/nohrs-services/examples/copy_ab.rs` (release, mediana de 5
rodadas alternadas, origem no C:, Premiere aberto). O lado "antes" é a literal
do `copy_path_no_replace` anterior: `create_new` + `std::io::copy` +
`set_permissions`. Com os dois lados iguais, antes da mudança, o ruído chegou a
**2,4×** (2.000 arquivos para o D:, 7,2 contra 3,0 s) e a 5% no arquivo grande
do C:.

#### A cópia sem substituir não usava a cópia do sistema

O `copy_path` (que substitui) já usava `fs::copy`, que no Windows é
`CopyFileExW`. O que o colar usa, sem substituir, reivindicava o nome com
`create_new` e copiava com `std::io::copy`: os bytes passavam pelo programa em
blocos, a data de modificação virava "agora" e as streams alternativas ficavam
para trás. Agora, no Windows, é `CopyFileExW` com `COPY_FILE_FAIL_IF_EXISTS`,
que cria o destino com `CREATE_NEW` dentro da própria chamada: é a mesma
reivindicação, e nada que pegou o nome antes é sobrescrito.

O que muda no relato de falha (`ClaimFailure`): "nome ocupado" volta como
`Nothing`, porque é respondido antes de qualquer escrita. Outra falha volta como
`Nothing` se não há nada no destino (o `CopyFileExW` apaga o que escreveu ao
desistir), e como `PartialDestination` sem identidade se há algo lá: a limpeza
de quem chamou não apaga nada (como já era no Windows) e o relato diz onde está.

| Colar (copiar sem substituir) | Antes | Depois | 2ª rodada: antes | depois |
|---|---:|---:|---:|---:|
| 2.000 × 64 KB, C: → C: | 2.429 ms | 1.521 ms | 2.776 ms | 1.704 ms |
| 2.000 × 64 KB, C: → D: | 1.755 ms | 861 ms | 2.776 ms | 1.301 ms |
| 512 MB, C: → C: | 835 ms | 252 ms | 721 ms | 180 ms |
| 512 MB, C: → D: | 1.316 ms | 1.026 ms | 1.327 ms | 1.424 ms |
| data de modificação mantida | não | **sim** | não | **sim** |

O que os números sustentam: o arquivo grande no mesmo disco ficou ~4× mais
rápido (acima do ruído de 5% desse caso); os arquivos pequenos ficaram 1,6–2×
mais rápidos nas duas rodadas e nos dois destinos, mas o ruído desse caso é do
mesmo tamanho; o arquivo grande para o D: não mudou (o disco é o limite). O
resultado firme é a data.

**Cobertura.**
- `a_pasted_copy_keeps_the_modification_time` (arquivo solto e dentro de
  pasta), `a_pasted_copy_keeps_a_read_only_file_read_only`,
  `a_source_another_program_holds_leaves_nothing_at_the_destination` (origem
  aberta sem compartilhamento: `Nothing`, destino ausente, código 32 cru).
- Mutação: voltar ao `io::copy` reprova os dois de atributo; tirar o
  `FAIL_IF_EXISTS` reprova `the_fallback_move_claims_the_destination_before_writing_to_it`;
  marcar toda falha como parcial reprova o da origem travada.
- `copy_ab` recusa medir se a cópia de algum lado diferir da origem.

**Não conferido.**
- Não visto na tela (o Nohrs do dono aberto em `target\release`).
- Pasta de rede (onde o `CopyFileExW` deixa o servidor copiar) e o Google
  Drive não foram medidos.
- Falha no meio de uma cópia grande (disco cheio) não foi provocada; o ramo
  `PartialDestination` não tem teste no Windows.
- Ainda sem progresso nem cancelamento na interface: o `CopyFileExW` aceita
  rotina de progresso e sinal de cancelar, e é por aí que a barra "só aparece
  quando demora" (OTIMIZACAO.md §3.10) entraria.
- Pastas copiadas continuam com a data do colar (só arquivos mantêm).

---

### 5. Recarregar não é reabrir (04/11/2026)

**Sintoma.** Nenhum relatado; item 4 da leitura de 04/11. Excluir, colar,
renomear ou soltar um arquivo relia a pasta como se fosse a primeira visita:
fechava todas as pastas abertas na vista Árvore, fechava a pré-visualização de
texto e de imagem (mas não a de vídeo, PDF e 3D, que ficavam mostrando um
arquivo que podia nem existir mais), jogava fora os nomes já moldados da grade
e, numa pasta lenta, trocava as linhas por "Reading…" depois de 220 ms.

**Base.** Estrutural, por leitura de `navigation.rs` (`reload`,
`apply_listing`) e dos seis lugares que chamam `reload` depois de uma operação
(`file_ops.rs`, `drag.rs`, `context_menu.rs`). Nada cronometrado.

#### O `reload` passa a saber se é a mesma pasta

Em vez de mudar cada um dos seis chamadores, o painel guarda de que pasta vêm
as linhas na tela (`listed_cwd`). Uma leitura da mesma pasta é **refresh**
(OTIMIZACAO.md §3.9, o `RefreshDirectory` do Salamander):
- as pastas abertas na Árvore continuam abertas e são **relidas junto**, na
  mesma thread da listagem, para que um arquivo excluído dentro delas saia da
  tela; pasta aberta que sumiu fecha;
- o visualizador fica, a menos que o arquivo dele tenha sumido — e então fecha
  **inteiro**, inclusive vídeo, PDF e modelo (`clear_preview`, agora a única
  lista do que compõe o visualizador, usada também por `load_preview`). Saber
  se o arquivo sumiu é uma ida ao disco, então é perguntado na thread da
  leitura, não na da interface (§2.12);
- os nomes moldados da grade ficam (a chave é o nome);
- numa pasta lenta, as linhas velhas ficam até as novas chegarem.

Entrar em outra pasta continua como antes. Uma listagem que falha zera
`listed_cwd`, para a próxima leitura ser visita.

E o renomear: a seleção é por caminho, e o caminho muda; a linha renomeada
saía da seleção. Agora seleção, âncora e linha ativa seguem o nome novo, como
no Explorador.

**Cobertura.**
- `a_refresh_keeps_tree_folders_open_and_reads_them_again` (arquivo excluído
  dentro da pasta aberta sai; pasta aberta excluída fecha),
  `a_visit_to_another_folder_starts_with_the_tree_closed`,
  `a_refresh_keeps_the_preview_while_its_file_is_there`,
  `a_renamed_row_stays_selected`.
- Mutação: tratar todo `reload` como visita reprova dois; manter as pastas
  abertas sem relê-las reprova um; tirar o ajuste da seleção reprova um.
- 167 testes do `nohrs-pages` verdes.

**Não conferido.**
- Não visto na tela (o Nohrs do dono aberto em `target\release`).
- Ganho de tempo não medido: deixar de moldar de novo os nomes de uma grade
  grande a cada exclusão é estrutural.
- Renomear o próprio arquivo do visualizador fecha o visualizador (o caminho
  antigo sumiu); o Explorador mostraria o arquivo com o nome novo.
- O refresh ainda relê a pasta inteira. Corrigir a lista na memória para uma
  exclusão ou renomeação de um item só (sem reler 35 mil) é a próxima peça,
  e ainda não foi feita.

---

### 6. O visualizador segue as setas (04/11/2026)

**Sintoma.** Pedido do dono depois da leitura de 04/11. A leitura dizia que
"cada seta lê o arquivo inteiro" — **errado**: as setas (`move_active`) só
moviam a seleção, e o visualizador ficava no último item clicado. O código
mostrou isso antes de qualquer mudança (OTIMIZACAO.md §1.2: leitura diz o que
deveria acontecer). Seguir as setas, como o painel de visualização do
Explorador, foi decisão do dono; e é ao segui-las que o atraso e o
cancelamento passam a ser necessários.

**Base.** Nada a medir antes: o visualizador não seguia as setas.

#### Seguir a linha ativa, só quando ela para

`move_active_following` move a linha e chama `follow_active_in_preview`, que
espera `PREVIEW_FOLLOW_DELAY` (80 ms) antes de abrir o visualizador da linha
ativa. Cada tecla substitui a espera anterior (o `Task` guardado em
`preview_follow`), então segurar a seta para baixo lê só o arquivo onde ela
para (§5.4). 80 ms é pouco mais que a repetição do teclado (~30 ms) e fica
abaixo dos 100 ms em que o visualizador pareceria atrasado (§3.10).

Os cuidados:
- **o clique ganha**: quando a espera termina, ela só abre se a linha ativa
  ainda é a dela; um clique no meio já abriu o próprio visualizador;
- **visualizador escondido não lê nada**;
- só pastas e arquivos, como o clique (`row.rs`);
- **o carregamento do visualizador não é mais solto** (`detach`): fica em
  `preview_load`, e o seguinte o substitui, o que descarta a leitura de um
  arquivo que ainda não começou a ser lida. Uma leitura já em andamento
  (listar pasta, `7z.exe l`) não pode ser interrompida; termina e é
  descartada pela conferência de caminho que já existia. É o atraso que
  impede de começar leituras à toa.

**Cobertura.**
- `the_preview_follows_the_arrow_keys_once_they_stop` (com o relógio de teste:
  nada lido durante as teclas, cada tecla recomeça a espera, a última é lida),
  `a_hidden_preview_does_not_follow_the_arrow_keys`,
  `a_click_during_the_wait_is_not_undone_by_it`.
- Mutação: abrir na hora reprova o primeiro; não conferir a linha ativa
  reprova o do clique; ignorar o visualizador escondido reprova o dele.
- 170 testes do `nohrs-pages` verdes.
- **Visto na tela**, numa segunda cópia com `LOCALAPPDATA` próprio (setas por
  `PostMessage`, captura por `PrintWindow`), sem tocar a janela do dono: uma
  seta → o visualizador mostra `b.txt`; duas setas seguidas → 30 ms depois a
  seleção está em `c.txt` e o visualizador ainda em `b.txt`; 600 ms depois,
  `c.txt`.

**Não conferido.**
- Pasta grande com arquivos pesados (PDF, vídeo, `.rar`) segurando a seta: o
  ganho de não abrir 7-Zip ou o player a cada linha é estrutural, não
  cronometrado.
- A lista antiga do gpui-component (`ListEvent::Select` em `list_setup.rs`)
  continua abrindo na hora; não é a lista desenhada hoje.

---

### 7. Imagem ajustada vem do Shell, não da decodificação inteira (04/11/2026)

**Sintoma.** Nenhum relatado; item 10 da leitura de 04/11. O visualizador
desenhava imagens com `img(PathBuf)`, que decodifica o arquivo inteiro em
resolução cheia, mesmo quando o painel mostra umas centenas de pixels dele.

**Base.** Roteiro `measure-image.ps1` (no scratchpad da sessão): uma cópia do
Nohrs com `LOCALAPPDATA` próprio abre uma pasta com uma foto de 8000×6000
(48 MP, JPEG de 0,9 MB), um clique por `PostMessage` na linha da foto, e
capturas por `PrintWindow` a cada ~50 ms até a foto aparecer no painel; a
memória é a `PrivateMemorySize64` do processo. Release, Premiere aberto, 3
rodadas alternadas antes/depois.

| Foto de 48 MP no visualizador | Antes | Depois |
|---|---:|---:|
| Memória privada ao abrir | 122 → **677 MB** (+555) | 122 → **124 MB** (+2) |
| Até a imagem aparecer | 336 / 376 / 405 ms | 75 / 105 / 108 ms |
| Depois de zoom acima de 100% | 493 MB | 676 MB |

+555 MB é quase o triplo dos 192 MB da imagem em RGBA (cópias do
decodificador, a conversão para BGRA e a do atlas da GPU), e ficava enquanto a
foto estivesse no visualizador. A linha do zoom não é regressão: acima de
100% a imagem é decodificada inteira de propósito, nos dois lados.

#### O que mudou

No ajuste (zoom ≤ 100%) a imagem é a miniatura do Shell no tamanho do painel
(`fitted_picture` em `view/preview.rs`): o lado maior do painel em pixels de
tela, arredondado para 512, 1024 ou 2048 (`fitted_edge`), pelo mesmo cache e
pelas mesmas filas das miniaturas da grade, que já cancelam o que sai da tela
e respeitam o teto de memória (passada 1). Enquanto ela é feita, a miniatura
menor do mesmo arquivo (a da grade) aparece no lugar; sem nenhuma, um quadro
vazio, e não a decodificação inteira, que custaria a memória que isto evita
para ser trocada logo depois.

A decodificação inteira continua onde ela é o que se precisa:
- zoom acima de 100% — e, ao voltar para o ajuste, ela é solta
  (`gif_cache = None`);
- GIF animado (a miniatura do Shell é um quadro só);
- formato de que o Shell não tem miniatura (um codec que o Windows não tem).

Para saber esse último caso, a cache de miniaturas passou a guardar se a
resposta é a imagem de verdade ou o ícone do tipo no lugar dela (`settled`):
depois das novas tentativas o ícone fica `Ready` para sempre, e `retry_at` não
diz mais nada. `Thumbnails::picture` responde `Shown`, `Waiting` ou
`Unavailable`, e o ícone nunca aparece como se fosse a foto.

**Cobertura.**
- `a_fitted_image_is_not_decoded_whole` (ajustada: miniatura; zoom: arquivo;
  GIF: arquivo), `a_fitted_image_is_asked_for_in_steps_that_cover_the_pane`,
  `the_preview_takes_the_picture_and_never_the_stand_in`.
- Mutação: sempre decodificar inteira e ignorar o zoom reprovam o primeiro;
  aceitar o ícone como foto e mostrar qualquer tamanho menor enquanto espera
  reprovam o terceiro.
- 173 testes do `nohrs-pages` verdes.
- **Visto na tela** (capturas da cópia isolada): a foto ajustada com o mesmo
  enquadramento de antes, e o zoom a 173% pela decodificação inteira.

**Não conferido.**
- Foto real (a de teste é de faixas de cor): nitidez da miniatura do Shell e a
  orientação EXIF, que o Shell aplica e a decodificação inteira do gpui talvez
  não.
- PNG com transparência: se a miniatura do Shell mantém o alfa no visualizador.
- `.webp` sem a extensão de imagem da Microsoft: o caminho `Unavailable` →
  decodificação inteira está testado com contadores, não com o arquivo real.
- Painel maior que 2048 pixels de tela (4K com escala alta): a imagem
  ajustada sai de 2048 e fica um pouco mais macia.
- A memória depois de voltar do zoom para o ajuste não foi medida.

**Lição.** No caminho de uma ação que se repete (clicar de foto em foto), o
custo de memória vem do tamanho do arquivo, não do que a tela mostra; pedir o
tamanho mostrado (OTIMIZACAO.md §3.5) vale também para o visualizador, e não
só para a grade.

---

### 8. Os itens menores da leitura de 04/11 (04/11/2026)

Seis mudanças pequenas, cada uma com teste e mutante próprios. Nenhuma vista
na tela.

#### Renomear só a caixa numerava o arquivo (defeito)

`foto.jpg` → `Foto.jpg` saía `Foto (1).jpg`: no NTFS `Foto.jpg` está ocupado —
pelo próprio arquivo — e `would_conflict` dizia que sim. Agora
`ops::would_collide_with_another(src, dst)` só conta colisão com **outra**
entrada: no Windows as duas são comparadas por `canonicalize`, que devolve o
caminho final com a caixa gravada; um link em `dst` conta como colisão mesmo
apontando para `src`, porque renomear por cima o destruiria.
Testes: `a_rename_that_only_changes_case_collides_with_nothing` (ops) e
`a_rename_that_only_changes_case_is_not_numbered` (explorador).

#### Renomear fora da thread da interface (§2.12)

`commit_rename` perguntava ao disco na thread do gpui três vezes (o nome está
ocupado? qual número está livre? e o próprio `rename`): num compartilhamento
que parou de responder, a janela congelava. Agora roda em `background_spawn`, e
`finish_rename` aplica a resposta (seleção segue o nome novo, refresh,
mensagem). Teste: `a_rename_runs_off_the_ui_thread` (o executor de teste só roda
trabalho de fundo quando deixado: logo depois do `commit_rename` o arquivo
ainda tem o nome antigo). `rename_to_invalid_name_reports_error` ganhou um
`run_until_parked` antes de ler a mensagem — a conferência é a mesma; a
resposta é que passou a chegar depois.

#### A listagem não ordena o que vai ser ordenado de novo

`list_dir_impl` ordenava a pasta inteira por `to_lowercase`, e o explorador e a
Árvore ordenam de novo pela ordem escolhida. Medido com
`examples/listing_sort_cost.rs` (release, 3 rodadas): WinSxS\Manifests, 35.628
itens, **8,7–10,4 ms de 82–101 ms** da listagem; System32, 0,5 de 4,8 ms.
Agora só ordena quando há página (`offset > 0 || limit < total`), que é quando
a ordem é contrato: sem ela o cursor da página seguinte não continuaria de
onde a anterior parou. O teste `lists_files_and_dirs_sorted_case_insensitively`
virou `lists_files_and_dirs` (o contrato mudou, de propósito) e
`pages_are_sorted_case_insensitively` guarda o que ficou. O `listing_ab` da
passada 1 compara as listagens sem olhar a ordem.

Armadilha que o mutante pegou: a primeira versão do teste de páginas passava
**sem** a ordenação, porque o NTFS já devolve os nomes na ordem dele, sem
caixa. Só com um `_` no nome (0x5F: depois das letras para o NTFS, que compara
em maiúsculas; antes delas para o `to_lowercase`) as duas ordens divergem e o
mutante reprova.

#### Miniatura nova quando o arquivo muda (§6.1)

A cache de miniaturas é por caminho e só esvaziava ao trocar de pasta: um
arquivo sobrescrito por "colar e substituir" ou salvo por outro programa
mantinha a imagem velha. No refresh (passada 5), cada item cujo tamanho ou
data mudou desde a leitura anterior é esquecido (`Thumbnails::forget`) e
pedido de novo; os outros mantêm a sua. Miniatura solta a textura; ícone só
perde o lugar, porque um ícone pode ser uma textura dividida pelo tipo inteiro.
Teste: `a_refresh_forgets_the_picture_of_a_file_that_changed`. Não pega:
sobrescrita no mesmo segundo com o mesmo tamanho (a data da listagem é em
segundos), nem mudança sem refresh (a pasta não é vigiada).

#### O visualizador não lê 2 MB de binário

Arquivo desconhecido era lido inteiro (até 2 MB) para o `from_utf8` falhar;
num drive na nuvem, isso é baixar o arquivo. Agora `read_unless_binary` lê os
primeiros 8 KB e para num byte zero, que nenhum texto UTF-8 mostrável tem.
Texto em UTF-16 já caía em "não disponível" antes e continua. Teste:
`a_binary_file_is_not_read_past_its_first_bytes` (inclusive texto maior que os
8 KB chegando inteiro).

#### A pasta no visualizador na ordem da lista (§7.5)

`read_folder` comparava com `to_lowercase` dos dois nomes a cada comparação e
punha `f10` antes de `f2`, ao contrário da lista principal. Agora usa a mesma
chave (`entries::SortName`, o `LCMapStringW` + `SORT_DIGITSASNUMBERS` da
passada 1), uma vez por nome; os arquivos compactados também. Teste:
`a_folder_in_the_preview_is_in_the_listing_order`.

**Cobertura.** Mutação, um por item: comparar colisão sem olhar a entrada,
renomear na thread da interface, nunca ordenar páginas, não esquecer a
miniatura, não farejar binário, voltar ao `to_lowercase` — cada um reprova o
seu teste (o de renomear na thread também reprova
`rename_collision_falls_back_to_numbering`). 78 testes do `nohrs-services` e
178 do `nohrs-pages` verdes.

**Não conferido.** Nada visto na tela. A vantagem de renomear em segundo plano
num compartilhamento travado não foi provocada; o renomear de caixa não foi
testado em FAT32/exFAT nem no Google Drive (`I:`).

---

### 9. Sem dormir na thread da interface (22/11/2026)

**Sintoma.** Nenhum relatado; passada pedida pela revisão
`docs/prime-files-优化方案.md` §2, que manda caçar 	hread::sleep rodando na
thread da interface. Dos 30 sleep do código de produção, quase todos estão em
thread própria e dormir ali é legítimo: a vigia do 
eveal e da barra do
`dialog_jump`, a caixa de diálogo de arquivo (thread `nohrs-file-dialogs`), o
`foreground` (thread criada só para isso), o processo de arquivamento, o
`media` e o laço de mensagens da `shell_windows`. Dois não estavam:

#### A busca da barra de salto congelava a janela à espera do índice

`RootView::search_for_dialog_once` roda em `cx.spawn` — a thread do gpui. Se
o índice ainda está subindo, ele dormia **250 ms ali dentro**, com até 10
tentativas: até 2,5 s de janela parada, justo quando a pessoa está digitando na
barra. Agora espera o timer do executor de segundo plano
(`cx.background_executor().timer(250ms).await`), que devolve a thread entre um
tique e outro.

#### Entregar arquivos a um diálogo de outro programa travava a janela

`ExplorerPane::activate_entry` chamava `file_dialog::deliver` na thread da
interface. Para vários arquivos, `fill_and_open` dorme 300 ms: o Shell move o
diálogo de pasta de forma assíncrona e a automação espera o movimento. Passou
para `cx.background_spawn`; o diálogo é janela de outro processo, então o
preenchimento não depende de quem chama.

**Base / depois.** Sem número de antes: os dois caminhos não foram medidos na
tela (o primeiro só dura a janela curta em que o índice não está pronto; o
segundo aparece ao mandar vários arquivos para um diálogo alheio) e a
justificativa é estrutural — nada que durma pode rodar na thread que desenha.
O que dá para afirmar: antes, o primeiro somava 250 ms × tentativas de bloqueio
da thread de desenho; hoje, zero.

#### Também nesta passada (estabilidade, não desempenho)

O registro de pânico em `panic.log` só era instalado quando
`diagnostics.runtime_log` estava ligado — e ele vem desligado por padrão. Numa
janela release não há console, então um crash não deixava rastro algum. Agora
`install_panic_log` é chamado no começo de `NohrsApp::run` (antes de ler a
configuração) e de novo, sem reinstalar o hook, quando o log sobe; o registro
ganhou também o diretório de trabalho, que muitas vezes é a única pista que a
pilha não dá.

#### Build do instalador

`[profile.dist]` passou a usar `lto = "fat"`, `codegen-units = 1` e
`debug = "line-tables-only"`: `LumaFiles.exe` vai de 43,2 MB (release) para
**35,7 MB** (-17%), ao custo de ~10 min de build que só o instalador paga. A
build `release` do dia a dia não mudou. Medições em `docs/size.md`.

**Cobertura.** 431 testes de `nohrs-pages` (`--test-threads=1`) e 95 de
`nohrs-core` verdes depois das mudanças; `cargo build --workspace` sem
aviso novo.

---

### 10. Memória (24/11/2026)

**Sintoma.** Nenhum relatado: esta é a medida de partida do item O0 de
[`LumaFiles-内存与缩略图优化方案.md`](./LumaFiles-内存与缩略图优化方案.md) (v1.1).
Antes de mexer no consumo — índice de nomes, miniaturas, listas — o número de
partida, medido com o mesmo instrumento que cada item seguinte vai usar.

**Método.** `scripts/measure-memory.ps1` sobe `LumaFiles.exe` (release,
hard link do build) com um perfil portátil próprio: `userdata` ao lado do
executável e `[storage] portable_mode = true`, então nada do perfil real
(configuração, estado, histórico, logs) é lido nem escrito. O script amostra a
cada segundo `PrivateMemorySize64`, `WorkingSet64`, handles e objetos GDI/USER,
roda cada cenário três vezes e a tabela traz a mediana das três. Os CSV de cada
rodada ficam em `target\tmp\perf\runs`, fora do repositório. As pastas de teste
são geradas pelo mesmo script em `target\tmp\perf\data`: 100.000 arquivos de
256 B, 10.000 fotos de 800x600 e 100.000 subpastas.

No cenário 2 o instantâneo real do índice (261 MB, dois segmentos, ~3,7 milhões
de nomes) foi copiado para o perfil, com apenas o carimbo `built` (bytes 8..16)
atualizado para agora: o instantâneo da máquina tinha dois dias e o app o refaz
depois de seis horas, e o que o item O1 muda é o caminho de *carga*. Com o
carimbo antigo a mesma rodada mede carga **e** reconstrução: pico de 1.174 MB e
estável de 744 MB, registrado aqui para não ser confundido com o número de cima.

| Cenário | Pico privado | Estável privado | Pico working set | Handles | GDI | USER |
|---|---:|---:|---:|---:|---:|---:|
| 1. Início a frio, 30 s parado | 175.9 MB | 134.9 MB | 115.9 MB | 1158 | 55 | 50 |
| 2. Índice próprio: carga do instantâneo de 261 MB | 588.8 MB | 505.9 MB | 542.9 MB | 1217 | 55 | 52 |
| 3. Pasta de 100.000 arquivos + 5 letras no filtro | 269.0 MB | 242.2 MB | 211.6 MB | 1227 | 55 | 58 |
| 4. 10.000 fotos, grade a 256 px, rolagem até o fim | 499.7 MB | 454.0 MB | 316.0 MB | 1167 | 55 | 51 |
| 5. Duas abas com a mesma pasta de fotos, rolagem nas duas | 881.9 MB | 826.6 MB | 376.0 MB | 1245 | 55 | 53 |
| 6. 100.000 subpastas na lista, rolagem contínua | 275.0 MB | 273.2 MB | 231.0 MB | 1097 | 55 | 51 |
| 7. Janela fechada, processo residente, 10 min | 221.9 MB | 148.4 MB | 115.5 MB | 1160 | 55 | 51 |
| 8. Abrir e fechar a pasta grande 10 vezes | 327.6 MB | 173.0 MB | 213.5 MB | 1168 | 55 | 50 |

O cenário **2b** (carregar o instantâneo com o carimbo *original*, que faz o app
refazer o índice em seguida) foi acrescentado depois do O0, com o mesmo instrumento
e o mesmo perfil: base de **1.094,4 MB de pico e 757,7 MB estáveis** (mediana de três
rodadas). É o estado em que a maioria dos usuários está, porque o instantâneo passa
de seis horas.

**O que os números dizem.** A base ociosa é 134.9 MB (cenário 1, estável).
O índice próprio soma 371.0 MB ao estável e 453.9 MB ao pico (cenário 2: 505.9 MB
estável) — a tabela e as strings que o plano estimou em ~350 MB, medidas.

A pasta de 100.000 arquivos com filtro (cenário 3) chega a 269.0 MB de pico; a de
100.000 subpastas na lista (cenário 6) fica em ~275 MB com ~8.600 subpastas roladas
em 90 s — é o ícone por caminho que O4 passa a contar no orçamento.

As fotos a 256 px (cenário 4) são o maior consumidor isolado: 499.7 MB de pico e
454.0 MB estável, ~320 MB acima da base — mais do que os 192 MB que o teto por
painel conta, porque ele conta só as miniaturas `Ready` e não o que a GPU, o cache
do Shell e as decodificações em voo seguram enquanto a rolagem corre. As duas abas do cenário 5 mantêm dois caches assim ao mesmo tempo
(881.9 MB de pico), que é exatamente o que o orçamento global de O3 tem de limitar.

O cenário 7 (10 min de janela fechada, processo residente) fecha em 148.4 MB
estável, e o 8 (dez idas e voltas numa pasta grande) em 173.0 MB: comparados com
a base ociosa, os dois mostram se fica memória para trás.

#### Notas de método (o que não é o app)

O cenário 5 foi executado com **duas abas** e não com dois painéis: os atalhos de
painel (`Ctrl+\`, `Ctrl+Shift+→`, `Ctrl+→`) não disparam a partir de teclado
sintético — só `Ctrl+T`, `Ctrl+W`, `Ctrl+=` e `Ctrl+-` disparam —, mas cada aba
tem o próprio cache de miniaturas, que é o que o orçamento global de O3 cobre.
O cenário 6 rolou ~8.600 das 100.000 subpastas em 90 s (a captura final mostra
`folder_008600`); rolar as 100.000 levaria ~17 min, então as comparações usam o
mesmo tempo de rolagem, não a pasta inteira.

Um perfil portátil novo também recebe, na primeira execução, uma cópia do índice
de nomes do perfil anterior (258 MB em `cache\everything.idx`); ela acontece em
todas as rodadas por igual e não aparece na memória privada, só no disco. O
cenário 2 substitui esse arquivo pelo instantâneo do teste antes de medir.

Os logs reais desta máquina ficam em `target\release\userdata\state\logs`
(752,4 MB em sete arquivos); a mesma coleção está no perfil não portátil,
`~\.local\state\nohrs\logs`, que é a origem da cópia de migração. O app
copia o perfil anterior para um perfil portátil novo na primeira execução, e
essa cópia acontecia dentro da janela de amostragem de cada rodada. Para o
baseline ser do app e não da cópia, os logs foram movidos para
`~\.local\state\nohrs-logs.baseline-off` durante as medições e devolvidos
depois.

#### O5: por que 549 MB de log num dia

`nohrs.log.2026-11-15` tem 2.532.006 linhas, e **2.531.269** delas são a mesma
mensagem:

```
{"level":"ERROR","message":"could not find asset at path \"icons/menu.svg\"",
 "log.file":"third_party\\gpui\\src\\elements\\svg.rs","log.line":121,"target":""}
```

É um ícone de menu pedido a cada quadro: o elemento SVG registra um ERROR por
desenho porque `icons/menu.svg` não existe nos assets. O mesmo vale para 14/11
(579.744 de 581.085 linhas) e, em menor escala, para 13/11 e 16/11. O log cresce
com o tempo em que a janela fica aberta, não com o uso. O item O5 tem duas
partes: pôr o asset que falta (ou parar de desenhá-lo) e limitar o arquivo; a
primeira é a que apaga o loop.

| Dia | Tamanho | Linhas ERROR |
|---|---:|---:|
| 2026-11-13 | 48,3 MB | não medido |
| 2026-11-14 | 126,3 MB | 579.884 |
| 2026-11-15 | 549,5 MB | 2.531.388 |
| 2026-11-16 | 64,4 MB | não medido |
| 2026-11-17 | 48 KB | — |

No total, `target\release\userdata\state\logs` guarda 752,4 MB em sete arquivos
(e o perfil não portátil, `~\.local\state\nohrs\logs`, tem a mesma coleção).

---

#### O2 + O2b: capacidade sobrando e reconstrução por partição

`Segment::shrink` solta o que a tabela cresceu além do tamanho: `names`, `lower`,
`pinyin`, `entries`, `non_ascii` e `paths`. Ele é chamado quando uma partição fica
pronta — o fim da caminhada de um disco (que é o mesmo caminho do primeiro índice e
da reconstrução, então o O2b sai no mesmo ponto: cada partição é encolhida assim que
termina, enquanto a próxima ainda está sendo lida e o índice antigo continua
servindo as buscas) e o fim da passagem de pinyin de uma partição carregada.

| Cenário | Antes (pico / estável) | Depois (pico / estável) | Δ estável |
|---|---:|---:|---:|
| 2. carga do instantâneo fresco | 615,4 / 531,8 MB | 615,6 / 530,2 MB | −1,6 MB |
| 2b. carga e reconstrução | **1.094,4 / 757,7 MB** | **1.031,2 / 579,8 MB** | **−177,9 MB** |

A carga não muda: o `Vec<Entry>` já nasce com a capacidade exata e as duas strings
vêm de um `String::from_utf8` do tamanho certo, então o `shrink` ali não tem o que
soltar (a passagem de pinyin cresce pouco). O ganho está na reconstrução, que era o
estado comum: **−63 MB de pico e −178 MB estáveis**, porque a tabela nova é
encolhida partição a partição enquanto ela cresce, em vez de guardar até o fim a
capacidade que o `push` reservou.

Os números "antes" desta tabela foram refeitos na mesma máquina, com o mesmo
instrumento e minutos antes do "depois" (o código do O2 guardado com `git stash` e
devolvido em seguida), porque a tabela do O0 foi medida com um binário anterior ao
O5a; comparar com ela ficaria com a diferença de duas builds. O cenário 2 agora
recebe o instantâneo com o carimbo `built` ajustado a cada rodada (o arquivo
`target\tmp\perf\index-fresh.idx` envelhece), senão ele mede reconstrução em vez de
carga.

#### O1: carga em fluxo

`load` passou a ler o arquivo por um `BufReader`: cabeçalho, contagem de segmentos e,
por segmento, os comprimentos, as entradas em blocos de 4.096 e as duas strings
direto no tamanho declarado, em vez de `std::fs::read` do arquivo inteiro (261 MB)
seguido de cópia de cada segmento. Todo comprimento é conferido contra o que resta
do arquivo antes de ser alocado, e o arquivo tem de terminar exatamente onde o
último segmento termina — um byte a mais devolve `None` (teste novo).

| Cenário 2 (carga do instantâneo fresco) | Antes | Depois | Δ |
|---|---:|---:|---:|
| pico privado | 615,6 MB | **529,2 MB** | **−86,4 MB** |
| estável privado | 530,2 MB | 529,0 MB | −1,2 MB |
| pico de working set | 545,5 MB | 458,5 MB | −87,0 MB |

O ganho é o buffer do arquivo inteiro, que existia só durante a carga: o pico agora
é o próprio valor estável. Não é o 1/4 de GB estimado no plano — o buffer era
liberado depressa, como a diferença de 83 MB entre pico e estável do O0 já dizia —,
mas passa do limite de 50 MB combinado para manter a mudança.

#### O3: um orçamento de miniaturas para o processo inteiro

Cada `Thumbnails` tinha o próprio teto de 192 MB. Agora há um só, de **512 MB**,
para todos — cada painel, cada aba, a barra lateral — num contador compartilhado
(`READY_BYTES`): um cache que encontra o orçamento estourado solta o que tem fora da
tela, do mais antigo para o mais novo; se não houver o que soltar, espera o próximo
quadro ou a vez de outro cache (não há LRU global, como o plano pede). O teto por
cache virou campo só para os testes (produção fica sem limite próprio), e o `Drop`
devolve os bytes de um cache fechado ao contador, senão o orçamento ficaria gasto
para sempre. O evento deixa uma linha de log com o total antes e depois.

| Cenário | Antes (pico / estável) | Depois (pico / estável) | Δ estável |
|---|---:|---:|---:|
| 4. 10.000 fotos a 256 px | 1.018,3 / 912,4 MB | 1.391,4 / 1.185,4 MB | +273,0 MB |
| 5. duas abas com fotos | 862,1 / 796,5 MB | 1.044,6 / 963,5 MB | +167,0 MB |

O número sobe, e é o que o plano já previa: um painel sozinho pode agora usar até
512 MB (antes parava em 192 MB), então rolar um álbum grande recarrega menos; e as
duas abas dividem os mesmos 512 MB em vez de 192 MB cada. O que muda é o **teto**: o
total de miniaturas não passa de 512 MB mais a miniatura que estiver chegando.

**Prova.** Numa rodada do cenário 4 com `runtime_log = true`, o log do app tem uma
linha do evento:

```
{"level":"INFO","message":"the shared thumbnail budget evicted",
 "before":537968640,"after":402259968,"cap":536870912,
 "target":"nohrs_pages::explorer::thumbnails"}
```

537.968.640 bytes são os 512 MiB do teto mais a miniatura que chegou naquele quadro;
depois da evicção o total fica em 402.259.968 (três quartos do orçamento, para o
próximo quadro não evictar de novo). No cenário 5, com as duas abas, não houve
nenhuma linha dessas: o total compartilhado ficou abaixo de 512 MB durante toda a
rodada (a linha só aparece quando o teto passa). E há um teste com dois caches contra
um orçamento pequeno, `the_shared_budget_caps_every_cache_together`, que mostra os
dois dividindo o mesmo contador.

**Ressalva de método.** Os números absolutos do cenário 4 dependem do cache de
miniaturas do Windows: depois da primeira rodada o sistema tem as miniaturas na
mão, a grade enche mais rápido e a memória sobe (a tabela do O0, medida com o cache
frio, dá 499,7/454,0 MB). Por isso as comparações de O3 foram feitas em sequência, no
mesmo dia, alternando as duas builds.

#### O4: ícones por caminho entram no orçamento

Os ícones que não são de classe (pastas, `.exe`, `.lnk`, `.url`, e as extensões cujo
ícone difere por arquivo) passam a contar no mesmo orçamento de 512 MB e a ser
evictados como as miniaturas, pela mesma ordem (fora da tela há mais tempo primeiro).
Os ícones de classe — os que o `Service::class_icons` guarda por extensão e tamanho —
não contam e não são soltos: a textura é a mesma para todas as linhas daquele tipo.
Para isso o `Answer`/`Slot` passaram a dizer se a imagem é a compartilhada, e a
evicção só chama `drop_image` no que é daquele caminho. Dois testes cobrem: um ícone
próprio soma e um de classe não, e a evicção nunca toca no de classe.

| Cenário 6 (rolagem de 90 s) | Antes | Depois |
|---|---:|---:|
| variação de memória na rolagem (≈8.600 pastas) | +36,4 MB | +33,9 … +40,0 MB |
| por 1.000 pastas | +4,2 MB | +3,9 … +4,7 MB |

A inclinação não muda, e o motivo está na própria medição: uma pasta na lista usa um
ícone de 16 px (1 KB), 8.600 delas dariam ~9 MB, dentro do ruído; e a rolagem rápida
cancela os pedidos que saíram da tela antes de chegarem, então a maior parte nunca
chega a ser guardada. Os ~36 MB que sobem na janela de 90 s não são do cache de
ícones: são da listagem em si (as linhas que vão sendo materializadas), que é o
assunto de O7. O O4 garante o teto — um diretório de 100.000 subpastas rolado até o
fim não soma 400 MB de ícones como o plano §1.3 temia —, mas o ganho nesta janela de
medição é abaixo do que o instrumento resolve.

#### O5a: os dois ícones que faltavam

`menu.svg` (o botão de recolher da barra vertical de abas) e `globe.svg` (o ícone
provisório de um item de URL) não estavam no pacote de assets. Um asset ausente não
é erro de compilação: o elemento SVG registra um ERROR a cada desenho, e foi assim
que um dia rendeu 2.531.269 linhas e 549 MB. Os dois vieram do Lucide, como o resto
do pacote e sob a mesma `LUCIDE-LICENSE.txt`, e um teste novo
(`crates/nohrs-ui/tests/icons.rs`) varre `crates/` por `IconName::X` e por literais
`"icons/*.svg"`, converte cada variante para o nome do arquivo como o
gpui-component faz (`ALargeSmall` → `a-large-small`, `Redo2` → `redo-2`, com
`GitHub` → `github.svg` de exceção) e falha listando os que não estão no pacote. O
teste foi conferido ao contrário: tirando `globe.svg` do diretório ele falha com
`icons/globe.svg (IconName::Globe at crates\nohrs-pages\src\quick_launch\view.rs)`.

#### O5b: um teto para o log

`[diagnostics] log_max_file_mb` (padrão 50) e `log_max_total_mb` (padrão 200) são
aplicados ao sink de arquivo: passado o teto de um dia, os registros daquele dia são
descartados e um aviso único sai no stderr (não pelo `tracing`, que voltaria a
escrever no mesmo writer); na abertura, os arquivos mais antigos são apagados até o
diretório caber no total. O contador reinicia quando a data muda, porque é aí que o
appender abre o arquivo do dia seguinte. Com 50 MB, o dia 15/11 (549 MB) teria parado
em 50 MB; e a pasta (752 MB hoje) volta a 200 MB na próxima abertura. Testes: o
writer para no teto e reclama uma vez, um log sem teto continua escrevendo, e a
limpeza apaga do mais antigo para o mais novo, sem tocar em arquivo que não é do
appender. O schema, o template e `docs/config.md` têm as duas chaves; `docs/logging.md`
diz o que elas fazem.

---

#### 第 1 轮复查（R1–R4、O11、UI 补审）

**R1：后台标签页只留最后一帧。** `Thumbnails::trim_to_last_frame` 释放最后一帧没画过的
全部（计入预算的连纹理一起 `drop_image`，还在等待的请求取消）。`PaneItem` 新增
`trim_background`，`PaneGroup` 在两种情况下调用：`set_active_tab` 选走另一个标签页时被
切走的那个，以及 `add_tab` 新标签页盖住的那个。

场景 `tabs_switch`（两个标签页各滚动一遍相册，然后切回第一个；3 次，`runtime_log` 打开）：

| 次 | 切回时释放 | 释放后共享总量 | 进程峰值 / 稳定私有 |
|---|---:|---:|---:|
| 1 | 151.891.968 B | 2.359.296 B | 476,4 / 451,2 MB |
| 2 | 146.386.944 B | 2.359.296 B | 459,9 / 439,8 MB |
| 3 | 151.891.968 B | 2.359.296 B | 479,8 / 454,6 MB |

切回第一个标签页时，离开的那个把 **~150 MB** 交还共享预算，自己只留一屏（2,4 MB）；
没有 R1 时这 150 MB 会一直占着。日志里每次运行有 3 行 trim（新标签页盖住旧的 2 次 +
切回 1 次），第一次释放 0，因为第一个标签页那时还没有任何就绪的缩略图（都在生成中，
被取消）。

**R2：关闭标签页前释放纹理。** `Thumbnails::release_all` 把计入预算的纹理全部
`drop_image` 并清零账本；`PaneItem::release_holdings` 在 `close_tab` 和 `remove_pane`
（关整个窗格、关最后一个标签页）里、entity 被丢弃之前调用。测试：缓存里有缩略图、自有
图标、类共享图标各一，释放后 entries 清空、`ready_bytes` 为 0，类共享图标不计入。

**R3：关掉 runtime_log 也清理旧日志。** 清理移到 `init_logging_with_file` 开头，无论
`enabled` 与否都跑；目录不存在就跳过（不会为了清理而创建）。测试：`enabled: false` 时
旧文件照样被删；目录不存在时什么都不创建。

**R4：上限按文件算。** `CappedWriter` 用当天日志文件的现有大小作为起始计数，一天内重启
N 次不会再写出 N × 50 MB。测试：已有 8/10 字节时只再接受 2 字节；文件大小按当天文件名
从磁盘读，没有文件时为 0。

**O11：迁移不再复制缓存和日志。** `prepare_storage` 只复制配置文件、`data` 和 `state`
（跳过 `logs`）；`migrate_logs` 变成 `migrate_panic_log`，只迁移 `panic.log`。三个测试：
state 复制跳过 logs、只迁移 panic.log、两个方向都不把 cache 列为目标。

**重跑的三个场景（同一台机，3 次中位数）：**

| 场景 | 上一轮 | 本轮 |
|---|---:|---:|
| 2b. 加载并重建 | 1.031,2 / 579,8 MB | **991,6 / 535,9 MB** |
| 4. 10.000 张照片 | 1.389,4 / 1.183,8 MB | **482,1 / 438,5 MB** |
| 5. 两个标签页各滚动 | 1.027,3 / 943,9 MB | **400,7 / 382,1 MB**（原始 396,4–407,6 / 379,4–388,4） |

场景 4 和 5 比上一轮低了约 2,6 倍，而这轮改的 R1、R2、O11 都不在"单标签页滚动"的路径
上。截图显示这一轮网格滚到了最大（250 px 格），相册在 60 秒里就绪的缩略图远少于上一轮
（上一轮那 1,2 GB 是缓存被塞满到 512 MB 上限的样子），而且 Windows 缩略图缓存的热度在
整机文件缓存被大索引遍历冲掉之后也会掉下来。结论：这两个场景的绝对值跨会话不可比，
本轮没有把它当作 R1/R2 的效果；要让它可比，下一轮应把网格缩放固定下来（例如由 harness
断言或由配置指定），而不是靠 15 次合成 `Ctrl+=`。

