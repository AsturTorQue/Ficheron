//! The few building blocks every tool window uses, so all of them look and behave alike:
//! a label column of the same width, the same buttons for choosing an option, the same section
//! titles and notes.

use crate::i18n::tr;
use crate::theme;
use eframe::egui::{self, RichText};

/// Width of the label column in every tool.
pub const LABEL_W: f32 = 120.0;

/// A label in the label column, left-aligned.
pub fn label(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let text: String = text.into();
    crate::checksum::cell(ui, LABEL_W, 20.0, |ui| ui.label(RichText::new(text).color(theme::p().sub)))
}

/// One labelled line of options.
pub fn field(ui: &mut egui::Ui, text: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        label(ui, text);
        add(ui);
    });
}

/// A choice among a few options, always shown as the same row of buttons.
pub fn choice(ui: &mut egui::Ui, value: &mut u8, options: &[&str]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (k, l) in options.iter().enumerate() {
            ui.selectable_value(value, k as u8, tr(l));
        }
    });
}

pub fn bool_choice(ui: &mut egui::Ui, value: &mut bool, off: &str, on: &str) {
    let mut v = *value as u8;
    choice(ui, &mut v, &[off, on]);
    *value = v == 1;
}

/// A small grey explanation.
pub fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(theme::p().muted));
}
