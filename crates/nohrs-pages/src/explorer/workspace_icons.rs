use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkspaceIcon {
    Bookmark,
    Briefcase,
    User,
    Layers,
    Palette,
    Swords,
    Code,
    Kanban,
    #[default]
    #[serde(other)]
    Tag,
}

impl WorkspaceIcon {
    pub(crate) const ALL: [Self; 9] = [
        Self::Bookmark, Self::Briefcase, Self::User, Self::Layers, Self::Palette,
        Self::Swords, Self::Code, Self::Kanban, Self::Tag,
    ];

    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Bookmark => "icons/aura-bookmark.svg",
            Self::Briefcase => "icons/aura-briefcase.svg",
            Self::User => "icons/aura-user.svg",
            Self::Layers => "icons/aura-layers.svg",
            Self::Palette => "icons/aura-palette.svg",
            Self::Swords => "icons/aura-swords.svg",
            Self::Code => "icons/aura-code-xml.svg",
            Self::Kanban => "icons/aura-folder-kanban.svg",
            Self::Tag => "icons/aura-tag.svg",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bookmark => nohrs_core::t!("Marcador"),
            Self::Briefcase => nohrs_core::t!("Maleta"),
            Self::User => nohrs_core::t!("Pessoa"),
            Self::Layers => nohrs_core::t!("Camadas"),
            Self::Palette => nohrs_core::t!("Paleta"),
            Self::Swords => nohrs_core::t!("Espadas"),
            Self::Code => nohrs_core::t!("Código"),
            Self::Kanban => nohrs_core::t!("Quadro"),
            Self::Tag => nohrs_core::t!("Etiqueta"),
        }
    }

    pub(crate) fn for_legacy_id(id: u32) -> Self {
        match id {
            0 => Self::Bookmark,
            1 => Self::Briefcase,
            2 => Self::User,
            3 => Self::Layers,
            _ => Self::Tag,
        }
    }
}
