# O tamanho do executável

Medido neste repositório, `cargo build -p nohrs`, Windows x64, `LumaFiles.exe`:

| Perfil / opções | Tamanho |
|---|---|
| `release` (o que `iniciar-nohrs.bat` compila) | 39.0 MB |
| `dist` com `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"` | 31.8 MB |
| idem, com `opt-level = 2` | 30.9 MB |
| idem, com `opt-level = "s"` | **23.5 MB** |
| `release` com `opt-level = "s"` (mantendo incremental e 256 CGUs) | 29.6 MB |

O que move a agulha é o `opt-level = "s"`: o otimizador deixa de inlinar cada
função genérica em cada chamador, e é isso que produz a maior parte do código.
`strip = "symbols"` não muda nada aqui — a build MSVC de release já não leva
tabela de símbolos (medido: 0 bytes de diferença). `lto = "fat"` com um único
codegen unit remove o que a build multi-unidade duplica: cerca de 7 MB.

## Por que ficou de fora, por enquanto

A build `release` continua como estava: ela existe para recompilar rápido depois
de uma edição (`incremental`, 256 codegen units). O `[profile.dist]`, que é o
que o instalador entrega, passou a usar `lto = "fat"` e `codegen-units = 1` em
22/11/2026 (ver a medição abaixo); `opt-level = "s"` e `panic = "abort"`
continuam de fora — o primeiro custa velocidade em execução, que é justamente o
motivo de o projeto usar `release` e não `debug`, e o segundo impediria o
`catch_unwind` que os testes usam.

## Medição de 22/11/2026: `dist` com LTO

Depois que o código cresceu (selos de tipo, status de VCS, colunas coloridas),
`cargo build -p nohrs` no perfil `release` passou a dar **43,2 MB**. Com
`[profile.dist]` em `lto = "fat"`, `codegen-units = 1` e
`debug = "line-tables-only"`:

| Perfil | Tamanho | Build |
|---|---:|---:|
| `release` | 43,2 MB | segundos, incremental |
| `dist` com LTO | **35,7 MB** (-17%) | ~10 minutos |

`debug = "line-tables-only"` não entra no `.exe` nesta cadeia MSVC (as linhas
vão para o PDB), então a tabela acima é só o efeito do LTO. É o preço certo
para quem instala: o `iniciar-nohrs.bat` continua no `release` e ninguém espera
10 minutos a cada edição.

Se o tamanho voltar a importar, nesta ordem, do mais barato ao mais caro:

1. `opt-level = "s"` no `[profile.release]`: 39 → 29.6 MB, sem tocar no tempo
   de recompilação (a granularidade dos codegen units é a mesma);
2. `lto = "fat"` + `codegen-units = 1` no `[profile.dist]` (já aplicado);
   somar `opt-level = "s"` levaria o `dist` a ~23.5 MB, ao custo de runtime;
3. `panic = "abort"`: 1–3 MB. O código instala um `panic::set_hook` para
   registrar o erro (`nohrs-core/src/telemetry/logging.rs`); não há
   `catch_unwind` em produção, então isso só muda o que acontece depois do
   registro — o processo termina em vez de tentar continuar;
4. features opcionais, que tiram funcionalidade: o índice de busca (`tantivy`),
   os visualizadores de modelos 3D (glTF/FBX/OBJ/STL) e o PDF.
