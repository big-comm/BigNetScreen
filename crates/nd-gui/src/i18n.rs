//! Internationalisation (gettext).
//!
//! Every piece of UI text goes through `tr!()`. The `msgid`s are in English,
//! which is the project's source language and what translation tooling expects
//! as input; the translations live in `po/<lang>.po` and are installed as
//! `<prefix>/share/locale/<lang>/LC_MESSAGES/bignetscreen.mo`.

/// The application's gettext domain.
pub const DOMAIN: &str = "bignetscreen";

/// Sets gettext up. Call once, at the start of `main`.
pub fn init() {
    gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");

    // Under Flatpak or a standard installation the directory is
    // `<prefix>/share/locale`. `BIGNETSCREEN_LOCALEDIR` allows running straight
    // from the build tree.
    let localedir = std::env::var("BIGNETSCREEN_LOCALEDIR").unwrap_or_else(|_| {
        if std::path::Path::new("/app/share/locale").exists() {
            "/app/share/locale".to_string()
        } else {
            "/usr/share/locale".to_string()
        }
    });

    if let Err(err) = gettextrs::bindtextdomain(DOMAIN, localedir.as_str()) {
        tracing::warn!(%err, "bindtextdomain failed; the UI will stay in the source language");
    }
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    if let Err(err) = gettextrs::textdomain(DOMAIN) {
        tracing::warn!(%err, "textdomain failed");
    }
    tracing::debug!(%localedir, "i18n initialised");
}

/// Translates a piece of text.
#[macro_export]
macro_rules! tr {
    ($msg:expr) => {
        gettextrs::gettext($msg)
    };
    ($msg:expr, $($arg:tt)*) => {
        format!("{}", gettextrs::gettext($msg))
            .replacen("{}", &format!($($arg)*), 1)
    };
}

/// Translates honouring plural forms.
#[macro_export]
macro_rules! tr_n {
    ($singular:expr, $plural:expr, $n:expr) => {
        gettextrs::ngettext($singular, $plural, $n as u32)
    };
}
