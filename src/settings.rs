//! Language, theme and accent colour, kept between runs.

use crate::{i18n, theme};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Tools open in windows of their own (true) or inside the main window (false). Inside is the
/// default on Linux, where many desktops open a new window behind the one that asked for it.
static SEPARATE: AtomicBool = AtomicBool::new(!cfg!(target_os = "linux"));

static EXPLORER: AtomicBool = AtomicBool::new(false);
/// Size of the whole interface, in percent.
static ZOOM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);

pub fn zoom() -> f32 {
    ZOOM.load(Ordering::Relaxed) as f32 / 100.0
}

pub fn set_zoom(z: f32) {
    let pct = (z * 100.0).round().clamp(50.0, 250.0) as u32;
    if pct != ZOOM.load(Ordering::Relaxed) {
        ZOOM.store(pct, Ordering::Relaxed);
        save();
    }
}

/// Open on the file explorer instead of the start screen with the utilities.
pub fn start_with_explorer() -> bool {
    EXPLORER.load(Ordering::Relaxed)
}

pub fn set_start_with_explorer(on: bool) {
    EXPLORER.store(on, Ordering::Relaxed);
    save();
}

pub fn separate_windows() -> bool {
    SEPARATE.load(Ordering::Relaxed)
}

pub fn set_separate_windows(on: bool) {
    SEPARATE.store(on, Ordering::Relaxed);
    save();
}

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
            "ventanas_aparte" => SEPARATE.store(v == "1", Ordering::Relaxed),
            "inicio_explorador" => EXPLORER.store(v == "1", Ordering::Relaxed),
            "escala" => ZOOM.store(v.parse::<u32>().unwrap_or(100).clamp(50, 250), Ordering::Relaxed),
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
    let s = format!("idioma={}\ntema={}\nacento={}\nventanas_aparte={}\ninicio_explorador={}\nescala={}\n", i18n::LANGS[i18n::lang()].0, theme::theme_index(), theme::accent_index(), separate_windows() as u8, start_with_explorer() as u8, ZOOM.load(Ordering::Relaxed));
    let _ = std::fs::write(f, s);
}
