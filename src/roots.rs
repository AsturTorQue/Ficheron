//! Shared pieces for the tools that work on folders: a list of folders to work on, a single
//! destination box, a file-and-folder picker and the folder window they open.

use crate::dupes::Browser;
use crate::i18n::tr;
use crate::sync::quick_menu;
use crate::theme;
use eframe::egui::{self, RichText};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The folder window; returns the folders picked this frame. With `keep_open` it stays for more.
pub fn folder_window(ctx: &egui::Context, id: &str, slot: &mut Option<Browser>, keep_open: bool) -> Vec<PathBuf> {
    let Some(mut b) = slot.take() else { return vec![] };
    let mut open = true;
    let mut picks: Vec<PathBuf> = vec![];
    let mut nav: Option<PathBuf> = None;
    let mut close = false;
    egui::Window::new(tr("Elegir carpeta")).id(egui::Id::new(("folder_window", id))).open(&mut open).default_width(480.0).default_height(440.0).show(ctx, |ui| {
        ui.horizontal(|ui| {
            // From a drive's root (or «/») one level up is «Mi equipo», where all the drives are.
            if ui.add_enabled(!b.path.as_os_str().is_empty(), egui::Button::new("⬆")).on_hover_text(tr("Carpeta superior")).clicked() {
                nav = Some(b.path.parent().map(Path::to_path_buf).unwrap_or_default());
            }
            if ui.button("🏠").on_hover_text(tr("Carpeta personal")).clicked() {
                nav = dirs::home_dir();
            }
            if ui.button("💻").on_hover_text(tr("Mi equipo: todas las unidades")).clicked() {
                nav = Some(PathBuf::new());
            }
            let r = ui.add(egui::TextEdit::singleline(&mut b.input).desired_width(ui.available_width() - 4.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let t = b.input.trim();
                nav = Some(if t.is_empty() || t == tr("Mi equipo") { PathBuf::new() } else { PathBuf::from(t) });
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (name, p) in crate::dupes::quick_places().into_iter().take(8) {
                if ui.small_button(name).on_hover_text(p.to_string_lossy()).clicked() {
                    nav = Some(p);
                }
            }
        });
        ui.separator();
        let n_entries = b.entries.len();
        egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 270.0)).auto_shrink([false, false]).show_rows(ui, 26.0, n_entries.max(1), |ui, range| {
            if b.entries.is_empty() {
                ui.label(RichText::new(tr("No hay subcarpetas.")).color(theme::p().muted));
            }
            for k in range {
                let Some((name, p)) = b.entries.get(k) else { break };
                ui.horizontal(|ui| {
                    if ui.small_button("✔").on_hover_text(tr("Elegir esta carpeta")).clicked() {
                        picks.push(p.clone());
                    }
                    if ui.link(format!("📁 {name}")).on_hover_text(tr("Abrir")).clicked() {
                        nav = Some(p.clone());
                    }
                });
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            let name = b.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| b.path.to_string_lossy().to_string());
            if ui.add_enabled(!b.path.as_os_str().is_empty(), egui::Button::new(trf!("✔ Usar «{}»", name))).on_hover_text(b.path.to_string_lossy()).clicked() {
                picks.push(b.path.clone());
            }
            if ui.button(tr("Cerrar")).clicked() {
                close = true;
            }
        });
    });
    if let Some(p) = nav.filter(|p| p.as_os_str().is_empty() || p.is_dir()) {
        b.go(p);
    }
    if open && !close && (keep_open || picks.is_empty()) {
        *slot = Some(b);
    }
    picks
}

/// A list of folders to work on, with Browse, quick-add and typed-path buttons.
pub struct RootList {
    pub roots: Vec<PathBuf>,
    input: String,
    browser: Option<Browser>,
    id: &'static str,
    pub message: Option<String>,
}

impl RootList {
    pub fn new(roots: Vec<PathBuf>, id: &'static str) -> RootList {
        let mut r = RootList { roots: vec![], input: String::new(), browser: None, id, message: None };
        for p in roots {
            r.add(p);
        }
        r
    }

    pub fn add(&mut self, p: PathBuf) {
        if p.is_dir() && !self.roots.contains(&p) {
            self.roots.push(p);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path, title: &str) {
        ui.label(RichText::new(title).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt((self.id, "list")).max_height(80.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, r) in self.roots.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(format!("{}. {}", k + 1, r.display()));
                });
            }
            if self.roots.is_empty() {
                ui.label(RichText::new(tr("Todavía no hay ninguna carpeta. Pulsa «Examinar» o «Añadir rápido» para elegir dónde buscar.")).color(theme::p().muted));
            }
        });
        if let Some(k) = remove {
            self.roots.remove(k);
        }
        let mut browse = false;
        let mut add: Vec<PathBuf> = vec![];
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("📂 Examinar…")).clicked() {
                browse = true;
            }
            if let Some(p) = quick_menu(ui, cwd, selection) {
                add.push(p);
            }
            let r = ui.add(egui::TextEdit::singleline(&mut self.input).hint_text(tr("o pega una ruta aquí")).desired_width(240.0));
            if (ui.button(tr("Añadir")).clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))) && !self.input.trim().is_empty() {
                let p = PathBuf::from(self.input.trim());
                if p.is_dir() {
                    add.push(p);
                    self.input.clear();
                    self.message = None;
                } else {
                    self.message = Some(trf!("«{}» no es una carpeta", p.display()));
                }
            }
            ui.label(RichText::new(tr("También puedes arrastrar carpetas aquí.")).small().color(theme::p().muted));
        });
        if let Some(m) = &self.message {
            ui.label(RichText::new(m).small().color(theme::p().danger));
        }
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        add.extend(dropped);
        for p in add {
            self.add(p);
        }
        if browse {
            let start = self.roots.last().and_then(|r| r.parent().map(Path::to_path_buf)).or_else(dirs::home_dir).unwrap_or_else(|| cwd.to_path_buf());
            self.browser = Some(Browser::new(start, 0));
        }
        let ctx = ui.ctx().clone();
        for p in folder_window(&ctx, self.id, &mut self.browser, true) {
            self.add(p);
        }
    }
}

/// One folder box (a destination) with Browse and quick-add.
pub struct DestField {
    pub text: String,
    browser: Option<Browser>,
    id: &'static str,
}

impl DestField {
    pub fn new(id: &'static str) -> DestField {
        DestField { text: String::new(), browser: None, id }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.add(egui::TextEdit::singleline(&mut self.text).hint_text(tr("carpeta destino")).desired_width(300.0));
        if ui.button(tr("📂 Examinar…")).clicked() {
            let cur = PathBuf::from(self.text.trim());
            self.browser = Some(Browser::new(if cur.is_dir() { cur } else { dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf()) }, 1));
        }
        if let Some(p) = quick_menu(ui, cwd, selection) {
            self.text = p.to_string_lossy().to_string();
        }
        let ctx = ui.ctx().clone();
        if let Some(p) = folder_window(&ctx, self.id, &mut self.browser, false).pop() {
            self.text = p.to_string_lossy().to_string();
        }
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.text.trim())
    }
}

/// Navigates folders and ticks files and folders.
pub struct Picker {
    pub path: PathBuf,
    pub input: String,
    pub entries: Vec<(String, PathBuf, bool)>,
    pub checked: BTreeSet<PathBuf>,
    pub hidden: bool,
    /// Offer «Añadir esta carpeta» (off where only files make sense).
    pub folders_ok: bool,
}

impl Picker {
    pub fn new(path: PathBuf) -> Picker {
        let mut p = Picker { path: PathBuf::new(), input: String::new(), entries: vec![], checked: BTreeSet::new(), hidden: false, folders_ok: true };
        p.go(path);
        p
    }

    pub fn go(&mut self, p: PathBuf) {
        // An empty path is «Mi equipo»: every drive, above all the drive roots.
        if p.as_os_str().is_empty() {
            self.entries = crate::dupes::drive_list().into_iter().map(|(_, n, p)| (n, p, true)).collect();
            self.input = tr("Mi equipo").to_string();
            self.path = PathBuf::new();
            return;
        }
        let p = crate::fsops::canonical(&p);
        let mut entries: Vec<(String, PathBuf, bool)> = fs::read_dir(&p)
            .map(|rd| rd.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), e.path(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))).collect())
            .unwrap_or_default();
        entries.retain(|(n, _, _)| self.hidden || !n.starts_with('.'));
        entries.sort_by_key(|(n, _, d)| (!*d, n.to_lowercase()));
        self.input = p.to_string_lossy().to_string();
        self.path = p;
        self.entries = entries;
    }
}

/// The picker window; returns what was added this frame.
pub fn picker_window(ctx: &egui::Context, id: &str, slot: &mut Option<Picker>) -> Vec<PathBuf> {
    let Some(mut pk) = slot.take() else { return vec![] };
    let mut open = true;
    let mut nav: Option<PathBuf> = None;
    let mut add = false;
    let mut this_folder = false;
    let mut close = false;
    egui::Window::new(tr("Elegir archivos y carpetas")).id(egui::Id::new(("picker_window", id))).open(&mut open).default_width(520.0).default_height(480.0).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.add_enabled(!pk.path.as_os_str().is_empty(), egui::Button::new("⬆")).on_hover_text(tr("Carpeta superior")).clicked() {
                nav = Some(pk.path.parent().map(Path::to_path_buf).unwrap_or_default());
            }
            if ui.button("🏠").on_hover_text(tr("Carpeta personal")).clicked() {
                nav = dirs::home_dir();
            }
            if ui.button("💻").on_hover_text(tr("Mi equipo: todas las unidades")).clicked() {
                nav = Some(PathBuf::new());
            }
            let r = ui.add(egui::TextEdit::singleline(&mut pk.input).desired_width(ui.available_width() - 130.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let t = pk.input.trim();
                nav = Some(if t.is_empty() || t == tr("Mi equipo") { PathBuf::new() } else { PathBuf::from(t) });
            }
            if ui.checkbox(&mut pk.hidden, tr("ocultos")).changed() {
                nav = Some(pk.path.clone());
            }
        });
        ui.separator();
        let n_entries = pk.entries.len();
        egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 330.0)).auto_shrink([false, false]).show_rows(ui, 26.0, n_entries, |ui, range| {
            // Only the visible rows are drawn (and copied): a folder can hold tens of thousands of entries.
            for k in range {
                let (name, path, is_dir) = &pk.entries[k].clone();
                ui.horizontal(|ui| {
                    let mut on = pk.checked.contains(path);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            pk.checked.insert(path.clone());
                        } else {
                            pk.checked.remove(path);
                        }
                    }
                    if *is_dir {
                        if ui.link(format!("📁 {name}")).on_hover_text(tr("Abrir")).clicked() {
                            nav = Some(path.clone());
                        }
                    } else {
                        ui.label(format!("🗋 {name}"));
                    }
                });
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.small_button(tr("Marcar todo")).clicked() {
                let all: Vec<PathBuf> = pk.entries.iter().map(|e| e.1.clone()).collect();
                pk.checked.extend(all);
            }
            if ui.small_button(tr("Desmarcar todo")).clicked() {
                pk.checked.clear();
            }
            if ui.add_enabled(!pk.checked.is_empty(), egui::Button::new(trf!("➕ Añadir {} marcado(s)", pk.checked.len()))).clicked() {
                add = true;
            }
            // The folder being looked at, whole, without going up to tick it.
            let name = pk.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| pk.path.to_string_lossy().to_string());
            if pk.folders_ok && ui.add_enabled(!pk.path.as_os_str().is_empty(), egui::Button::new(trf!("📂 Añadir esta carpeta («{}»)", name))).on_hover_text(pk.path.to_string_lossy()).clicked() {
                this_folder = true;
            }
            if ui.button(tr("Cerrar")).clicked() {
                close = true;
            }
        });
    });
    if let Some(p) = nav.filter(|p| p.as_os_str().is_empty() || p.is_dir()) {
        pk.go(p);
    }
    let mut out = vec![];
    if add {
        out = pk.checked.iter().cloned().collect();
        pk.checked.clear();
    }
    if this_folder {
        out.push(pk.path.clone());
    }
    if open && !close {
        *slot = Some(pk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_path_is_my_computer_with_the_drives() {
        let b = Browser::new(PathBuf::new(), 0);
        assert!(b.path.as_os_str().is_empty() && !b.entries.is_empty(), "the drives are listed");
        #[cfg(unix)]
        assert!(b.entries.iter().any(|(_, p)| p == Path::new("/")), "the root filesystem is a drive on Unix");
        let pk = Picker::new(PathBuf::new());
        assert!(pk.path.as_os_str().is_empty() && pk.entries.iter().all(|e| e.2), "every entry is a folder");
        // Going up from the top of a drive reaches «Mi equipo»; from there, into a drive.
        let mut b = Browser::new(PathBuf::from("/"), 0);
        assert_eq!(b.path.parent(), None);
        b.go(b.path.parent().map(Path::to_path_buf).unwrap_or_default());
        assert!(b.path.as_os_str().is_empty());
        let first = b.entries[0].1.clone();
        b.go(first.clone());
        assert_eq!(b.path, crate::fsops::canonical(&first));
    }
}
