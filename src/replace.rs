//! Find and replace text inside many files at once: plain text or regular expression (with
//! $1 groups), case and whole-word options, files and folders from a list with "Examinar…",
//! include / exclude filters and subfolders. A preview shows every file with its changed lines
//! (before and after); tick the files to change and each one keeps a .bak copy.

use crate::fsops;
use crate::grep::highlighted;
use crate::i18n::tr;
use crate::sources::Sources;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, FontId, RichText};
use regex::{Regex, RegexBuilder};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

const MAX_HITS_PER_FILE: usize = 100;

#[derive(Clone)]
struct Opts {
    find: String,
    replace: String,
    regex: bool,
    case: bool,
    word: bool,
    backup: bool,
    max_bytes: u64,
}

fn build(o: &Opts) -> Result<Regex, String> {
    if o.find.is_empty() {
        return Err(tr("Escribe el texto a buscar.").to_string());
    }
    let mut p = if o.regex { o.find.clone() } else { regex::escape(&o.find) };
    if o.word {
        p = format!(r"\b(?:{p})\b");
    }
    RegexBuilder::new(&p).case_insensitive(!o.case).multi_line(true).build().map_err(|e| trf!("Expresión no válida: {}", e.to_string().lines().last().unwrap_or("").trim()))
}

/// The replacement text: literal unless regex mode, where $1 and ${name} work.
fn replacement(o: &Opts) -> String {
    if o.regex {
        o.replace.clone()
    } else {
        o.replace.replace('$', "$$")
    }
}

#[derive(Clone)]
struct Hit {
    line: usize,
    old: String,
    new: String,
    ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Default)]
struct FileRes {
    path: PathBuf,
    count: usize,
    hits: Vec<Hit>,
    on: bool,
    changed: bool,
    note: Option<String>,
    error: Option<String>,
}

fn clip(s: &str, ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    const MAX: usize = 240;
    if s.len() <= MAX {
        return (s.to_string(), ranges.to_vec());
    }
    let mut start = ranges.first().map(|r| r.0).unwrap_or(0).saturating_sub(60);
    while start > 0 && !s.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + MAX).min(s.len());
    while end < s.len() && !s.is_char_boundary(end) {
        end -= 1;
    }
    let shifted = ranges.iter().filter(|(a, b)| *a >= start && *b <= end).map(|(a, b)| (a - start + 1, b - start + 1)).collect();
    (format!("…{}", &s[start..end]), shifted)
}

fn read_text(path: &Path, max: u64) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > max {
        return Err(tr("demasiado grande").to_string());
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return Err(tr("parece un archivo binario").to_string());
    }
    String::from_utf8(bytes).map_err(|_| tr("no es UTF-8 válido").to_string())
}

/// Looks for matches (and what they would become) without touching the file.
fn preview(path: &Path, re: &Regex, o: &Opts) -> FileRes {
    let mut r = FileRes { path: path.to_path_buf(), on: true, ..Default::default() };
    let text = match read_text(path, o.max_bytes) {
        Ok(t) => t,
        Err(e) => {
            r.note = Some(e);
            r.on = false;
            return r;
        }
    };
    r.count = re.find_iter(&text).count();
    if r.count == 0 {
        r.on = false;
        return r;
    }
    let rep = replacement(o);
    for (i, line) in text.lines().enumerate() {
        if r.hits.len() >= MAX_HITS_PER_FILE {
            break;
        }
        let ranges: Vec<(usize, usize)> = re.find_iter(line).filter(|m| m.end() > m.start()).map(|m| (m.start(), m.end())).collect();
        if ranges.is_empty() {
            continue;
        }
        let (old, ranges) = clip(line, &ranges);
        let new = re.replace_all(line, rep.as_str()).to_string();
        let new = clip(&new, &[]).0;
        r.hits.push(Hit { line: i + 1, old, new, ranges });
    }
    r
}

/// Replaces in the file (a temp file, then rename) after keeping a .bak copy if asked.
fn apply(path: &Path, re: &Regex, o: &Opts) -> FileRes {
    let mut r = FileRes { path: path.to_path_buf(), ..Default::default() };
    let result = (|| -> Result<(), String> {
        let text = read_text(path, o.max_bytes)?;
        r.count = re.find_iter(&text).count();
        let new_text = re.replace_all(&text, replacement(o).as_str()).to_string();
        if new_text == text {
            return Ok(());
        }
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
        })?;
        r.changed = true;
        Ok(())
    })();
    if let Err(e) = result {
        r.error = Some(e);
    }
    r
}

struct Job {
    rx: Receiver<(Vec<FileRes>, usize, bool)>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
}

pub struct Replacer {
    src: Sources,
    o: Opts,
    max_mb: u32,
    job: Option<Job>,
    results: Vec<FileRes>,
    scanned: usize,
    applied: bool,
    filter: String,
    collapsed: HashSet<usize>,
    rev: u64,
    rows_cache: Option<((u64, String, usize), Vec<(usize, Option<usize>)>)>,
    status: Option<(String, bool)>,
    problems: Vec<String>,
}

impl Replacer {
    pub fn new(targets: Vec<PathBuf>) -> Replacer {
        let mut src = Sources::new("replace_src", targets);
        src.include = "*.txt; *.md; *.csv; *.json; *.xml; *.html; *.css; *.js; *.ts; *.py; *.rs; *.c; *.h; *.cpp; *.java; *.yml; *.yaml; *.toml; *.ini; *.log; *.sh; *.sql".into();
        Replacer {
            src,
            o: Opts { find: String::new(), replace: String::new(), regex: false, case: false, word: false, backup: true, max_bytes: 20 << 20 },
            max_mb: 20,
            job: None,
            results: vec![],
            scanned: 0,
            applied: false,
            filter: String::new(),
            collapsed: HashSet::new(),
            rev: 0,
            rows_cache: None,
            status: None,
            problems: vec![],
        }
    }

    /// Previews a search in the folder, for trying the tool (`--replace`).
    pub fn demo(&mut self, dir: &Path) {
        self.src.items = vec![dir.to_path_buf()];
        self.o.find = "mundo".into();
        self.o.replace = "planeta".into();
        self.start(false);
    }

    fn start(&mut self, do_apply: bool) {
        if self.src.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas con texto.").to_string(), true));
            return;
        }
        self.o.max_bytes = self.max_mb as u64 * 1024 * 1024;
        let re = match build(&self.o) {
            Ok(r) => r,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let filters = match self.src.filters() {
            Ok(f) => f,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let (items, recursive, hidden) = (self.src.items.clone(), self.src.recursive, self.src.hidden);
        let o = self.o.clone();
        // When applying, only the files ticked in the preview are changed.
        let chosen: Option<HashSet<PathBuf>> = do_apply.then(|| self.results.iter().filter(|r| r.on && r.count > 0).map(|r| r.path.clone()).collect());
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (d2, t2, c2) = (done.clone(), total.clone(), cancel.clone());
        std::thread::spawn(move || {
            let files: Vec<PathBuf> = match &chosen {
                Some(set) => {
                    let mut v: Vec<PathBuf> = set.iter().cloned().collect();
                    v.sort();
                    v
                }
                None => crate::sources::collect_files(&items, &filters, recursive, hidden, &c2).0,
            };
            t2.store(files.len(), Ordering::Relaxed);
            let mut out = Vec::new();
            for f in &files {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                out.push(if do_apply { apply(f, &re, &o) } else { preview(f, &re, &o) });
                d2.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send((out, files.len(), do_apply));
        });
        self.results.clear();
        self.collapsed.clear();
        self.rev += 1;
        self.status = None;
        self.problems.clear();
        self.job = Some(Job { rx, done, total, cancel });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok((results, scanned, applied)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        self.applied = applied;
        self.scanned = scanned;
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        self.problems = results.iter().filter_map(|r| r.error.as_ref().map(|e| format!("{}: {e}", r.path.display()))).collect();
        if applied {
            events.push(ToolEvent::Reload);
            self.status = Some((trf!("Listo: {} archivo(s) modificados · {} error(es)", results.iter().filter(|r| r.changed).count(), bad), bad > 0 || cancelled));
        } else {
            let with = results.iter().filter(|r| r.count > 0).count();
            let total: usize = results.iter().map(|r| r.count).sum();
            self.status = Some((trf!("{} coincidencia(s) en {} archivo(s) de {}", total, with, scanned), cancelled));
        }
        // After applying, the list keeps what was done; a preview shows only files with matches.
        self.results = if applied { results } else { results.into_iter().filter(|r| r.count > 0).collect() };
        self.rev += 1;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            self.src.ui(ui, selection, cwd, tr("ARCHIVOS DE TEXTO"));
            ui.separator();
            egui::Grid::new("replace_fields").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                ui.label(tr("Buscar"));
                let r = ui.add(egui::TextEdit::singleline(&mut self.o.find).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.start(false);
                }
                ui.end_row();
                ui.label(tr("Reemplazar por"));
                ui.add(egui::TextEdit::singleline(&mut self.o.replace).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace)).on_hover_text(tr("En modo expresión regular puedes usar $1, $2… para los grupos."));
                ui.end_row();
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.o.regex, tr("Expresión regular"));
                ui.checkbox(&mut self.o.case, tr("Distinguir mayúsculas"));
                ui.checkbox(&mut self.o.word, tr("Palabra completa"));
                ui.checkbox(&mut self.o.backup, tr("Guardar una copia .bak del original"));
                ui.label(tr("Tamaño máximo"));
                ui.add(egui::DragValue::new(&mut self.max_mb).range(1..=2048).suffix(" MB"));
            });
        });
        ui.separator();
        let ticked = self.results.iter().filter(|r| r.on && r.count > 0).count();
        ui.horizontal(|ui| {
            let can = !busy && !self.src.items.is_empty();
            if ui.add_enabled(can, egui::Button::new(RichText::new(tr("🔍 Vista previa")).strong())).clicked() {
                self.start(false);
            }
            let ready = can && !self.applied && ticked > 0;
            let btn = egui::Button::new(RichText::new(trf!("✏ Reemplazar en {} archivo(s)", ticked)).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(ready, btn).on_hover_text(tr("Primero haz la vista previa; se cambian solo los archivos marcados.")).clicked() {
                self.start(true);
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let (d, t) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed));
                if t > 0 {
                    ui.add(egui::ProgressBar::new(d as f32 / t as f32).desired_width(160.0).show_percentage());
                }
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if !self.results.is_empty() && !busy {
            self.results_ui(ui, events);
        }
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in self.problems.iter().take(50) {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar por ruta")).desired_width(180.0));
            if !self.applied {
                if ui.small_button(tr("Marcar todos")).clicked() {
                    self.results.iter_mut().for_each(|r| r.on = true);
                }
                if ui.small_button(tr("Desmarcar todos")).clicked() {
                    self.results.iter_mut().for_each(|r| r.on = false);
                }
            }
            if ui.small_button(tr("Contraer todo")).clicked() {
                self.collapsed = (0..self.results.len()).collect();
                self.rev += 1;
            }
            if ui.small_button(tr("Expandir todo")).clicked() {
                self.collapsed.clear();
                self.rev += 1;
            }
        });
        let key = (self.rev, self.filter.clone(), self.results.len());
        if self.rows_cache.as_ref().map(|c| c.0 != key).unwrap_or(true) {
            let q = self.filter.to_lowercase();
            let mut rows = Vec::with_capacity(self.results.len() * 3);
            for (fi, f) in self.results.iter().enumerate() {
                if !q.is_empty() && !f.path.to_string_lossy().to_lowercase().contains(&q) {
                    continue;
                }
                rows.push((fi, None));
                if !self.collapsed.contains(&fi) {
                    rows.extend((0..f.hits.len()).map(|h| (fi, Some(h))));
                }
            }
            self.rows_cache = Some((key, rows));
        }
        let (rows_key, rows) = self.rows_cache.take().unwrap_or_default();
        let font = FontId::monospace(13.0);
        let roots = self.src.items.clone();
        let (mut toggle, mut folder) = (None, None);
        let applied = self.applied;
        egui::ScrollArea::vertical().id_salt("replace_rows").max_height(crate::tools::grow(ui, 340.0)).auto_shrink([false, false]).show_rows(ui, 24.0, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &(fi, hi) in &rows[range] {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::click());
                let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(4.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                let f = &mut self.results[fi];
                match hi {
                    None => {
                        ui.painter().rect_filled(rect.shrink2(egui::vec2(0.0, 1.0)), 4.0, if resp.hovered() { theme::p().hover } else { theme::p().stripe });
                        if !applied {
                            row.checkbox(&mut f.on, "");
                        }
                        row.label(RichText::new(if self.collapsed.contains(&fi) { "▸" } else { "▾" }).color(theme::p().sub));
                        let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        row.label(RichText::new(format!("🗋 {name}")).strong());
                        row.label(RichText::new(trf!("{} coincidencias", f.count)).small().color(theme::accent()));
                        match (&f.error, f.changed, applied) {
                            (Some(e), _, _) => {
                                row.label(RichText::new(format!("✖ {e}")).small().color(theme::p().danger));
                            }
                            (_, true, true) => {
                                row.label(RichText::new(tr("✔ modificado")).small().color(theme::p().ok));
                            }
                            (_, false, true) => {
                                row.label(RichText::new(tr("sin cambios")).small().color(theme::p().muted));
                            }
                            _ => {}
                        }
                        let rel = roots.iter().find_map(|r| f.path.parent().and_then(|p| p.strip_prefix(r).ok())).map(|p| p.to_string_lossy().to_string()).filter(|p| !p.is_empty());
                        row.add(egui::Label::new(RichText::new(rel.unwrap_or_else(|| f.path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())).small().color(theme::p().muted)).truncate());
                        if resp.clicked() {
                            toggle = Some(fi);
                        }
                        resp.context_menu(|ui| {
                            if ui.button(tr("Ir a la carpeta")).clicked() {
                                folder = f.path.parent().map(Path::to_path_buf);
                                ui.close_menu();
                            }
                        });
                    }
                    Some(h) => {
                        let hit = &f.hits[h];
                        if h % 2 == 1 {
                            ui.painter().rect_filled(rect, 0.0, theme::p().stripe.gamma_multiply(0.5));
                        }
                        let gutter = egui::Rect::from_min_size(rect.min + egui::vec2(18.0, 0.0), egui::vec2(52.0, rect.height()));
                        ui.painter().rect_filled(gutter, 0.0, theme::p().hover.gamma_multiply(0.6));
                        ui.painter().text(gutter.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, hit.line.to_string(), FontId::monospace(12.0), theme::p().sub);
                        let text_rect = egui::Rect::from_min_max(egui::pos2(gutter.right() + 8.0, rect.top()), rect.right_bottom());
                        let mut line_ui = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                        line_ui.add(egui::Label::new(highlighted(&hit.old, &hit.ranges, font.clone(), theme::p().text, Color32::BLACK, Color32::from_rgb(255, 150, 130))).truncate());
                        line_ui.label(RichText::new("»").color(theme::p().sub));
                        line_ui.add(egui::Label::new(RichText::new(&hit.new).font(font.clone()).color(theme::p().ok)).truncate());
                    }
                }
            }
        });
        self.rows_cache = Some((rows_key, rows));
        if let Some(fi) = toggle {
            if !self.collapsed.remove(&fi) {
                self.collapsed.insert(fi);
            }
            self.rev += 1;
        }
        if let Some(p) = folder {
            events.push(ToolEvent::Navigate(p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(find: &str, rep: &str) -> Opts {
        Opts { find: find.into(), replace: rep.into(), regex: false, case: false, word: false, backup: true, max_bytes: 1 << 20 }
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
    fn preview_shows_before_and_after_then_applies_with_backup() {
        let d = std::env::temp_dir().join(format!("rp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), "hola mundo\nnada\nmundo\n").unwrap();
        fs::write(d.join("b.txt"), "nada").unwrap();
        fs::write(d.join("c.txt"), [0u8, 1, 2]).unwrap();
        let o = opts("mundo", "planeta");
        let re = build(&o).unwrap();
        let pre = preview(&d.join("a.txt"), &re, &o);
        assert_eq!(pre.count, 2);
        assert_eq!((pre.hits.len(), pre.hits[0].line, pre.hits[0].new.as_str(), pre.hits[0].ranges.clone()), (2, 1, "hola planeta", vec![(5, 10)]));
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "hola mundo\nnada\nmundo\n", "a preview changes nothing");
        assert_eq!(preview(&d.join("b.txt"), &re, &o).count, 0);
        assert!(preview(&d.join("c.txt"), &re, &o).note.is_some());
        let done = apply(&d.join("a.txt"), &re, &o);
        assert!(done.changed);
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "hola planeta\nnada\nplaneta\n");
        assert_eq!(fs::read_to_string(d.join("a.txt.bak")).unwrap(), "hola mundo\nnada\nmundo\n");
        fs::remove_dir_all(&d).unwrap();
    }
}
