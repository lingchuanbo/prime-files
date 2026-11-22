//! What kind of file an entry is, as the listing paints it: the extension
//! badge in the type column, the type's name beside it, and the colour the
//! type gives the row's name, badge, size and date.
//!
//! The colour comes from the user's filename rules first (the setting that
//! used to paint only the name), and from a built-in palette by kind when no
//! rule covers the file: programs green, archives amber, files orange, images
//! purple, video pink, sound cyan and documents blue, so a folder reads at a
//! glance without anyone configuring anything.

use super::filter::KindFilter;
use nohrs_core::t;

/// The type of one listing entry, ready to be drawn.
pub(crate) struct TypeStyle {
    /// The extension in upper case, for the badge; empty when there is none
    /// to show (a folder, a link, a name without an extension).
    pub(crate) extension: String,
    /// The type's name, as the type column shows it beside the badge.
    pub(crate) label: &'static str,
    /// The colour rules and the built-in palette give the type; `None` for
    /// entries whose rows stay neutral, as folders' do.
    pub(crate) color: Option<u32>,
}

impl TypeStyle {
    /// The type of the entry named `name` of kind `kind`. `rule_color` is the
    /// colour a filename rule gives it, which wins over the built-in palette.
    pub(crate) fn of(name: &str, kind: &str, rule_color: Option<u32>) -> Self {
        match kind {
            "dir" => {
                return Self {
                    extension: String::new(),
                    label: t!("Pasta"),
                    color: None,
                };
            }
            "symlink" => {
                return Self {
                    extension: String::new(),
                    label: t!("Atalho"),
                    color: None,
                };
            }
            // Anything else is shown as a file: the listing reads kinds from
            // the filesystem, and a folder's own colour would paint a row the
            // rules never meant to colour.
            _ => {}
        }
        let extension = extension_of(name);
        let kind = match extension.as_deref() {
            Some(extension) => KindFilter::of_extension(extension),
            None => KindFilter::Files,
        };
        Self {
            extension: extension.unwrap_or_default(),
            label: type_name(kind),
            color: Some(rule_color.unwrap_or_else(|| default_color(kind))),
        }
    }
}

/// The extension of `name` in upper case, or `None` when it has none worth a
/// badge. A dotfile (`.gitignore`) has no extension: Windows hides that dot
/// from the name, and a badge reading "GITIGNORE" would not be a type.
fn extension_of(name: &str) -> Option<String> {
    if name.starts_with('.') {
        return None;
    }
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())?;
    Some(extension.to_uppercase())
}

/// The type's name for a kind, in the language of the interface.
fn type_name(kind: KindFilter) -> &'static str {
    match kind {
        KindFilter::Images => t!("Imagens"),
        KindFilter::Videos => t!("Vídeos"),
        KindFilter::Audio => t!("Áudio"),
        KindFilter::Documents => t!("Documentos"),
        KindFilter::Archives => t!("Compactados"),
        KindFilter::Programs => t!("Programas"),
        KindFilter::Files | KindFilter::Folders | KindFilter::All => t!("Arquivo"),
    }
}

/// The colour the built-in palette gives a kind, for a file no rule covers.
fn default_color(kind: KindFilter) -> u32 {
    match kind {
        // A program people run.
        KindFilter::Programs => 0x22C55E,
        // Something that has to be unpacked before it is useful.
        KindFilter::Archives => 0xF59E0B,
        // Media by what it plays or shows.
        KindFilter::Images => 0xA855F7,
        KindFilter::Videos => 0xEC4899,
        KindFilter::Audio => 0x06B6D4,
        // Text, office files and PDFs.
        KindFilter::Documents => 0x3B82F6,
        // Everything else, the plain file: the one colour that says "nothing
        // more specific than a file".
        KindFilter::Files | KindFilter::Folders | KindFilter::All => 0xF97316,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_badged_in_upper_case_and_named_by_kind() {
        let exe = TypeStyle::of("setup.exe", "file", None);
        assert_eq!(exe.extension, "EXE");
        assert_eq!(exe.label, t!("Programas"));
        assert_eq!(exe.color, Some(default_color(KindFilter::Programs)));

        let archive = TypeStyle::of("backup.7z", "file", None);
        assert_eq!(archive.extension, "7Z");
        assert_eq!(archive.label, t!("Compactados"));
        assert_eq!(archive.color, Some(default_color(KindFilter::Archives)));

        // A kind the filter bar knows nothing more specific about is a file.
        let ini = TypeStyle::of("config.ini", "file", None);
        assert_eq!(ini.label, t!("Arquivo"));
        assert_eq!(ini.color, Some(default_color(KindFilter::Files)));

        let photo = TypeStyle::of("Shot.PNG", "file", None);
        assert_eq!(photo.extension, "PNG");
        assert_eq!(photo.label, t!("Imagens"));
    }

    #[test]
    fn a_rule_colour_wins_over_the_built_in_palette() {
        let themed = TypeStyle::of("setup.exe", "file", Some(0x123456));
        assert_eq!(themed.color, Some(0x123456));
    }

    #[test]
    fn folders_links_and_extensionless_names_stay_unbadged() {
        let folder = TypeStyle::of("src", "dir", Some(0x123456));
        assert_eq!(folder.extension, "");
        assert_eq!(folder.label, t!("Pasta"));
        assert_eq!(folder.color, None, "a folder's row is not painted by type");

        let link = TypeStyle::of("shortcut", "symlink", None);
        assert_eq!(link.extension, "");
        assert_eq!(link.label, t!("Atalho"));
        assert_eq!(link.color, None);

        let plain = TypeStyle::of("README", "file", None);
        assert_eq!(plain.extension, "");
        assert_eq!(plain.label, t!("Arquivo"));
        assert_eq!(plain.color, Some(default_color(KindFilter::Files)));
    }

    #[test]
    fn a_dotfile_is_not_an_extension_badge() {
        assert_eq!(extension_of(".gitignore"), None);
        assert_eq!(extension_of("archive.tar.gz").as_deref(), Some("GZ"));
    }
}
