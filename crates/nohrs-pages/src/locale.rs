//! Which language the interface starts in when `ui.language` is "auto".
//!
//! The regional format comes first (Settings › Time & language › Region, e.g.
//! `pt-BR`): it is what the person picked for where they live, and it is what
//! the request was — "the language of the place you are". When its language
//! has no translation, the country set in Windows decides (a French format in
//! Mexico still most likely reads Spanish). Then the display language, and
//! last English.

use nohrs_core::i18n::Lang;

/// Finds the language on this machine and records it for "auto".
pub fn detect_and_record() -> Lang {
    let system = system_locale();
    let lang = choose(
        system.locale.as_deref(),
        system.country.as_deref(),
        system.ui_language.as_deref(),
    );
    tracing::info!(
        locale = ?system.locale,
        country = ?system.country,
        ui_language = ?system.ui_language,
        language = lang.code(),
        "interface language detected"
    );
    nohrs_core::i18n::set_detected(lang);
    lang
}

fn choose(locale: Option<&str>, country: Option<&str>, ui_language: Option<&str>) -> Lang {
    locale
        .and_then(Lang::parse)
        .or_else(|| country.and_then(Lang::for_country))
        // The region part of the format names the country too (`fr-MX`).
        .or_else(|| {
            locale
                .and_then(|name| name.split(['-', '_']).nth(1))
                .and_then(Lang::for_country)
        })
        .or_else(|| ui_language.and_then(Lang::parse))
        .unwrap_or(Lang::En)
}

#[derive(Default)]
struct SystemLocale {
    locale: Option<String>,
    country: Option<String>,
    ui_language: Option<String>,
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn system_locale() -> SystemLocale {
    use windows::Win32::Globalization::{
        GetUserDefaultGeoName, GetUserDefaultLocaleName, GetUserDefaultUILanguage, LCIDToLocaleName,
    };

    fn text(buffer: &[u16], written: i32) -> Option<String> {
        // The count includes the terminating NUL; 0 means the call failed.
        let length = usize::try_from(written).ok()?.checked_sub(1)?;
        let name = String::from_utf16_lossy(buffer.get(..length)?);
        (!name.is_empty()).then_some(name)
    }

    let mut buffer = [0u16; 85];
    // SAFETY: each call writes at most `buffer.len()` UTF-16 units into a
    // buffer it borrows only for the call.
    let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    let locale = text(&buffer, written);
    let written = unsafe { GetUserDefaultGeoName(&mut buffer) };
    let country = text(&buffer, written);
    let language_id = unsafe { GetUserDefaultUILanguage() };
    let written = unsafe { LCIDToLocaleName(u32::from(language_id), Some(&mut buffer), 0) };
    let ui_language = text(&buffer, written);
    SystemLocale {
        locale,
        country,
        ui_language,
    }
}

#[cfg(not(windows))]
fn system_locale() -> SystemLocale {
    SystemLocale {
        locale: std::env::var("LANG")
            .ok()
            .map(|value| value.split('.').next().unwrap_or_default().to_string()),
        ..SystemLocale::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regional_format_wins() {
        assert_eq!(choose(Some("pt-BR"), Some("US"), Some("en-US")), Lang::Pt);
        assert_eq!(choose(Some("es-AR"), None, None), Lang::Es);
        assert_eq!(choose(Some("en-GB"), Some("BR"), None), Lang::En);
        assert_eq!(choose(Some("fr-FR"), Some("MX"), None), Lang::Fr);
        assert_eq!(choose(Some("ja-JP"), None, None), Lang::Ja);
    }

    #[test]
    fn untranslated_format_falls_to_the_country_then_the_display_language() {
        assert_eq!(choose(Some("ko-KR"), Some("MX"), Some("en-US")), Lang::Es);
        assert_eq!(choose(Some("qu-PE"), None, None), Lang::Es);
        assert_eq!(choose(Some("ko-KR"), Some("KR"), Some("pt-BR")), Lang::Pt);
        assert_eq!(choose(Some("ko-KR"), Some("KR"), Some("ko-KR")), Lang::En);
        assert_eq!(choose(None, None, None), Lang::En);
    }
}
