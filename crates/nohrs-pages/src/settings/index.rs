//! The search index section: which drives the whole-computer search answers
//! from, which names it leaves out, and the index itself.
//!
//! The layout follows Everything's options dialog — a tab strip over a list
//! of drives or rules — but the settings apply as they are changed: they are
//! written to `search.toml` and the search filters its hits with them, so
//! nothing here waits for an Apply button and nothing is reindexed.

use gpui::{
    AnyElement, Context, Div, ElementId, Entity, SharedString, Subscription, Window, div,
    prelude::*, px, rgb,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, Sizable as _};
use nohrs_core::config::IndexSource;
use nohrs_core::telemetry::LogErr;
use nohrs_core::{t, tf};
use nohrs_ui::theme::theme;

use super::{Group, Pick, Row, SettingsPage, ago, button};
use crate::explorer::everything::{self, Source};

/// The tabs of the section, in the order the strip shows them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Tab {
    /// Which fixed drives are searched.
    #[default]
    Volumes,
    /// What the search leaves out.
    Exclude,
    /// The index itself: its source, its state and the instant indexing.
    Advanced,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Volumes, Self::Exclude, Self::Advanced];

    fn label(self) -> &'static str {
        match self {
            Self::Volumes => t!("Unidades"),
            Self::Exclude => t!("Exclusões"),
            Self::Advanced => t!("Avançado"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Volumes => "icons/aura-hard-drive.svg",
            Self::Exclude => "icons/funnel.svg",
            Self::Advanced => "icons/settings.svg",
        }
    }
}

/// The section's own state: which tab is open and the fields being edited.
#[derive(Default)]
pub(crate) struct Section {
    pub(crate) tab: Tab,
    /// The pattern typed into "Adicionar filtro…", made on the first render.
    pub(crate) pattern: Option<Entity<InputState>>,
    /// File masks kept, semicolon-separated.
    pub(crate) include_only: Option<Entity<InputState>>,
    /// File masks left out, semicolon-separated.
    pub(crate) masks: Option<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

/// The fields need a window, so they are made on the first render of the
/// section and kept while they are being edited.
pub(crate) fn ensure_inputs(
    page: &mut SettingsPage,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) {
    if page.index.pattern.is_some()
        && page.index.include_only.is_some()
        && page.index.masks.is_some()
    {
        return;
    }
    let pattern = cx.new(|cx| InputState::new(window, cx));
    pattern.update(cx, |state, cx| {
        state.set_placeholder(t!("Padrão para excluir, como *.tmp"), window, cx);
    });
    let include_only = cx.new(|cx| InputState::new(window, cx));
    include_only.update(cx, |state, cx| {
        state.set_placeholder(t!("*.pdf; *.docx"), window, cx);
        state.set_value(
            page.config.indexing.exclude.include_only.join("; "),
            window,
            cx,
        );
    });
    let masks = cx.new(|cx| InputState::new(window, cx));
    masks.update(cx, |state, cx| {
        state.set_placeholder(t!("*.tmp; *~"), window, cx);
        state.set_value(page.config.indexing.exclude.masks.join("; "), window, cx);
    });
    let mut subscriptions = Vec::new();
    subscriptions.push(cx.subscribe_in(
        &include_only,
        window,
        |page, input, event: &InputEvent, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let masks = patterns_of(input.read(cx).value().as_ref());
                if masks != page.config.indexing.exclude.include_only {
                    page.write("indexing.exclude", "include_only", toml_array(&masks), cx);
                }
            }
        },
    ));
    subscriptions.push(cx.subscribe_in(
        &masks,
        window,
        |page, input, event: &InputEvent, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let patterns = patterns_of(input.read(cx).value().as_ref());
                if patterns != page.config.indexing.exclude.masks {
                    page.write("indexing.exclude", "masks", toml_array(&patterns), cx);
                }
            }
        },
    ));
    subscriptions.push(cx.subscribe_in(
        &pattern,
        window,
        |page, input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                add_filter(page, input.clone(), window, cx);
            }
        },
    ));
    page.index = Section {
        tab: page.index.tab,
        pattern: Some(pattern),
        include_only: Some(include_only),
        masks: Some(masks),
        _subscriptions: subscriptions,
    };
}

impl Section {
    /// The rows of the open tab.
    pub(crate) fn groups(&self, page: &SettingsPage, cx: &mut Context<SettingsPage>) -> Vec<Group> {
        match self.tab {
            Tab::Volumes => self.volumes(page, cx),
            Tab::Exclude => self.exclude(page, cx),
            Tab::Advanced => self.advanced(page, cx),
        }
    }

    /// The tab strip drawn above the section's rows.
    pub(crate) fn tabs(&self, cx: &mut Context<SettingsPage>) -> Div {
        div()
            .flex()
            .gap_1()
            .pb(px(2.0))
            .children(Tab::ALL.map(|tab| {
                let active = self.tab == tab;
                div()
                    .id(ElementId::Name(SharedString::from(format!(
                        "index-tab-{tab:?}"
                    ))))
                    .h(px(32.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .text_sm()
                    .map(|this| {
                        if active {
                            this.bg(rgb(theme::accent_subtle()))
                                .text_color(rgb(theme::accent()))
                        } else {
                            this.text_color(rgb(theme::fg_secondary())).hover(|style| {
                                style
                                    .bg(rgb(theme::bg_hover()))
                                    .text_color(rgb(theme::fg()))
                            })
                        }
                    })
                    .on_click(cx.listener(move |page, _event, _window, cx| {
                        page.index.tab = tab;
                        cx.notify();
                    }))
                    .child(Icon::new(Icon::empty()).path(tab.icon()).size_4())
                    .child(tab.label())
            }))
    }

    // -----------------------------------------------------------------
    // Unidades

    fn volumes(&self, page: &SettingsPage, cx: &mut Context<SettingsPage>) -> Vec<Group> {
        let indexing = &page.config.indexing;
        let drives = everything::volumes(cx);
        let all: Vec<String> = drives
            .iter()
            .map(|volume| normal_volume(&volume.root))
            .collect();
        let selected = |root: &str| {
            indexing.volumes.is_empty()
                || indexing
                    .volumes
                    .iter()
                    .any(|chosen| normal_volume(chosen) == root)
        };
        let mut rows = vec![Row::hint(
            t!("Unidades fixas"),
            t!("A busca em todo o computador lê os nomes destas unidades. Uma unidade desmarcada sai dos resultados; o índice dela continua no disco."),
            div()
                .max_w(px(280.0))
                .text_xs()
                .text_right()
                .text_color(rgb(theme::fg_secondary()))
                .child(self.index_line(cx)),
        )
        .wide()];
        if drives.is_empty() {
            rows.push(Row::new(
                t!("Nenhuma unidade fixa encontrada."),
                None,
                div(),
            ));
        }
        for volume in &drives {
            let root = volume.root.clone();
            let id = normal_volume(&root);
            let on = selected(&id);
            let mut chosen: Vec<String> = if indexing.volumes.is_empty() {
                all.clone()
            } else {
                indexing
                    .volumes
                    .iter()
                    .map(|volume| normal_volume(volume))
                    .collect()
            };
            if on {
                chosen.retain(|entry| entry != &id);
                // Empty means "every drive": leaving the list empty would
                // turn the last unchecked drive back on.
                if chosen.is_empty() {
                    chosen.extend(all.iter().cloned());
                }
            } else if !chosen.iter().any(|entry| entry == &id) {
                chosen.push(id.clone());
            }
            let value = if chosen.len() == all.len() {
                "[]".to_string()
            } else {
                toml_array(&chosen)
            };
            let entries = match volume.entries {
                Some(entries) => tf!("{n} nomes neste índice", n = entries),
                None => t!("Índice do serviço de indexação").to_string(),
            };
            rows.push(Row::hint(
                SharedString::from(root),
                entries,
                page.switch_pick(
                    ElementId::Name(SharedString::from(format!("index-volume-{id}"))),
                    on,
                    Pick::Config("indexing", "volumes", SharedString::from(value)),
                    cx,
                ),
            ));
        }
        vec![Group::titled(t!("Unidades"), rows)]
    }

    /// What the index holds right now, as the volumes and advanced tabs show it.
    fn index_line(&self, cx: &mut Context<SettingsPage>) -> String {
        let status = everything::status(cx);
        if status.source == Source::Everything {
            return everything::external_text(status.external.as_ref());
        }
        if status.service {
            return match status.service_index_status.as_ref() {
                Some(Ok(index)) if index.ready && index.refreshing => {
                    tf!("{n} nomes, reconstruindo", n = index.entries)
                }
                Some(Ok(index)) if index.ready => tf!("{n} nomes no índice", n = index.entries),
                Some(Ok(_)) | None => t!("O índice está sendo lido…").to_string(),
                Some(Err(error)) => format!("{} {error}", t!("Sem resposta do índice:")),
            };
        }
        match (status.building, status.entries) {
            (Some(found), _) => tf!("Indexando… {n} itens até agora", n = found),
            (None, Some(entries)) => match status.built.and_then(|built| built.elapsed().ok()) {
                Some(age) => tf!("{n} itens, atualizado {ago}", n = entries, ago = ago(age)),
                None => tf!("{n} itens", n = entries),
            },
            (None, None) => t!("O índice é criado na primeira busca.").to_string(),
        }
    }

    // -----------------------------------------------------------------
    // Exclusões

    fn exclude(&self, page: &SettingsPage, cx: &mut Context<SettingsPage>) -> Vec<Group> {
        let exclude = &page.config.indexing.exclude;
        let attributes = vec![
            Row::hint(
                t!("Arquivos e pastas ocultos"),
                t!("Deixa de fora o que o Windows marca como oculto, com tudo o que houver dentro das pastas ocultas."),
                page.switch(
                    "index-exclude-hidden",
                    exclude.hidden,
                    ("indexing.exclude", "hidden"),
                    cx,
                ),
            )
            .reset(("indexing.exclude", "hidden"), !exclude.hidden, "false"),
            Row::hint(
                t!("Arquivos e pastas de sistema"),
                t!("Deixa de fora o que o Windows marca como sistema, como as pastas e os arquivos protegidos."),
                page.switch(
                    "index-exclude-system",
                    exclude.system,
                    ("indexing.exclude", "system"),
                    cx,
                ),
            )
            .reset(("indexing.exclude", "system"), !exclude.system, "false"),
            Row::hint(
                t!("Ativar lista de exclusão"),
                t!("Liga as pastas, os filtros e as máscaras abaixo. Os arquivos ocultos e de sistema têm o seu próprio interruptor."),
                page.switch(
                    "index-exclude-enabled",
                    exclude.enabled,
                    ("indexing.exclude", "enabled"),
                    cx,
                ),
            )
            .reset(("indexing.exclude", "enabled"), exclude.enabled, "true"),
        ];

        let mut list = Vec::new();
        if exclude.paths.is_empty() && exclude.globs.is_empty() {
            list.push(Row::new(
                t!("Nenhuma pasta ou filtro na lista."),
                None,
                div(),
            ));
        }
        for (ix, path) in exclude.paths.iter().enumerate() {
            let mut remaining = exclude.paths.clone();
            if ix < remaining.len() {
                remaining.remove(ix);
            }
            list.push(Row::new(
                SharedString::from(path.clone()),
                None,
                remove_button(
                    ElementId::Name(SharedString::from(format!("index-path-remove-{ix}"))),
                    "paths",
                    toml_array(&remaining),
                    cx,
                ),
            ));
        }
        for (ix, pattern) in exclude.globs.iter().enumerate() {
            let mut remaining = exclude.globs.clone();
            if ix < remaining.len() {
                remaining.remove(ix);
            }
            list.push(Row::new(
                SharedString::from(pattern.clone()),
                None,
                remove_button(
                    ElementId::Name(SharedString::from(format!("index-glob-remove-{ix}"))),
                    "globs",
                    toml_array(&remaining),
                    cx,
                ),
            ));
        }
        list.push(Row::new(
            t!("Adicionar pasta…"),
            None,
            button("index-add-folder", t!("Adicionar pasta…"), false)
                .on_click(cx.listener(|page, _event, window, cx| add_folder(page, window, cx))),
        ));
        list.push({
            let mut control = div().flex().items_center().gap_2();
            if let Some(input) = self.pattern.clone() {
                control = control
                    .child(div().w(px(260.0)).child(Input::new(&input)))
                    .child(
                        button("index-add-filter", t!("Adicionar filtro…"), false).on_click(
                            cx.listener(|page, _event, window, cx| {
                                if let Some(input) = page.index.pattern.clone() {
                                    add_filter(page, input, window, cx);
                                }
                            }),
                        ),
                    );
            }
            Row::hint(
                t!("Adicionar filtro…"),
                t!("Um nome de arquivo ou de pasta; a pasta sai com tudo o que há dentro. Vários separados por ponto e vírgula, como .git; .svn; temp; *.tmp."),
                control,
            )
            .wide()
        });

        let masks = Group::titled(
            t!("Máscaras de arquivo"),
            vec![
                Row::hint(
                    t!("Incluir somente estes arquivos"),
                    t!("Separe com ponto e vírgula, como *.docx; *.pdf. Vazio inclui todos os arquivos. Aperte Enter para salvar."),
                    mask_input(self.include_only.clone(), t!("*.pdf; *.docx")),
                )
                .wide(),
                Row::hint(
                    t!("Excluir estes arquivos"),
                    t!("Separe com ponto e vírgula, como *.tmp; *~. Aperte Enter para salvar."),
                    mask_input(self.masks.clone(), t!("*.tmp; *~")),
                )
                .wide(),
            ],
        );

        vec![
            Group::new(attributes),
            Group::titled(t!("Pastas e filtros"), list),
            masks,
        ]
    }

    // -----------------------------------------------------------------
    // Avançado

    fn advanced(&self, page: &SettingsPage, cx: &mut Context<SettingsPage>) -> Vec<Group> {
        let status = everything::status(cx);
        let source = status.source;
        let picked = page.config.indexing.source;
        let refresh_feedback = if status.service_refreshing {
            Some(t!("Pedindo para reconstruir o índice…").to_string())
        } else {
            status
                .service_refresh_result
                .as_ref()
                .map(|result| match result {
                    Ok(()) => t!("O serviço de indexação aceitou o pedido.").to_string(),
                    Err(error) => format!(
                        "{} {error}",
                        t!("Não foi possível pedir a reconstrução do índice:")
                    ),
                })
        };
        let building = status.building.is_some();
        let own_index = source != Source::Everything;
        let mut rows = vec![
            Row::hint(
                t!("Estado do índice"),
                t!(
                    "O que a busca em todo o computador tem agora. Mudar as listas acima não refaz o índice: os resultados são filtrados na hora."
                ),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(
                        div()
                            .max_w(px(280.0))
                            .text_xs()
                            .text_right()
                            .text_color(rgb(theme::fg_secondary()))
                            .child(self.index_line(cx)),
                    )
                    .when_some(refresh_feedback, |this, feedback| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::fg_secondary()))
                                .child(feedback),
                        )
                    })
                    .when(own_index && !building, |this| {
                        this.child(
                            button(
                                "everything-rebuild",
                                if status.service_refreshing {
                                    t!("Pedindo para reconstruir o índice…")
                                } else {
                                    t!("Reindexar agora")
                                },
                                status.service_refreshing,
                            )
                            .on_click(cx.listener(
                                |_page, _event, _window, cx| {
                                    crate::explorer::everything::rebuild(cx)
                                },
                            )),
                        )
                    }),
            ),
            Row::hint(
                t!("Índice do computador"),
                t!(
                    "De onde vêm os nomes da busca em todo o computador. Com o Everything aberto, o índice dele serve e nada é criado aqui; o automático decide isso na primeira busca."
                ),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(page.config_choice(
                        "index-source",
                        &[
                            (t!("Automático"), picked == IndexSource::Auto, "\"auto\""),
                            (
                                t!("Neste programa"),
                                picked == IndexSource::PrimeFiles,
                                "\"prime-files\"",
                            ),
                            (
                                "Everything",
                                picked == IndexSource::Everything,
                                "\"everything\"",
                            ),
                        ],
                        ("indexing", "source"),
                        cx,
                    ))
                    .when(
                        picked == IndexSource::Auto && source == Source::Own,
                        |this| {
                            this.child(
                                div()
                                    .max_w(px(280.0))
                                    .text_xs()
                                    .text_right()
                                    .text_color(rgb(theme::fg_secondary()))
                                    .child(everything::auto_note(status.external.as_ref())),
                            )
                        },
                    ),
            ),
        ];
        if own_index {
            use crate::explorer::everything::mft;
            let mft_state = mft::state();
            let (mft_busy, mft_error) = cx
                .try_global::<crate::explorer::everything::Everything>()
                .map_or((false, None), |state| {
                    (state.mft_busy, state.mft_error.clone())
                });
            let mft_text = match (mft_busy, mft_error, mft_state) {
                (true, _, _) => t!("Aguardando a permissão do Windows…").to_string(),
                (false, Some(error), _) => tf!("Não deu certo: {error}", error = error),
                (false, None, mft::State::On) => t!("Ligada").to_string(),
                (false, None, mft::State::Stopped) => {
                    t!("O serviço está instalado mas parado.").to_string()
                }
                (false, None, mft::State::Off) => t!("Desligada").to_string(),
            };
            let mft_on = mft_state == mft::State::On;
            rows.push(Row::hint(
                t!("Indexação instantânea"),
                t!("Lê a tabela dos discos NTFS como o Everything: o índice do computador fica pronto em segundos, também depois de reiniciar. Instala um serviço do Windows, com permissão de administrador uma vez."),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(
                        div()
                            .max_w(px(280.0))
                            .text_xs()
                            .text_right()
                            .text_color(rgb(theme::fg_secondary()))
                            .child(mft_text),
                    )
                    .when(!mft_busy, |this| {
                        this.child(
                            button(
                                "everything-mft",
                                if mft_on { t!("Desativar") } else { t!("Ativar") },
                                false,
                            )
                            .on_click(cx.listener(move |_page, _event, _window, cx| {
                                mft::set(!mft_on, cx)
                            })),
                        )
                    }),
            ));
        }
        vec![
            Group::titled(t!("Índice"), rows),
            Group::new(vec![Row::hint(
                t!("Como o índice é feito"),
                t!(
                    "Os nomes das unidades ficam guardados neste computador. Com o programa aberto, o que é criado ou renomeado entra na hora; com o serviço, ele guarda os nomes e os relê a cada 30 minutos, e sem o serviço o índice deste processo é refeito quando passa de 6 horas. Nada sai do computador."
                ),
                div(),
            )]),
        ]
    }
}

/// The masks field under its description.
fn mask_input(input: Option<Entity<InputState>>, placeholder: &'static str) -> AnyElement {
    match input {
        Some(input) => div()
            .w_full()
            .child(Input::new(&input).small())
            .into_any_element(),
        None => div()
            .text_xs()
            .text_color(rgb(theme::fg_secondary()))
            .child(placeholder)
            .into_any_element(),
    }
}

/// "C:", "c:\" and "C:/" are the same drive.
fn normal_volume(root: &str) -> String {
    root.trim().trim_end_matches(['\\', '/']).to_uppercase()
}

/// The masks of a `;`-separated field, in the order typed, without blanks.
fn patterns_of(text: &str) -> Vec<String> {
    text.split([';', ','])
        .map(str::trim)
        .filter(|mask| !mask.is_empty())
        .map(str::to_string)
        .collect()
}

/// A TOML inline array for `[indexing]` / `[indexing.exclude]` writes.
fn toml_array(entries: &[String]) -> String {
    let values = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap_or_else(|_| "\"\"".to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

/// A small "Remover" button that writes `value` to `[indexing.exclude] key`.
fn remove_button(
    id: ElementId,
    key: &'static str,
    value: String,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    button(id, t!("Remover"), true)
        .on_click(cx.listener(move |page, _event, _window, cx| {
            page.write("indexing.exclude", key, value.clone(), cx);
        }))
        .into_any_element()
}

/// Asks for a folder and appends it to `[indexing.exclude] paths`.
fn add_folder(_page: &mut SettingsPage, window: &mut Window, cx: &mut Context<SettingsPage>) {
    let prompt = crate::quick_launch::path_prompt_for(
        cx,
        crate::quick_launch::PathWant::Folders,
        false,
        t!("Escolher uma pasta"),
    );
    let receiver = cx.prompt_for_paths(prompt);
    cx.spawn_in(window, async move |this, cx| {
        let Ok(Ok(Some(picked))) = receiver.await else {
            return;
        };
        let Some(path) = picked.first() else {
            return;
        };
        let entry = path.to_string_lossy().to_string();
        this.update_in(cx, |page, _window, cx| {
            let mut paths = page.config.indexing.exclude.paths.clone();
            if paths
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&entry))
            {
                return;
            }
            paths.push(entry);
            page.write("indexing.exclude", "paths", toml_array(&paths), cx);
        })
        .log_err();
    })
    .detach();
}

/// Adds the pattern typed into "Adicionar filtro…" to `[indexing.exclude] globs`.
fn add_filter(
    page: &mut SettingsPage,
    input: Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) {
    let typed = patterns_of(input.read(cx).value().as_ref());
    if typed.is_empty() {
        return;
    }
    let mut globs = page.config.indexing.exclude.globs.clone();
    let mut added = false;
    for pattern in typed {
        if !globs
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&pattern))
        {
            globs.push(pattern);
            added = true;
        }
    }
    if added {
        page.write("indexing.exclude", "globs", toml_array(&globs), cx);
    }
    input.update(cx, |state, cx| state.set_value("", window, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_names_compare_without_case_or_trailing_slash() {
        assert_eq!(normal_volume("c:"), "C:");
        assert_eq!(normal_volume("C:\\"), "C:");
        assert_eq!(normal_volume(" d:/ "), "D:");
    }

    #[test]
    fn masks_split_on_semicolons_and_commas_without_blanks() {
        assert_eq!(
            patterns_of(" *.pdf ; *.docx ,, "),
            vec!["*.pdf".to_string(), "*.docx".to_string()]
        );
        assert!(patterns_of("   ").is_empty());
    }

    #[test]
    fn arrays_are_written_as_toml_inline_arrays() {
        assert_eq!(toml_array(&[]), "[]");
        assert_eq!(
            toml_array(&["a".to_string(), "b\"c".to_string()]),
            "[\"a\", \"b\\\"c\"]"
        );
    }
}
