//! Colour themes: a palette for every part of the window, plus an optional accent colour.

use eframe::egui::{self, Color32, Stroke};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct Palette {
    pub name: &'static str,
    pub dark: bool,
    pub base: Color32,
    pub bar: Color32,
    pub side: Color32,
    pub side2: Color32,
    pub text: Color32,
    pub sub: Color32,
    pub muted: Color32,
    pub border: Color32,
    pub hover: Color32,
    pub stripe: Color32,
    pub button: Color32,
    pub danger: Color32,
    pub ok: Color32,
    pub accent: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}
const fn gray(v: u8) -> Color32 {
    Color32::from_rgb(v, v, v)
}

pub const PALETTES: [Palette; 6] = [
    Palette {
        name: "Claro",
        dark: false,
        base: Color32::WHITE,
        bar: rgb(240, 241, 244),
        side: rgb(244, 245, 247),
        side2: rgb(248, 248, 250),
        text: gray(30),
        sub: gray(100),
        muted: gray(150),
        border: gray(212),
        hover: gray(234),
        stripe: gray(247),
        button: gray(228),
        danger: rgb(190, 40, 40),
        ok: rgb(30, 120, 60),
        accent: rgb(53, 132, 228),
    },
    Palette {
        name: "Oscuro",
        dark: true,
        base: rgb(30, 31, 34),
        bar: rgb(40, 42, 46),
        side: rgb(35, 37, 41),
        side2: rgb(35, 37, 41),
        text: rgb(225, 227, 230),
        sub: rgb(160, 164, 170),
        muted: rgb(115, 119, 125),
        border: rgb(62, 65, 70),
        hover: rgb(52, 55, 60),
        stripe: rgb(34, 35, 39),
        button: rgb(55, 58, 63),
        danger: rgb(240, 110, 110),
        ok: rgb(110, 200, 130),
        accent: rgb(70, 145, 240),
    },
    Palette {
        name: "Nord",
        dark: true,
        base: rgb(46, 52, 64),
        bar: rgb(59, 66, 82),
        side: rgb(52, 59, 74),
        side2: rgb(52, 59, 74),
        text: rgb(236, 239, 244),
        sub: rgb(190, 198, 212),
        muted: rgb(140, 150, 168),
        border: rgb(76, 86, 106),
        hover: rgb(67, 76, 94),
        stripe: rgb(50, 56, 69),
        button: rgb(67, 76, 94),
        danger: rgb(225, 125, 135),
        ok: rgb(163, 190, 140),
        accent: rgb(94, 129, 172),
    },
    Palette {
        name: "Sepia",
        dark: false,
        base: rgb(253, 248, 238),
        bar: rgb(240, 230, 212),
        side: rgb(245, 237, 222),
        side2: rgb(248, 242, 230),
        text: rgb(60, 45, 30),
        sub: rgb(115, 95, 70),
        muted: rgb(160, 140, 115),
        border: rgb(215, 200, 175),
        hover: rgb(235, 222, 200),
        stripe: rgb(249, 243, 232),
        button: rgb(232, 220, 198),
        danger: rgb(170, 50, 40),
        ok: rgb(60, 120, 50),
        accent: rgb(176, 106, 48),
    },
    Palette {
        name: "Menta",
        dark: false,
        base: rgb(250, 253, 251),
        bar: rgb(228, 242, 235),
        side: rgb(237, 246, 241),
        side2: rgb(242, 249, 245),
        text: rgb(25, 45, 35),
        sub: rgb(80, 110, 95),
        muted: rgb(140, 165, 150),
        border: rgb(200, 222, 210),
        hover: rgb(220, 238, 228),
        stripe: rgb(244, 250, 246),
        button: rgb(216, 234, 224),
        danger: rgb(180, 50, 50),
        ok: rgb(30, 120, 70),
        accent: rgb(32, 150, 100),
    },
    Palette {
        name: "Alto contraste",
        dark: true,
        base: Color32::BLACK,
        bar: gray(22),
        side: gray(12),
        side2: gray(12),
        text: Color32::WHITE,
        sub: gray(225),
        muted: gray(185),
        border: gray(200),
        hover: gray(60),
        stripe: gray(26),
        button: gray(45),
        danger: rgb(255, 120, 120),
        ok: rgb(120, 255, 150),
        accent: rgb(25, 120, 255),
    },
];

/// Accent colours; the first one keeps the theme's own.
pub const ACCENTS: [(&str, Color32); 7] = [
    ("Del tema", Color32::TRANSPARENT),
    ("Azul", rgb(53, 132, 228)),
    ("Verde", rgb(38, 162, 105)),
    ("Morado", rgb(145, 65, 172)),
    ("Naranja", rgb(230, 97, 0)),
    ("Rosa", rgb(213, 66, 142)),
    ("Rojo", rgb(200, 45, 50)),
];

/// 0 follows the system (light or dark); 1.. are PALETTES[n - 1].
static THEME: AtomicUsize = AtomicUsize::new(0);
static ACCENT: AtomicUsize = AtomicUsize::new(0);
static SYSTEM_DARK: AtomicBool = AtomicBool::new(false);

pub fn theme_index() -> usize {
    THEME.load(Ordering::Relaxed)
}
pub fn set_theme(i: usize) {
    THEME.store(i.min(PALETTES.len()), Ordering::Relaxed);
}
pub fn accent_index() -> usize {
    ACCENT.load(Ordering::Relaxed)
}
pub fn set_accent(i: usize) {
    ACCENT.store(i.min(ACCENTS.len() - 1), Ordering::Relaxed);
}

pub fn p() -> &'static Palette {
    match theme_index() {
        0 => &PALETTES[if SYSTEM_DARK.load(Ordering::Relaxed) { 1 } else { 0 }],
        i => &PALETTES[i - 1],
    }
}

pub fn accent() -> Color32 {
    match accent_index() {
        0 => p().accent,
        i => ACCENTS[i].1,
    }
}

/// Follows the system's light/dark choice when the theme is automatic. Returns true when it changed.
pub fn follow_system(ctx: &egui::Context) -> bool {
    let dark = ctx.input(|i| i.raw.system_theme) == Some(egui::Theme::Dark);
    SYSTEM_DARK.swap(dark, Ordering::Relaxed) != dark && theme_index() == 0
}

pub fn apply(ctx: &egui::Context) {
    follow_system(ctx);
    let p = p();
    let a = accent();
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.selection.bg_fill = a;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = a;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.panel_fill = p.side2;
    v.window_fill = p.base;
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.extreme_bg_color = p.base;
    v.faint_bg_color = p.stripe;
    v.code_bg_color = p.stripe;
    v.warn_fg_color = p.danger;
    v.error_fg_color = p.danger;
    v.widgets.noninteractive.bg_fill = p.side2;
    v.widgets.noninteractive.weak_bg_fill = p.side2;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.inactive.bg_fill = p.button;
    v.widgets.inactive.weak_bg_fill = p.button;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, p.text);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, a);
    v.widgets.active.bg_fill = a.gamma_multiply(0.5);
    v.widgets.active.weak_bg_fill = a.gamma_multiply(0.5);
    v.widgets.active.fg_stroke = Stroke::new(1.5_f32, p.text);
    v.widgets.open.weak_bg_fill = p.hover;
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, p.text);
    ctx.set_visuals(v);
}
