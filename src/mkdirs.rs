//! Create many folders at once from a list: one name per line, indentation makes subfolders.
//! Includes templates (numbered folders, months, a project skeleton) and a preview.

use crate::i18n::tr;
use crate::roots::DestField;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use std::path::{Path, PathBuf};

/// Turns the indented text into relative folder paths, in order. Errors say the line.
fn parse(text: &str, prefix_counter: bool) -> Result<Vec<PathBuf>, String> {
    let mut stack: Vec<(usize, String)> = vec![];
    let mut out = vec![];
    let mut n = 0usize;
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let indent = raw.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
        let mut name = raw.trim().to_string();
        if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '\0')) || name == "." || name == ".." {
            return Err(trf!("Línea {}: el nombre «{}» no es válido", i + 1, name));
        }
        // "a/b/c" creates the whole chain.
        let parts: Vec<String> = name.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();
        if parts.iter().any(|p| p == ".." || p == ".") {
            return Err(trf!("Línea {}: el nombre «{}» no es válido", i + 1, name));
        }
        while stack.last().map(|(d, _)| *d >= indent).unwrap_or(false) {
            stack.pop();
        }
        if prefix_counter && stack.is_empty() {
            n += 1;
            name = format!("{:02} {}", n, parts.join("/"));
        } else {
            name = parts.join("/");
        }
        let mut path = PathBuf::new();
        for (_, s) in &stack {
            path.push(s);
        }
        path.push(&name);
        out.push(path.clone());
        stack.push((indent, name));
    }
    if out.is_empty() {
        return Err(tr("La lista está vacía.").to_string());
    }
    Ok(out)
}

fn create(base: &Path, rel: &[PathBuf]) -> (usize, usize, Vec<String>) {
    let (mut made, mut existed, mut errors) = (0, 0, vec![]);
    for r in rel {
        let p = base.join(r);
        if p.is_dir() {
            existed += 1;
        } else {
            match std::fs::create_dir_all(&p) {
                Ok(()) => made += 1,
                Err(e) => errors.push(format!("{}: {}", r.display(), e)),
            }
        }
    }
    (made, existed, errors)
}

const TEMPLATES: [(&str, &str); 5] = [
    ("Meses del año", "Enero\nFebrero\nMarzo\nAbril\nMayo\nJunio\nJulio\nAgosto\nSeptiembre\nOctubre\nNoviembre\nDiciembre"),
    ("Proyecto", "Documentos\n  Contratos\n  Facturas\nImágenes\n  Originales\n  Editadas\nEntrega\nNotas"),
    ("Años y trimestres", "2024\n  T1\n  T2\n  T3\n  T4\n2025\n  T1\n  T2\n  T3\n  T4"),
    ("Del 01 al 10", "01\n02\n03\n04\n05\n06\n07\n08\n09\n10"),
    ("Medios", "Fotos\nVídeos\nMúsica\nDocumentos\nDescargas"),
];

pub struct MakeFolders {
    text: String,
    dest: DestField,
    number: bool,
    status: Option<(String, bool)>,
    last: Vec<PathBuf>,
}

impl MakeFolders {
    pub fn new(base: PathBuf) -> MakeFolders {
        let mut dest = DestField::new("mkdirs_dest");
        dest.text = base.to_string_lossy().to_string();
        MakeFolders { text: String::new(), dest, number: false, status: None, last: vec![] }
    }

    /// Fills a template and creates it in the given folder, for trying the tool (`--mkdirs`).
    pub fn demo(&mut self, dir: &Path) {
        self.text = TEMPLATES[1].1.to_string();
        self.dest.text = dir.join("Nuevo proyecto").to_string_lossy().to_string();
        self.run();
    }

    fn run(&mut self) {
        let base = self.dest.path();
        if self.dest.text.trim().is_empty() {
            self.status = Some((tr("Elige dónde crear las carpetas.").to_string(), true));
            return;
        }
        match parse(&self.text, self.number) {
            Err(e) => self.status = Some((e, true)),
            Ok(rel) => {
                let (made, existed, errors) = create(&base, &rel);
                self.status = Some(if errors.is_empty() {
                    (trf!("Listo: {} carpeta(s) creadas, {} ya existían.", made, existed), false)
                } else {
                    (trf!("{} error(es): {}", errors.len(), errors[0]), true)
                });
                self.last = rel;
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("DÓNDE CREARLAS")).small().strong().color(theme::p().sub));
        self.dest.ui(ui, cwd, selection);
        ui.separator();
        ui.label(RichText::new(tr("LISTA DE CARPETAS")).small().strong().color(theme::p().sub));
        ui.label(RichText::new(tr("Una por línea. Empuja con espacios o tabulador para hacer una subcarpeta; «a/b/c» crea toda la cadena.")).small().color(theme::p().muted));
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Plantillas:"));
            for (name, body) in TEMPLATES {
                if ui.small_button(tr(name)).clicked() {
                    self.text = body.to_string();
                }
            }
        });
        ui.add(egui::TextEdit::multiline(&mut self.text).desired_rows(10).desired_width(f32::INFINITY).code_editor());
        ui.checkbox(&mut self.number, tr("Numerar las carpetas principales (01, 02…)"));
        ui.separator();
        let parsed = parse(&self.text, self.number);
        match &parsed {
            Ok(rel) => {
                ui.label(RichText::new(trf!("Vista previa: {} carpeta(s)", rel.len())).small().strong().color(theme::p().sub));
                let base = self.dest.path();
                egui::ScrollArea::vertical().id_salt("mk_prev").max_height(crate::tools::grow(ui, 160.0)).show(ui, |ui| {
                    for r in rel.iter().take(300) {
                        let depth = r.components().count().saturating_sub(1);
                        let exists = base.join(r).is_dir();
                        let name = r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let t = format!("{}📁 {}{}", "    ".repeat(depth), name, if exists { format!("  ({})", tr("ya existe")) } else { String::new() });
                        ui.label(RichText::new(t).color(if exists { theme::p().muted } else { theme::p().text }));
                    }
                });
            }
            Err(e) if !self.text.trim().is_empty() => {
                ui.label(RichText::new(e).color(theme::p().danger));
            }
            _ => {}
        }
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("📁 Crear carpetas")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(parsed.is_ok(), btn).clicked() {
                self.run();
                events.push(ToolEvent::Reload);
            }
            if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_chains_numbering_and_bad_names() {
        let r = parse("A\n  B\n    C\n  D\nE\nx/y", false).unwrap();
        let s: Vec<String> = r.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(s, ["A", "A/B", "A/B/C", "A/D", "E", "x/y"]);
        let n = parse("Uno\n  sub\nDos", true).unwrap();
        assert_eq!(n[0], PathBuf::from("01 Uno"));
        assert_eq!(n[1], PathBuf::from("01 Uno").join("sub"));
        assert_eq!(n[2], PathBuf::from("02 Dos"));
        assert!(parse("a:b", false).is_err() && parse("../x", false).is_err() && parse("  \n", false).is_err());
    }

    #[test]
    fn creates_once_and_counts_existing() {
        let d = std::env::temp_dir().join(format!("mk-{}", std::process::id()));
        let rel = parse("a\n  b\nc", false).unwrap();
        assert_eq!(create(&d, &rel).0, 3);
        let (made, existed, err) = create(&d, &rel);
        assert_eq!((made, existed, err.len()), (0, 3, 0));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
