//! Language, theme and accent colour, kept between runs.

use crate::{i18n, theme};
use std::path::PathBuf;

fn file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("ajustes.txt"))
}

pub fn load() {
    let text = file().and_then(|f| std::fs::read_to_string(f).ok());
    let mut lang = None;
    for line in text.as_deref().unwrap_or("").lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "idioma" => lang = i18n::LANGS.iter().position(|(code, _)| *code == v),
            "tema" => theme::set_theme(v.parse().unwrap_or(0)),
            "acento" => theme::set_accent(v.parse().unwrap_or(0)),
            _ => {}
        }
    }
    i18n::set_lang(lang.unwrap_or_else(i18n::system_language));
}

pub fn save() {
    let Some(f) = file() else { return };
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let s = format!("idioma={}\ntema={}\nacento={}\n", i18n::LANGS[i18n::lang()].0, theme::theme_index(), theme::accent_index());
    let _ = std::fs::write(f, s);
}
