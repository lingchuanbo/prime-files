//! The welcome screen: the first thing Prime Files shows, and what Settings ›
//! About opens again. Five short steps — language, theme and accent; how the
//! files look and sound (hidden items, compact mode, sounds under the mouse,
//! waveforms); the default file manager, starting with Windows and updates;
//! the other programs it takes over ("show in folder", Listary) and instant
//! indexing; and a summary of all of it.
//!
//! Like the settings page it never applies anything itself: choices go to
//! `config.toml` or the registry, and the root shows them back. Finishing (or
//! skipping) writes `ui.welcome_done = true`, which is what takes the screen
//! away; Settings sets it back to false to bring it again.

use gpui::{
    AnyElement, Context, Div, ElementId, IntoElement, Render, SharedString, Stateful, Window, div, prelude::*, px,
    rgb,
};
use gpui_component::Icon;
use nohrs_core::config::{self, Config, ThemeMode};
use nohrs_core::i18n::{self, Lang};
use nohrs_core::telemetry::LogErr;
use nohrs_core::{t, tf};
use nohrs_ui::theme::theme;

use crate::explorer::everything::{self, Everything, Source, mft};

const STEPS: usize = 5;

/// The welcome screen.
pub struct WelcomePage {
    step: usize,
    config: Config,
    // `None` until the registry or Listary's file has answered.
    default_explorer: Option<bool>,
    starts_with_windows: Option<bool>,
    listary: Option<crate::listary::State>,
    // The last change that failed, shown above the buttons.
    error: Option<String>,
    // A change under way (Listary restarts, the registry is written).
    busy: bool,
}

impl WelcomePage {
    /// A welcome screen on its first step, reading what the system has now.
    pub fn new(config: Config, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            step: 0,
            config,
            default_explorer: None,
            starts_with_windows: None,
            listary: None,
            error: None,
            busy: false,
        };
        page.refresh(cx);
        page
    }

    /// Takes the config the window runs on, so the choices show it.
    pub fn set_config(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config;
        cx.notify();
    }

    // Asks the registry and Listary, off the UI thread.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let read = cx.background_spawn(async {
            (
                crate::default_explorer::is_default(),
                crate::default_explorer::starts_with_windows(),
                crate::listary::state(),
            )
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let (is_default, starts, listary) = read.await;
            this.update(cx, |page, cx| {
                page.default_explorer = Some(is_default);
                page.starts_with_windows = Some(starts);
                page.listary = Some(listary);
                page.busy = false;
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn write(&mut self, section: &'static str, key: &'static str, value: String, cx: &mut Context<Self>) {
        let path = config::paths::config_file();
        let write = cx.background_spawn(async move { config::set_value(&path, section, key, &value) });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            if let Err(error) = write.await {
                this.update(cx, |page, cx| {
                    page.error = Some(tf!("Não foi possível salvar {section}.{key}: {error}", section = section, key = key, error = error));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    // Runs a system change (registry, Listary) in the background, then reads
    // everything again so the switches show what really happened.
    fn change(&mut self, run: impl FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.error = Some(tf!("Não foi possível achar o executável do Prime Files: {error}", error = error));
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let task = cx.background_spawn(async move { run(&exe) });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = task.await;
            this.update(cx, |page, cx| {
                if let Err(error) = outcome {
                    page.error = Some(error.to_string());
                }
                page.refresh(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        self.write("ui", "welcome_done", "true".into(), cx);
    }

    // -----------------------------------------------------------------------
    // Steps

    fn step_welcome(&self, cx: &mut Context<Self>) -> Div {
        let chosen = Lang::parse(&self.config.ui.language);
        let mut languages: Vec<(SharedString, bool, String)> =
            vec![(t!("Automático").into(), chosen.is_none(), "\"auto\"".to_string())];
        for lang in Lang::ALL {
            languages.push((lang.native_name().into(), chosen == Some(lang), format!("\"{}\"", lang.code())));
        }
        let mode = self.config.theme.mode;
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(hero("icons/folder-open.svg", t!("Bem-vindo ao Prime Files"), t!("Um explorador de arquivos rápido para o Windows. Vamos deixá-lo do seu jeito em menos de um minuto.")))
            .child(
                card()
                    .child(card_title(t!("Idioma")))
                    .child(card_hint(tf!(
                        "Automático segue o formato regional do Windows; neste computador, {language}.",
                        language = i18n::detected().native_name()
                    )))
                    .child(
                        div().flex().flex_wrap().gap(px(6.0)).children(languages.into_iter().enumerate().map(
                            |(ix, (label, selected, value))| {
                                pill(ElementId::NamedInteger("welcome-lang".into(), ix as u64), label, selected)
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.write("ui", "language", value.clone(), cx);
                                    }))
                            },
                        )),
                    ),
            )
            .child(
                card().child(card_title(t!("Tema"))).child(
                    div()
                        .flex()
                        .gap(px(10.0))
                        .child(theme_tile("welcome-dark", t!("Escuro"), mode == ThemeMode::Dark, (0x1E1F22, 0x2A2C30, 0xDCDDE0), cx, "\"dark\""))
                        .child(theme_tile("welcome-light", t!("Claro"), mode == ThemeMode::Light, (0xFFFFFF, 0xF3F4F6, 0x1F2937), cx, "\"light\""))
                        .child(theme_tile("welcome-system", t!("Igual ao Windows"), mode == ThemeMode::System, (0x1E1F22, 0xF3F4F6, 0x9A9CA3), cx, "\"system\"")),
                ),
            )
            .child(card().child(card_title(t!("Cor de destaque"))).child(self.accents(cx)))
    }

    // The named accents as swatches, as in Settings › Appearance.
    fn accents(&self, cx: &mut Context<Self>) -> Div {
        let accent = self.config.theme.accent.trim().to_string();
        div().flex().flex_wrap().gap(px(6.0)).children(
            theme::NAMED_ACCENTS.iter().zip(crate::settings::ACCENT_LABELS).enumerate().map(
                |(index, (&(name, color), &label))| {
                    let selected = accent.eq_ignore_ascii_case(name);
                    div()
                        .id(ElementId::NamedInteger("welcome-accent".into(), index as u64))
                        .w(px(72.0))
                        .py(px(8.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme::bg_hover())))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.write("theme", "accent", format!("\"{name}\""), cx);
                        }))
                        .child(
                            div()
                                .size(px(28.0))
                                .rounded_full()
                                .bg(rgb(color.unwrap_or_else(theme::palette_accent)))
                                .border_2()
                                .border_color(rgb(if selected { theme::fg() } else { theme::bg_secondary() })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(if selected { theme::fg() } else { theme::fg_secondary() }))
                                .child(i18n::tr(label)),
                        )
                },
            ),
        )
    }

    // A switch of `config.toml`: flips `[section] key` from `on`.
    fn config_card(
        &self,
        id: &'static str,
        icon: &'static str,
        title: &'static str,
        text: &'static str,
        on: bool,
        (section, key): (&'static str, &'static str),
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        option_card(id, icon, title, text, Some(on), false, cx.listener(move |page, _, _, cx| {
            page.write(section, key, (!on).to_string(), cx);
        }))
    }

    fn step_view(&self, cx: &mut Context<Self>) -> Div {
        let ui = &self.config.ui;
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(hero("icons/layout-dashboard.svg", t!("Como você vê os arquivos"), t!("Ajustes do dia a dia. Tudo fica em Configurações para mudar depois.")))
            .child(self.config_card(
                "welcome-hidden",
                "icons/eye.svg",
                t!("Mostrar itens ocultos"),
                t!("Arquivos e pastas marcados como ocultos."),
                ui.show_hidden,
                ("ui", "show_hidden"),
                cx,
            ))
            .child(self.config_card(
                "welcome-compact",
                "icons/layout-dashboard.svg",
                t!("Modo compacto"),
                t!("Esconde as barras escolhidas, deixando só a lista; cada uma desliza para fora quando o mouse encosta na borda dela, como no Zen Browser."),
                self.config.compact.enabled,
                ("compact", "enabled"),
                cx,
            ))
            .child(self.config_card(
                "welcome-hover-audio",
                "icons/music.svg",
                t!("Tocar ao passar o mouse"),
                t!("Deixe o mouse parado sobre um arquivo de som na lista para ouvi-lo; tirar o mouse para. Um som por vez, no volume do visualizador."),
                ui.hover_audio,
                ("ui", "hover_audio"),
                cx,
            ))
            .child(self.config_card(
                "welcome-waveforms",
                "icons/gallery-vertical-end.svg",
                t!("Forma de onda nos ícones"),
                t!("No modo Ícones, arquivos de som mostram a forma de onda no lugar do ícone."),
                ui.audio_waveforms,
                ("ui", "audio_waveforms"),
                cx,
            ))
    }

    fn step_default(&self, cx: &mut Context<Self>) -> Div {
        let is_default = self.default_explorer;
        let starts = self.starts_with_windows;
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(hero("icons/app-window.svg", t!("O seu explorador padrão"), t!("Escolha se as pastas do Windows abrem aqui. Dá para desfazer a qualquer momento, e nada muda para os outros usuários do computador.")))
            .child(option_card(
                "welcome-default",
                "icons/folder-open.svg",
                t!("Usar o Prime Files como explorador padrão"),
                t!("Pastas, unidades, Win+E e o ícone do Explorador na barra de tarefas passam a abrir aqui."),
                is_default,
                self.busy,
                cx.listener(move |page, _, _, cx| {
                    let enable = !is_default.unwrap_or(false);
                    page.change(
                        move |exe| {
                            if enable {
                                crate::default_explorer::enable(exe)
                            } else {
                                crate::default_explorer::disable()
                            }
                        },
                        cx,
                    );
                }),
            ))
            .child(option_card(
                "welcome-autostart",
                "icons/clock.svg",
                t!("Iniciar com o Windows"),
                t!("Abre minimizado quando você entra no Windows, para que \"Mostrar na pasta\" de qualquer programa caia aqui na hora."),
                starts,
                self.busy,
                cx.listener(move |page, _, _, cx| {
                    let enable = !starts.unwrap_or(false);
                    page.change(move |exe| crate::default_explorer::set_starts_with_windows(exe, enable), cx);
                }),
            ))
            .child(self.config_card(
                "welcome-updates",
                "icons/refresh-cw.svg",
                t!("Atualizar automaticamente"),
                t!("Procura uma versão nova ao abrir, baixa em segundo plano e instala quando você fecha o Prime Files."),
                self.config.updates.automatic,
                ("updates", "automatic"),
                cx,
            ))
    }

    fn step_integrations(&self, cx: &mut Context<Self>) -> Div {
        let reveal = self.config.explorer.reveal_from_other_apps;
        let mft_on = mft::state() == mft::State::On;
        let (mft_busy, mft_error) =
            cx.try_global::<Everything>().map_or((false, None), |state| (state.mft_busy, state.mft_error.clone()));
        let listary = self.listary;
        // With Everything answering, its index is the computer's and the
        // service this step offers would never be read.
        let own_index = everything::source(cx) != Source::Everything;
        let listary_card: AnyElement = match listary {
            Some(crate::listary::State::NotInstalled) => info_card(
                "icons/search.svg",
                "Listary",
                t!("O Listary não está instalado. Se você instalar depois, ligue em Configurações › Integração com o Windows."),
            )
            .into_any_element(),
            state => {
                let ours = state == Some(crate::listary::State::Ours);
                option_card(
                    "welcome-listary",
                    "icons/search.svg",
                    t!("Abrir as pastas do Listary aqui"),
                    t!("As pastas que você abre pelo Listary (Ctrl duas vezes) abrem no Prime Files. O Listary reinicia ao mudar."),
                    state.map(|_| ours),
                    self.busy,
                    cx.listener(move |page, _, _, cx| {
                        let enable = !ours;
                        page.change(
                            move |exe| {
                                if enable {
                                    crate::listary::enable(exe)
                                } else {
                                    crate::listary::disable()
                                }
                            },
                            cx,
                        );
                    }),
                )
                .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(hero("icons/layers.svg", t!("Os seus outros programas"), t!("O Prime Files atende os programas que pedem para mostrar um arquivo, e destaca o arquivo quando ele chega.")))
            .child(option_card(
                "welcome-reveal",
                "icons/film.svg",
                t!("\"Mostrar na pasta\" abre aqui"),
                t!("Premiere (Revelar no Explorador), Chrome, Edge e Firefox (Mostrar na pasta, depois de baixar) e o menu Iniciar (Abrir local do arquivo) abrem a pasta no Prime Files, com o arquivo selecionado e piscando em destaque."),
                Some(reveal),
                false,
                cx.listener(move |page, _, _, cx| {
                    page.write("explorer", "reveal_from_other_apps", (!reveal).to_string(), cx);
                }),
            ))
            .child(listary_card)
            .when(own_index, |this| {
                this.child(option_card(
                    "welcome-mft",
                    "icons/database.svg",
                    t!("Indexação instantânea"),
                    t!("Lê a tabela dos discos NTFS como o Everything: o índice do computador fica pronto em segundos, também depois de reiniciar. Instala um serviço do Windows, com permissão de administrador uma vez."),
                    Some(mft_on),
                    mft_busy,
                    cx.listener(move |_page, _, _, cx| mft::set(!mft_on, cx)),
                ))
                .when_some(mft_error, |this, error| this.child(note_text(tf!("Não deu certo: {error}", error = error))))
            })
            .when(reveal && self.starts_with_windows == Some(false), |this| {
                this.child(note(t!("Isso vale enquanto o Prime Files está aberto. Para valer sempre, ligue \"Iniciar com o Windows\" no passo anterior.")))
            })
    }

    fn step_done(&self) -> Div {
        let reveal = self.config.explorer.reveal_from_other_apps;
        let listary = match self.listary {
            Some(crate::listary::State::Ours) => Some(true),
            Some(crate::listary::State::NotInstalled) | None => None,
            Some(_) => Some(false),
        };
        let ui = &self.config.ui;
        let rows: Vec<(&'static str, Option<bool>)> = vec![
            (t!("Mostrar itens ocultos"), Some(ui.show_hidden)),
            (t!("Modo compacto"), Some(self.config.compact.enabled)),
            (t!("Tocar ao passar o mouse"), Some(ui.hover_audio)),
            (t!("Forma de onda nos ícones"), Some(ui.audio_waveforms)),
            (t!("Explorador padrão do Windows"), self.default_explorer),
            (t!("Iniciar com o Windows"), self.starts_with_windows),
            (t!("Atualizar automaticamente"), Some(self.config.updates.automatic)),
            (t!("\"Mostrar na pasta\" abre aqui"), Some(reveal)),
            ("Listary", listary),
            (t!("Indexação instantânea"), Some(mft::state() == mft::State::On)),
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(hero("icons/circle-check.svg", t!("Tudo pronto"), t!("Você pode mudar qualquer uma destas escolhas depois, em Configurações.")))
            .child(card().gap(px(4.0)).children(rows.into_iter().map(|(label, state)| {
                let (icon, color, text) = match state {
                    Some(true) => ("icons/circle-check.svg", theme::accent(), t!("Ligado")),
                    Some(false) => ("icons/circle-x.svg", theme::muted(), t!("Desligado")),
                    None => ("icons/minus.svg", theme::muted(), t!("Não instalado")),
                };
                div()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(Icon::new(Icon::empty()).path(icon).size_4().text_color(rgb(color)))
                    .child(div().flex_1().text_sm().text_color(rgb(theme::fg())).child(label))
                    .child(div().text_xs().text_color(rgb(theme::fg_secondary())).child(text))
            })))
            .child(note(t!("Dicas: Espaço mostra o arquivo grande, os botões laterais do mouse voltam e avançam, Ctrl+E busca no computador inteiro, e Configurações › Atalhos muda qualquer tecla.")))
    }
}

impl Render for WelcomePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let step = self.step.min(STEPS - 1);
        let body = match step {
            0 => self.step_welcome(cx),
            1 => self.step_view(cx),
            2 => self.step_default(cx),
            3 => self.step_integrations(cx),
            _ => self.step_done(),
        };
        let last = step == STEPS - 1;
        div()
            .id("welcome")
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::bg()))
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(720.0))
                    .px(px(24.0))
                    .py(px(36.0))
                    .flex()
                    .flex_col()
                    .gap(px(26.0))
                    // Where the person is: one segment per step.
                    .child(
                        div().flex().gap(px(6.0)).children((0..STEPS).map(|index| {
                            div().flex_1().h(px(4.0)).rounded_full().bg(rgb(if index <= step {
                                theme::accent()
                            } else {
                                theme::border_hover()
                            }))
                        })),
                    )
                    .child(body)
                    .when_some(self.error.clone(), |this, error| {
                        this.child(div().text_sm().text_color(rgb(theme::danger())).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .id("welcome-skip")
                                    .text_sm()
                                    .text_color(rgb(theme::fg_secondary()))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(rgb(theme::fg())))
                                    .when(!last, |this| this.child(t!("Pular")))
                                    .on_click(cx.listener(|page, _, _, cx| page.finish(cx))),
                            )
                            .child(div().flex_1())
                            .when(step > 0, |this| {
                                this.child(secondary_button("welcome-back", t!("Voltar")).on_click(cx.listener(
                                    |page, _, _, cx| {
                                        page.step = page.step.saturating_sub(1);
                                        cx.notify();
                                    },
                                )))
                            })
                            .child(
                                primary_button("welcome-next", if last { t!("Começar a usar") } else { t!("Próximo") })
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        if last {
                                            page.finish(cx);
                                        } else {
                                            page.step += 1;
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
    }
}

impl crate::Page for WelcomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        <Self as Render>::render(self, window, cx).into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Pieces

fn hero(icon: &'static str, title: &'static str, text: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .size(px(56.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(16.0))
                .bg(rgb(theme::accent_subtle()))
                .text_color(rgb(theme::accent()))
                .child(Icon::new(Icon::empty()).path(icon).size_6()),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::fg()))
                .child(title),
        )
        .child(div().text_sm().line_height(px(20.0)).text_color(rgb(theme::fg_secondary())).child(text))
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(18.0))
        .rounded(px(12.0))
        .bg(rgb(theme::bg_secondary()))
        .border_1()
        .border_color(rgb(theme::border()))
}

fn card_title(text: &'static str) -> Div {
    div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgb(theme::fg())).child(text)
}

fn card_hint(text: String) -> Div {
    div().text_xs().text_color(rgb(theme::fg_secondary())).child(text)
}

fn note(text: &'static str) -> Div {
    note_text(text.to_string())
}

fn note_text(text: String) -> Div {
    div()
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(rgb(theme::border()))
        .text_xs()
        .line_height(px(18.0))
        .text_color(rgb(theme::fg_secondary()))
        .child(text)
}

fn pill(id: ElementId, label: SharedString, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .text_sm()
        .map(|this| {
            if selected {
                this.bg(rgb(theme::accent())).border_color(rgb(theme::accent())).text_color(rgb(theme::accent_fg()))
            } else {
                this.border_color(rgb(theme::border_hover()))
                    .text_color(rgb(theme::fg()))
                    .hover(|style| style.bg(rgb(theme::bg_hover())))
            }
        })
        .child(label)
}

// A theme as a little window: its background, a bar and a line of text.
fn theme_tile(
    id: &'static str,
    label: &'static str,
    selected: bool,
    (background, bar, text): (u32, u32, u32),
    cx: &mut Context<WelcomePage>,
    value: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .rounded(px(10.0))
        .border_2()
        .cursor_pointer()
        .border_color(rgb(if selected { theme::accent() } else { theme::border() }))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click(cx.listener(move |page, _, _, cx| page.write("theme", "mode", value.to_string(), cx)))
        .child(
            div()
                .h(px(64.0))
                .rounded(px(6.0))
                .bg(rgb(background))
                .border_1()
                .border_color(rgb(theme::border()))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(6.0))
                .child(div().h(px(8.0)).rounded(px(3.0)).bg(rgb(bar)))
                .child(div().w(px(48.0)).h(px(5.0)).rounded(px(3.0)).bg(rgb(text)))
                .child(div().w(px(32.0)).h(px(5.0)).rounded(px(3.0)).bg(rgb(theme::accent()))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_sm()
                .text_color(rgb(theme::fg()))
                .when(selected, |this| {
                    this.child(Icon::new(Icon::empty()).path("icons/check.svg").size_3().text_color(rgb(theme::accent())))
                })
                .child(label),
        )
}

// A choice as a card: icon, name, what it does, and a switch.
fn option_card(
    id: &'static str,
    icon: &'static str,
    title: &'static str,
    text: &'static str,
    on: Option<bool>,
    busy: bool,
    toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let known = on.is_some();
    let on = on.unwrap_or(false);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(14.0))
        .p(px(18.0))
        .rounded(px(12.0))
        .bg(rgb(theme::bg_secondary()))
        .border_1()
        .border_color(rgb(if on { theme::accent() } else { theme::border() }))
        .when(known && !busy, |this| this.cursor_pointer().hover(|style| style.bg(rgb(theme::bg_hover()))).on_click(toggle))
        .child(
            div()
                .size(px(40.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.0))
                .bg(rgb(if on { theme::accent_subtle() } else { theme::bg() }))
                .text_color(rgb(if on { theme::accent() } else { theme::fg_secondary() }))
                .child(Icon::new(Icon::empty()).path(icon).size_5()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgb(theme::fg())).child(title))
                .child(div().text_xs().line_height(px(17.0)).text_color(rgb(theme::fg_secondary())).child(text)),
        )
        .child(if known && !busy {
            switch(on).into_any_element()
        } else {
            div().text_xs().text_color(rgb(theme::muted())).child(t!("Verificando…")).into_any_element()
        })
}

fn info_card(icon: &'static str, title: &'static str, text: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.0))
        .p(px(18.0))
        .rounded(px(12.0))
        .bg(rgb(theme::bg_secondary()))
        .border_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .size(px(40.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.0))
                .bg(rgb(theme::bg()))
                .text_color(rgb(theme::muted()))
                .child(Icon::new(Icon::empty()).path(icon).size_5()),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgb(theme::fg())).child(title))
                .child(div().text_xs().line_height(px(17.0)).text_color(rgb(theme::fg_secondary())).child(text)),
        )
}

fn switch(on: bool) -> Div {
    const WIDTH: f32 = 40.0;
    const HEIGHT: f32 = 22.0;
    const KNOB: f32 = 16.0;
    let inset = (HEIGHT - KNOB) / 2.0 - 1.0;
    div()
        .relative()
        .flex_shrink_0()
        .w(px(WIDTH))
        .h(px(HEIGHT))
        .rounded_full()
        .border_1()
        .map(|track| {
            if on {
                track.bg(rgb(theme::accent())).border_color(rgb(theme::accent()))
            } else {
                track.bg(rgb(theme::bg())).border_color(rgb(theme::border_hover()))
            }
        })
        .child(
            div()
                .absolute()
                .top(px(inset))
                .left(px(if on { WIDTH - KNOB - inset - 2.0 } else { inset }))
                .size(px(KNOB))
                .rounded_full()
                .bg(rgb(if on { theme::accent_fg() } else { theme::fg_secondary() })),
        )
}

fn primary_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(36.0))
        .px(px(20.0))
        .flex()
        .items_center()
        .rounded(px(8.0))
        .cursor_pointer()
        .bg(rgb(theme::accent()))
        .text_color(rgb(theme::accent_fg()))
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .hover(|style| style.bg(rgb(theme::accent_hover())))
        .child(label)
}

fn secondary_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(36.0))
        .px(px(16.0))
        .flex()
        .items_center()
        .rounded(px(8.0))
        .cursor_pointer()
        .border_1()
        .border_color(rgb(theme::border_hover()))
        .text_color(rgb(theme::fg()))
        .text_sm()
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .child(label)
}
