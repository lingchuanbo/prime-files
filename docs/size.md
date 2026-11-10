# O tamanho do executável

Medido neste repositório, `cargo build -p nohrs`, Windows x64, `nohrs.exe`:

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

Nada disso foi aplicado além dos perfis como estão: a build `release` existe
para recompilar rápido depois de uma edição (`incremental`, 256 codegen units),
e as opções acima custam tempo de build (`dist` leva ~8 minutos) ou velocidade
em execução (`opt-level = "s"`), que é justamente o motivo de o projeto usar
`release` e não `debug` (ver o comentário em `Cargo.toml`).

Se o tamanho voltar a importar, nesta ordem, do mais barato ao mais caro:

1. `opt-level = "s"` no `[profile.release]`: 39 → 29.6 MB, sem tocar no tempo
   de recompilação (a granularidade dos codegen units é a mesma);
2. `lto = "fat"` + `codegen-units = 1` + `strip = "symbols"` no
   `[profile.dist]`: o que o instalador entrega, 39 → 23.5 MB, ao custo de uma
   build completa mais lenta;
3. `panic = "abort"`: 1–3 MB. O código só instala um `panic::set_hook` para
   registrar o erro (`nohrs-core/src/telemetry/logging.rs`); não há
   `catch_unwind`, então isso só muda o que acontece depois do registro — o
   processo termina em vez de tentar continuar;
4. features opcionais, que tiram funcionalidade: o índice de busca (`tantivy`),
   os visualizadores de modelos 3D (glTF/FBX/OBJ/STL) e o PDF.
