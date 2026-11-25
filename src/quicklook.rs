//! Quick look: Space shows the selected file in a small window (picture, text, folder contents
//! or basic facts) without opening any other program. It follows the selection while open.

use crate::fsops::{self, human_size, Entry};
use crate::i18n::tr;
use crate::theme;
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};

enum Content {
    Image(egui::TextureHandle, [usize; 2]),
    Text(String, bool),
    Folder(Vec<String>, usize),
    Nothing,
}

pub struct QuickLook {
    pub path: PathBuf,
    name: String,
    facts: String,
    content: Content,
}

const MAX_IMAGE_BYTES: u64 = 80 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;

fn is_picture_ext(e: &str) -> bool {
    matches!(e, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "ico" | "avif" | "heic" | "heif")
}

fn load_image(path: &Path) -> Option<image::DynamicImage> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "avif" | "heic" | "heif") {
        return crate::images::decode_with_ffmpeg(path).ok();
    }
    image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()
}

fn load_text(path: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; MAX_TEXT_BYTES + 1];
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..]).ok()?;
        if k == 0 {
            break;
        }
        n += k;
    }
    let cut = n > MAX_TEXT_BYTES;
    buf.truncate(n.min(MAX_TEXT_BYTES));
    if buf.contains(&0) {
        return None;
    }
    Some((String::from_utf8_lossy(&buf).to_string(), cut))
}

impl QuickLook {
    pub fn open(ctx: &egui::Context, e: &Entry) -> QuickLook {
        let mut facts = e.kind_label().to_string();
        if !e.is_dir {
            facts = format!("{facts} · {}", human_size(e.size));
        }
        facts = format!("{facts} · {}", fsops::format_time(e.modified));
        let ext = e.path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let content = if e.is_dir {
            let mut names: Vec<String> = std::fs::read_dir(&e.path).map(|rd| rd.flatten().map(|d| format!("{} {}", if d.path().is_dir() { "📁" } else { "🗋" }, d.file_name().to_string_lossy())).collect()).unwrap_or_default();
            names.sort();
            let total = names.len();
            names.truncate(200);
            Content::Folder(names, total)
        } else if is_picture_ext(&ext) && e.size <= MAX_IMAGE_BYTES {
            match load_image(&e.path) {
                Some(img) => {
                    let img = if img.width().max(img.height()) > 1600 { img.thumbnail(1600, 1600) } else { img };
                    facts = format!("{facts} · {} × {}", img.width(), img.height());
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let tex = ctx.load_texture("quicklook", egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), egui::TextureOptions::LINEAR);
                    Content::Image(tex, size)
                }
                None => Content::Nothing,
            }
        } else {
            match load_text(&e.path) {
                Some((t, cut)) => Content::Text(t, cut),
                None => Content::Nothing,
            }
        };
        QuickLook { path: e.path.clone(), name: e.name.clone(), facts, content }
    }

    /// Draws the window; false once its close button was pressed.
    pub fn show(&self, ctx: &egui::Context) -> bool {
        let mut open = true;
        egui::Window::new(RichText::new(format!("👁 {}", self.name)).strong())
            .id(egui::Id::new("quicklook"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([760.0, 540.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(&self.facts).small().color(theme::p().sub));
                ui.separator();
                match &self.content {
                    Content::Image(tex, size) => {
                        let avail = ui.available_size();
                        let scale = (avail.x / size[0] as f32).min((avail.y - 6.0) / size[1] as f32).min(1.0).max(0.05);
                        ui.vertical_centered(|ui| {
                            ui.image((tex.id(), egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale)));
                        });
                    }
                    Content::Text(t, cut) => {
                        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(t).monospace()).wrap_mode(egui::TextWrapMode::Extend));
                            if *cut {
                                ui.label(RichText::new(tr("… (solo se muestra el principio)")).color(theme::p().muted));
                            }
                        });
                    }
                    Content::Folder(names, total) => {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            if names.is_empty() {
                                ui.label(RichText::new(tr("Esta carpeta está vacía")).color(theme::p().muted));
                            }
                            for n in names {
                                ui.label(n);
                            }
                            if *total > names.len() {
                                ui.label(RichText::new(trf!("… y {} más", total - names.len())).color(theme::p().muted));
                            }
                        });
                    }
                    Content::Nothing => {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("🗋").size(48.0).color(theme::p().muted));
                            ui.label(RichText::new(tr("No hay vista previa para este tipo de archivo")).color(theme::p().muted));
                        });
                    }
                }
            });
        open
    }
}
