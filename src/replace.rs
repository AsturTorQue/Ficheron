//! Find and replace text inside many files at once: plain text or regular expression (with
//! $1 groups), case and whole-word options, a preview of every file's matches before
//! touching anything, and a .bak copy of each changed file.

use crate::fsops;
use crate::i18n::tr;
use crate::roots::{picker_window, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone)]
struct Opts {
    find: String,
    replace: String,
    regex: bool,
    case: bool,
    word: bool,
    backup: bool,
    include: String,
    recursive: bool,
}

fn build(o: &Opts) -> Result<Regex, String> {
    if o.find.is_empty() {
        return Err(tr("Escribe el texto a buscar.").to_string());
    }
    let mut p = if o.regex { o.find.clone() } else { regex::escape(&o.find) };
    if o.word {
        p = format!(r"\b(?:{p})\b");
    }
    RegexBuilder::new(&p).case_insensitive(!o.case).multi_line(true).build().map_err(|e| trf!("Expresión no válida: {}", e))
}

/// The replacement text: literal unless regex mode, where $1 and ${name} work.
fn replacement(o: &Opts) -> String {
    if o.regex {
        o.replace.clone()
    } else {
        o.replace.replace('$', "$$")
    }
}

fn collect(items: &[PathBuf], o: &Opts) -> Vec<PathBuf> {
    let pats = crate::rename::name_matcher(&o.include, false).ok().flatten();
    let mut out = vec![];
    for p in items {
        if p.is_file() {
            out.push(p.clone());
        } else if p.is_dir() {
            let mut stack = vec![p.clone()];
            while let Some(d) = stack.pop() {
                let mut kids: Vec<PathBuf> = fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
                kids.sort();
                for k in kids {
                    let name = k.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if k.is_dir() {
                        if o.recursive && !name.starts_with('.') {
                            stack.push(k);
                        }
                    } else if k.is_file() && pats.as_ref().map(|re| re.is_match(&name)).unwrap_or(true) {
                        out.push(k);
                    }
                }
            }
        }
    }
    out.dedup();
    out
}

#[derive(Clone, Default)]
struct Res {
    path: PathBuf,
    count: usize,
    sample: String,
    changed: bool,
    note: Option<String>,
    error: Option<String>,
}

fn process(path: &Path, re: &Regex, o: &Opts, apply: bool) -> Res {
    let mut r = Res { path: path.to_path_buf(), ..Default::default() };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            r.error = Some(e.to_string());
            return r;
        }
    };
    if bytes.iter().take(8000).any(|b| *b == 0) {
        r.note = Some(tr("parece un archivo binario").to_string());
        return r;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        r.note = Some(tr("no es UTF-8 válido").to_string());
        return r;
    };
    r.count = re.find_iter(&text).count();
    if let Some(m) = re.find(&text) {
        let line_start = text[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = text[m.end()..].find('\n').map(|i| m.end() + i).unwrap_or(text.len());
        r.sample = text[line_start..line_end].trim().chars().take(120).collect();
    }
    if r.count == 0 || !apply {
        return r;
    }
    let new_text = re.replace_all(&text, replacement(o).as_str()).to_string();
    if new_text == text {
        return r;
    }
    let result = (|| -> Result<(), String> {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if o.backup {
            fs::copy(path, fsops::unique_name(&dir, &format!("{name}.bak"))).map_err(|e| e.to_string())?;
        }
        let tmp = dir.join(format!(".{name}.part"));
        fs::write(&tmp, new_text.as_bytes()).map_err(|e| e.to_string())?;
        if let Ok(m) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, m.permissions());
        }
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })
    })();
    match result {
        Ok(()) => r.changed = true,
        Err(e) => r.error = Some(e),
    }
    r
}

pub struct Replacer {
    items: Vec<PathBuf>,
    picker: Option<Picker>,
    o: Opts,
    rx: Option<Receiver<(Vec<Res>, bool)>>,
    results: Vec<Res>,
    applied: bool,
    status: Option<(String, bool)>,
}

impl Replacer {
    pub fn new(targets: Vec<PathBuf>) -> Replacer {
        Replacer {
            items: targets.into_iter().filter(|p| p.exists()).collect(),
            picker: None,
            o: Opts {
                find: String::new(),
                replace: String::new(),
                regex: false,
                case: false,
                word: false,
                backup: true,
                include: "*.txt; *.md; *.csv; *.json; *.xml; *.html; *.css; *.js; *.ts; *.py; *.rs; *.c; *.h; *.cpp; *.java; *.yml; *.yaml; *.toml; *.ini; *.log; *.sh; *.sql".into(),
                recursive: true,
            },
            rx: None,
            results: vec![],
            applied: false,
            status: None,
        }
    }

    /// Previews a search in the folder, for trying the tool (`--replace`).
    pub fn demo(&mut self, dir: &Path) {
        self.items = vec![dir.to_path_buf()];
        self.o.find = "mundo".into();
        self.o.replace = "planeta".into();
        self.start(false);
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.exists() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn start(&mut self, apply: bool) {
        if self.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas con texto.").to_string(), true));
            return;
        }
        let re = match build(&self.o) {
            Ok(r) => r,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let files = collect(&self.items, &self.o);
        if files.is_empty() {
            self.status = Some((tr("No hay archivos de texto en lo que has añadido.").to_string(), true));
            return;
        }
        let o = self.o.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let v: Vec<Res> = files.iter().map(|f| process(f, &re, &o, apply)).collect();
            let _ = tx.send((v, apply));
        });
        self.results.clear();
        self.status = None;
        self.rx = Some(rx);
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(rx) = &self.rx else { return };
        let Ok((results, applied)) = rx.try_recv() else { return };
        self.rx = None;
        self.applied = applied;
        let with: Vec<&Res> = results.iter().filter(|r| r.count > 0).collect();
        let total: usize = with.iter().map(|r| r.count).sum();
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        self.status = Some(if applied {
            events.push(ToolEvent::Reload);
            (trf!("Listo: {} archivo(s) modificados · {} error(es)", results.iter().filter(|r| r.changed).count(), bad), bad > 0)
        } else {
            (trf!("{} coincidencia(s) en {} archivo(s) de {}", total, with.len(), results.len()), false)
        });
        self.results = results;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);
        let busy = self.rx.is_some();
        egui::ScrollArea::vertical().id_salt("replace_all").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.label(RichText::new(tr("ARCHIVOS DE TEXTO")).small().strong().color(theme::p().sub));
                let mut remove = None;
                for (k, p) in self.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✖").clicked() {
                            remove = Some(k);
                        }
                        ui.add(egui::Label::new(p.to_string_lossy()).truncate());
                    });
                }
                if let Some(k) = remove {
                    self.items.remove(k);
                }
                ui.horizontal_wrapped(|ui| {
                    let sel = selection.to_vec();
                    if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).clicked() {
                        self.add(sel);
                    }
                    if ui.button(tr("📂 Elegir archivos…")).clicked() {
                        self.picker = Some(Picker::new(cwd.to_path_buf()));
                    }
                    ui.checkbox(&mut self.o.recursive, tr("Incluir subcarpetas"));
                });
                ui.horizontal(|ui| {
                    ui.add_sized([150.0, 18.0], egui::Label::new(tr("De las carpetas, solo")));
                    ui.add(egui::TextEdit::singleline(&mut self.o.include).desired_width(520.0));
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add_sized([150.0, 18.0], egui::Label::new(tr("Buscar")));
                    ui.add(egui::TextEdit::singleline(&mut self.o.find).desired_width(420.0));
                });
                ui.horizontal(|ui| {
                    ui.add_sized([150.0, 18.0], egui::Label::new(tr("Reemplazar por")));
                    ui.add(egui::TextEdit::singleline(&mut self.o.replace).desired_width(420.0)).on_hover_text(tr("En modo expresión regular puedes usar $1, $2… para los grupos."));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(154.0);
                    ui.checkbox(&mut self.o.regex, tr("Expresión regular"));
                    ui.checkbox(&mut self.o.case, tr("Distinguir mayúsculas"));
                    ui.checkbox(&mut self.o.word, tr("Palabra completa"));
                    ui.checkbox(&mut self.o.backup, tr("Guardar una copia .bak del original"));
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                let ok = !busy && !self.items.is_empty();
                if ui.add_enabled(ok, egui::Button::new(RichText::new(tr("🔍 Vista previa")).strong())).clicked() {
                    self.start(false);
                }
                let btn = egui::Button::new(RichText::new(tr("✏ Reemplazar en los archivos")).color(Color32::WHITE).strong()).fill(theme::accent());
                if ui.add_enabled(ok, btn).clicked() {
                    self.start(true);
                }
                if busy {
                    ui.spinner();
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                } else if let Some((s, err)) = &self.status {
                    ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
                }
            });
            if !self.results.is_empty() {
                ui.separator();
                egui::Grid::new("replace_grid").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                    for h in ["Archivo", "Coincidencias", "Primera línea", "Estado"] {
                        ui.label(RichText::new(tr(h)).small().strong().color(theme::p().sub));
                    }
                    ui.end_row();
                    for r in self.results.iter().filter(|r| r.count > 0 || r.error.is_some()) {
                        let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        if ui.link(name).on_hover_text(r.path.to_string_lossy()).clicked() {
                            if let Some(p) = r.path.parent() {
                                events.push(ToolEvent::Navigate(p.to_path_buf()));
                            }
                        }
                        ui.label(r.count.to_string());
                        ui.add(egui::Label::new(RichText::new(&r.sample).small().code()).truncate());
                        let (t, c) = match &r.error {
                            Some(e) => (format!("✖ {e}"), theme::p().danger),
                            None if r.changed => (tr("✔ modificado").to_string(), theme::p().ok),
                            None => (String::new(), theme::p().muted),
                        };
                        ui.label(RichText::new(t).small().color(c));
                        ui.end_row();
                    }
                });
            }
        });
        let ctx = ui.ctx().clone();
        let added = picker_window(&ctx, "replace_picker", &mut self.picker);
        self.add(added);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(find: &str, rep: &str) -> Opts {
        Opts { find: find.into(), replace: rep.into(), regex: false, case: false, word: false, backup: true, include: "*.txt".into(), recursive: true }
    }

    #[test]
    fn literal_regex_word_and_case() {
        let mut o = opts("a.b", "$x");
        let re = build(&o).unwrap();
        assert_eq!(re.replace_all("a.b axb", replacement(&o).as_str()), "$x axb", "dot and $ are literal");
        o.regex = true;
        o.find = r"(\w)\.(\w)".into();
        o.replace = "$2-$1".into();
        assert_eq!(build(&o).unwrap().replace_all("a.b", replacement(&o).as_str()), "b-a");
        let mut w = opts("casa", "X");
        w.word = true;
        assert_eq!(build(&w).unwrap().replace_all("Casa casas casa", "X"), "X casas X");
        w.case = true;
        assert_eq!(build(&w).unwrap().replace_all("Casa casa", "X"), "Casa X");
        assert!(build(&opts("", "")).is_err());
    }

    #[test]
    fn previews_then_replaces_with_backup_and_skips_binary() {
        let d = std::env::temp_dir().join(format!("rp-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), "hola mundo\nmundo\n").unwrap();
        fs::write(d.join("b.txt"), "nada").unwrap();
        fs::write(d.join("c.txt"), [0u8, 1, 2]).unwrap();
        let o = opts("mundo", "planeta");
        let re = build(&o).unwrap();
        let pre = process(&d.join("a.txt"), &re, &o, false);
        assert_eq!(pre.count, 2);
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "hola mundo\nmundo\n");
        let done = process(&d.join("a.txt"), &re, &o, true);
        assert!(done.changed);
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "hola planeta\nplaneta\n");
        assert_eq!(fs::read_to_string(d.join("a.txt.bak")).unwrap(), "hola mundo\nmundo\n");
        assert_eq!(process(&d.join("b.txt"), &re, &o, true).count, 0);
        assert!(process(&d.join("c.txt"), &re, &o, true).note.is_some());
        fs::remove_dir_all(&d).unwrap();
    }
}
