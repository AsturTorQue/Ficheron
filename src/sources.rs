//! A list of files and folders to work on, with an "Examinar…" button, drag and drop, and the
//! filters every tool needs (include / exclude by name, folders to skip, subfolders, hidden
//! files). Shared by the tools that take files from the person instead of from the main window.

use crate::i18n::tr;
use crate::roots::{picker_window, Picker};
use crate::theme;
use eframe::egui::{self, RichText};
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct Filters {
    pub include: Option<Regex>,
    pub exclude: Option<Regex>,
    pub exclude_dirs: Option<Regex>,
}

pub struct Sources {
    id: &'static str,
    pub items: Vec<PathBuf>,
    picker: Option<Picker>,
    pub recursive: bool,
    pub hidden: bool,
    /// Name filters are regular expressions instead of wildcards.
    pub regex: bool,
    pub include: String,
    pub exclude: String,
    pub exclude_dirs: String,
}

impl Sources {
    pub fn new(id: &'static str, targets: Vec<PathBuf>) -> Sources {
        Sources {
            id,
            items: targets.into_iter().filter(|p| p.exists()).collect(),
            picker: None,
            recursive: true,
            hidden: false,
            regex: false,
            include: String::new(),
            exclude: String::new(),
            exclude_dirs: ".git; node_modules; .cache; $RECYCLE.BIN".into(),
        }
    }

    pub fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.exists() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    pub fn filters(&self) -> Result<Filters, String> {
        let m = |t: &str| crate::rename::name_matcher(t, self.regex).map_err(|e| trf!("Expresión no válida: {}", e));
        Ok(Filters { include: m(&self.include)?, exclude: m(&self.exclude)?, exclude_dirs: m(&self.exclude_dirs)? })
    }

    /// Every file to work on: the files added one by one, and those inside the folders that
    /// pass the filters. Sorted; unreadable folders are listed in the second value.
    #[cfg(test)]
    pub fn collect(&self, cancel: &AtomicBool) -> (Vec<PathBuf>, Vec<String>) {
        let Ok(f) = self.filters() else { return (vec![], vec![]) };
        collect_files(&self.items, &f, self.recursive, self.hidden, cancel)
    }

    /// The list, the buttons and the filters. Call every frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path, title: &str) {
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);
        ui.label(RichText::new(title).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt((self.id, "items")).max_height(crate::tools::grow(ui, 96.0)).auto_shrink([false, true]).show_rows(ui, 22.0, self.items.len().max(1), |ui, range| {
            if self.items.is_empty() {
                ui.label(RichText::new(tr("Todavía no hay nada. Añade los seleccionados, pulsa Examinar o arrastra archivos y carpetas aquí.")).color(theme::p().muted));
                return;
            }
            for k in range {
                let Some(p) = self.items.get(k) else { break };
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(if p.is_dir() { "📁" } else { "🗋" });
                    ui.add(egui::Label::new(p.to_string_lossy()).truncate());
                });
            }
        });
        if let Some(k) = remove {
            self.items.remove(k);
        }
        ui.horizontal_wrapped(|ui| {
            let sel: Vec<PathBuf> = selection.to_vec();
            if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).clicked() {
                self.add(sel);
            }
            if ui.button(tr("📂 Examinar…")).on_hover_text(tr("Elegir archivos y carpetas")).clicked() {
                let start = self.items.last().map(|p| if p.is_dir() { p.clone() } else { p.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.to_path_buf()) }).unwrap_or_else(|| cwd.to_path_buf());
                self.picker = Some(Picker::new(start));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
            ui.separator();
            ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas"));
            ui.checkbox(&mut self.hidden, tr("Incluir ocultos"));
            ui.checkbox(&mut self.regex, tr("Expresiones regulares")).on_hover_text(tr("Los filtros de nombre se leen como expresiones regulares (sin distinguir mayúsculas), p. ej. \\.(jpg|png)$ o ^IMG_\\d+"));
        });
        egui::Grid::new((self.id, "filters")).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            ui.label(tr("Incluir solo"));
            ui.add(egui::TextEdit::singleline(&mut self.include).hint_text(if self.regex { "\\.(jpg|png)$" } else { "*.jpg; *.png" }).desired_width(f32::INFINITY)).on_hover_text(tr("Patrones de nombre separados por «;», con * y ?. Vacío = todos. Solo afecta a los archivos de las carpetas."));
            ui.end_row();
            ui.label(tr("Excluir"));
            ui.add(egui::TextEdit::singleline(&mut self.exclude).hint_text(if self.regex { "\\.(tmp|log)$" } else { "*.tmp; *.log" }).desired_width(f32::INFINITY));
            ui.end_row();
            ui.label(tr("Saltar carpetas"));
            ui.add(egui::TextEdit::singleline(&mut self.exclude_dirs).desired_width(f32::INFINITY));
            ui.end_row();
        });
        let ctx = ui.ctx().clone();
        let added = picker_window(&ctx, self.id, &mut self.picker);
        self.add(added);
    }
}

/// Walks the folders of `items` (files in `items` are taken as they are).
pub fn collect_files(items: &[PathBuf], f: &Filters, recursive: bool, hidden: bool, cancel: &AtomicBool) -> (Vec<PathBuf>, Vec<String>) {
    let (mut out, mut errs) = (vec![], vec![]);
    for item in items {
        if item.is_file() {
            out.push(item.clone());
            continue;
        }
        let mut stack = vec![item.clone()];
        while let Some(dir) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return (out, errs);
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    errs.push(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !hidden && name.starts_with('.') {
                    continue;
                }
                let Ok(ty) = e.file_type() else { continue };
                if ty.is_dir() {
                    if recursive && !f.exclude_dirs.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                        stack.push(e.path());
                    }
                } else if ty.is_file() && f.include.as_ref().map(|re| re.is_match(&name)).unwrap_or(true) && !f.exclude.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                    out.push(e.path());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    (out, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_subfolders_hidden_and_explicit_files() {
        let d = std::env::temp_dir().join(format!("src-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::create_dir_all(d.join("node_modules")).unwrap();
        for f in ["a.txt", "b.log", ".oculto.txt", "sub/c.txt", "node_modules/x.txt"] {
            fs::write(d.join(f), "x").unwrap();
        }
        let mut s = Sources::new("t", vec![d.clone()]);
        let names = |s: &Sources| s.collect(&AtomicBool::new(false)).0.iter().map(|p| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
        assert_eq!(names(&s), ["a.txt", "b.log", "sub/c.txt"], "hidden files and skipped folders left out");
        s.include = "*.txt".into();
        assert_eq!(names(&s), ["a.txt", "sub/c.txt"]);
        s.exclude = "c.*".into();
        assert_eq!(names(&s), ["a.txt"]);
        s.recursive = false;
        s.include.clear();
        s.exclude.clear();
        assert_eq!(names(&s), ["a.txt", "b.log"], "no subfolders");
        s.hidden = true;
        assert!(names(&s).contains(&".oculto.txt".to_string()));
        // The same filters as regular expressions.
        s.regex = true;
        s.recursive = true;
        s.hidden = false;
        s.include = r"^[ab]\.(txt|log)$".into();
        s.exclude = r"^b\.".into();
        assert_eq!(names(&s), ["a.txt"]);
        assert!(s.filters().is_ok());
        s.include = "(".into();
        assert!(s.filters().is_err(), "a broken expression is reported");
        s.regex = false;
        s.include.clear();
        s.exclude.clear();
        // A file added by hand ignores the filters.
        s.exclude = "*.log".into();
        s.add([d.join("b.log")]);
        assert!(names(&s).contains(&"b.log".to_string()));
        fs::remove_dir_all(&d).unwrap();
    }
}
