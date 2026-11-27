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
    const MAX: usize = 2000;
    if s.len() <= MAX {
        return (s.to_string(), ranges.to_vec());
    }
    let mut start = ranges.first().map(|r| r.0).unwrap_or(0).saturating_sub(400);
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

/// Only counts the matches: the list of files is found this way, and the lines of each file
/// are worked out when it is chosen (`preview`).
fn scan(path: &Path, re: &Regex, o: &Opts) -> FileRes {
    let mut r = FileRes { path: path.to_path_buf(), ..Default::default() };
    match read_text(path, o.max_bytes) {
        Ok(t) => {
            r.count = re.find_iter(&t).count();
            r.on = r.count > 0;
        }
        Err(e) => r.note = Some(e),
    }
    r
}

/// Which file and which texts a preview was made for.
type DetailKey = (PathBuf, String, String, bool, bool, bool);

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
    /// The chosen change: (file, line among its matches).
    cur: Option<(usize, usize)>,
    scroll_to: bool,
    fold_form: Option<bool>,
    kept: Option<(PathBuf, usize)>,
    /// The chosen file's lines, before and after, made when it is chosen or the texts change.
    detail: Option<(DetailKey, FileRes)>,
    /// The search the file list was made with (text, regex, case, word).
    scanned_with: Option<(String, bool, bool, bool)>,
    /// Asking to confirm the replacement.
    confirm: bool,
    status: Option<(String, bool)>,
    problems: Vec<String>,
}

impl Replacer {
    pub fn new(targets: Vec<PathBuf>) -> Replacer {
        let mut src = Sources::new("replace_src", targets);
        src.include = "*.txt, *.md, *.csv, *.json, *.xml, *.html, *.css, *.js, *.ts, *.py, *.rs, *.c, *.h, *.cpp, *.java, *.yml, *.yaml, *.toml, *.ini, *.log, *.sh, *.sql".into();
        Replacer {
            src,
            o: Opts { find: String::new(), replace: String::new(), regex: false, case: false, word: false, backup: false, max_bytes: 20 << 20 },
            max_mb: 20,
            job: None,
            results: vec![],
            scanned: 0,
            applied: false,
            filter: String::new(),
            cur: None,
            scroll_to: false,
            fold_form: None,
            kept: None,
            detail: None,
            scanned_with: None,
            confirm: false,
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
                out.push(if do_apply { apply(f, &re, &o) } else { scan(f, &re, &o) });
                d2.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send((out, files.len(), do_apply));
        });
        let keep = self.cur.and_then(|(f, h)| self.results.get(f).map(|r| (r.path.clone(), h)));
        if !do_apply {
            self.scanned_with = Some(self.find_key());
        }
        self.confirm = false;
        self.detail = None;
        self.results.clear();
        self.kept = keep;
        self.cur = None;
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
        // Stay on the file that was being looked at.
        self.cur = self.kept.take().and_then(|(p, h)| self.results.iter().position(|r| r.path == p).map(|f| (f, h)));
        self.scroll_to = true;
        if !self.results.is_empty() && !applied {
            self.fold_form = Some(false);
        }
    }

    /// Starts from a search done in «Buscar texto en archivos»: those files and that text.
    pub fn prefill(&mut self, files: Vec<PathBuf>, find: String, regex: bool, case: bool, word: bool) {
        self.src.items = files;
        self.o.find = find;
        self.o.regex = regex;
        self.o.case = case;
        self.o.word = word;
        self.start(false);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let busy = self.job.is_some();
        // Like the text search: the form folds away when the preview arrives.
        let form_open = egui::CollapsingHeader::new(RichText::new(tr("DÓNDE Y QUÉ CAMBIAR")).small().strong().color(theme::p().sub))
            .id_salt("replace_form")
            .open(self.fold_form.take())
            .default_open(true)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    self.src.ui(ui, selection, cwd, tr("ARCHIVOS DE TEXTO"));
                    ui.separator();
                    self.fields_ui(ui, true);
                });
            })
            .body_returned
            .is_some();
        if !form_open {
            // Folded: the two texts stay at hand to adjust and preview again.
            ui.add_enabled_ui(!busy, |ui| self.fields_ui(ui, false));
        }
        ui.separator();
        let ticked = self.results.iter().filter(|r| r.on && r.count > 0).count();
        let stale = !self.results.is_empty() && !self.applied && self.scanned_with.as_ref() != Some(&self.find_key());
        ui.horizontal_wrapped(|ui| {
            let can = !busy && !self.src.items.is_empty();
            if ui.add_enabled(can, egui::Button::new(RichText::new(tr("🔍 Buscar archivos")).strong())).on_hover_text(tr("Busca qué archivos tienen el texto. El cambio de cada uno se ve al elegirlo en la lista.")).clicked() {
                self.start(false);
            }
            let ready = can && !self.applied && ticked > 0 && !stale;
            let btn = egui::Button::new(RichText::new(trf!("✏ Reemplazar en {} archivo(s)", ticked)).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(ready && !self.confirm, btn).on_hover_text(tr("Se cambian solo los archivos marcados. Pide confirmación antes.")).clicked() {
                self.confirm = true;
            }
            ui.checkbox(&mut self.o.backup, tr("Guardar copia .bak")).on_hover_text(tr("Deja junto a cada archivo cambiado una copia del original (nombre.bak). Desmarcado no se crea ningún archivo extra."));
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
        if stale {
            ui.label(RichText::new(tr("Has cambiado el texto a buscar: pulsa «Buscar archivos» para actualizar la lista antes de reemplazar.")).color(theme::p().danger));
            self.confirm = false;
        }
        if self.confirm {
            egui::Frame::new().fill(theme::p().danger.gamma_multiply(0.12)).stroke(egui::Stroke::new(1.0_f32, theme::p().danger)).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
                ui.label(RichText::new(trf!("⚠ Vas a cambiar {} archivo(s): «{}» por «{}».", ticked, self.o.find, self.o.replace)).strong());
                ui.label(if self.o.backup {
                    tr("Esta operación no se puede deshacer desde aquí. Se guarda una copia .bak de cada archivo, con la que podrías recuperarlo a mano.")
                } else {
                    tr("Esta operación no se puede deshacer y no se guardan copias .bak: los archivos quedarán cambiados.")
                });
                ui.checkbox(&mut self.o.backup, tr("Guardar copia .bak"));
                ui.horizontal(|ui| {
                    let yes = egui::Button::new(RichText::new(tr("Sí, reemplazar")).color(Color32::WHITE).strong()).fill(theme::p().danger);
                    if ui.add(yes).clicked() {
                        self.confirm = false;
                        self.start(true);
                    }
                    if ui.button(tr("Cancelar")).clicked() {
                        self.confirm = false;
                    }
                });
            });
        }
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in self.problems.iter().take(50) {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
        if !self.results.is_empty() && !busy {
            self.results_ui(ui, events);
        }
    }

    fn fields_ui(&mut self, ui: &mut egui::Ui, full: bool) {
        egui::Grid::new(("replace_fields", full)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            crate::ui_kit::label(ui, tr("Buscar"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.o.find).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.start(false);
            }
            ui.end_row();
            crate::ui_kit::label(ui, tr("Reemplazar por"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.o.replace).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace)).on_hover_text(tr("En modo expresión regular puedes usar $1, $2… para los grupos."));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.start(false);
            }
            ui.end_row();
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.o.regex, tr("Expresión regular"));
            ui.checkbox(&mut self.o.case, tr("Distinguir mayúsculas"));
            ui.checkbox(&mut self.o.word, tr("Palabra completa"));
            if full {
                ui.label(tr("Tamaño máximo"));
                ui.add(egui::DragValue::new(&mut self.max_mb).range(1..=2048).suffix(" MB"));
            }
        });
    }

    fn shown_files(&self) -> Vec<usize> {
        let q = self.filter.to_lowercase();
        (0..self.results.len()).filter(|&i| q.is_empty() || self.results[i].path.to_string_lossy().to_lowercase().contains(&q)).collect()
    }

    fn find_key(&self) -> (String, bool, bool, bool) {
        (self.o.find.clone(), self.o.regex, self.o.case, self.o.word)
    }

    /// Works out the chosen file's lines (only that file), again whenever the texts change.
    /// Returns how many lines it has.
    fn ensure_detail(&mut self, fi: usize) -> usize {
        let key: DetailKey = (self.results[fi].path.clone(), self.o.find.clone(), self.o.replace.clone(), self.o.regex, self.o.case, self.o.word);
        if self.detail.as_ref().map(|d| d.0 != key).unwrap_or(true) {
            self.o.max_bytes = self.max_mb as u64 * 1024 * 1024;
            let r = match build(&self.o) {
                Ok(re) => preview(&key.0, &re, &self.o),
                Err(e) => FileRes { note: Some(e), ..Default::default() },
            };
            self.detail = Some((key, r));
        }
        self.detail.as_ref().map(|d| d.1.hits.len()).unwrap_or(0)
    }

    /// Next or previous line; past the end of a file it goes on to the next (or previous) file.
    fn step(&mut self, files: &[usize], step: i64) {
        if files.is_empty() {
            return;
        }
        let (f, h) = match self.cur {
            Some(c) => c,
            None => {
                let f = if step < 0 { files[files.len() - 1] } else { files[0] };
                let n = self.ensure_detail(f);
                self.cur = Some((f, if step < 0 { n.saturating_sub(1) } else { 0 }));
                self.scroll_to = true;
                return;
            }
        };
        let n = self.ensure_detail(f) as i64;
        let nh = h as i64 + step;
        if nh >= 0 && nh < n {
            self.cur = Some((f, nh as usize));
        } else {
            let pos = files.iter().position(|&x| x == f).unwrap_or(0) as i64;
            let nf = files[(pos + step).rem_euclid(files.len() as i64) as usize];
            let len = self.ensure_detail(nf);
            self.cur = Some((nf, if step < 0 { len.saturating_sub(1) } else { 0 }));
        }
        self.scroll_to = true;
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let files = self.shown_files();
        if self.cur.map(|(f, _)| !files.contains(&f)).unwrap_or(true) {
            self.cur = files.first().map(|&f| (f, 0));
            self.scroll_to = true;
        }
        let cur_len = self.cur.map(|(f, _)| self.ensure_detail(f)).unwrap_or(0);
        let file_pos = self.cur.and_then(|(f, _)| files.iter().position(|&x| x == f)).map(|p| p + 1).unwrap_or(0);
        let where_ = trf!("Línea {} de {} · archivo {} de {}", self.cur.map(|c| c.1 + 1).unwrap_or(0).min(cur_len), cur_len, file_pos, files.len());
        let mut step = 0i64;
        let applied = self.applied;
        ui.horizontal_wrapped(|ui| {
            if ui.button("⏮").on_hover_text(tr("Primera coincidencia")).clicked() {
                self.cur = None;
                step = 1;
            }
            if ui.button(tr("◀ Anterior")).on_hover_text(tr("Mayús + F3")).clicked() {
                step = -1;
            }
            ui.label(RichText::new(&where_).strong());
            if ui.button(tr("Siguiente ▶")).on_hover_text(tr("F3")).clicked() {
                step = 1;
            }
            if ui.button("⏭").on_hover_text(tr("Última coincidencia")).clicked() {
                self.cur = None;
                step = -1;
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar por ruta")).desired_width(160.0));
            if !applied {
                if ui.small_button(tr("Marcar todos")).clicked() {
                    self.results.iter_mut().for_each(|r| r.on = true);
                }
                if ui.small_button(tr("Desmarcar todos")).clicked() {
                    self.results.iter_mut().for_each(|r| r.on = false);
                }
            }
        });
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        ui.input_mut(|i| {
            if i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3) {
                step = -1;
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::F3) || (!typing && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                step = 1;
            } else if !typing && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                step = -1;
            }
        });
        if step != 0 {
            self.step(&files, step);
        }
        let roots = self.src.items.clone();
        let mut folder = None;
        let mut open = None;
        let height = ui.available_height().max(320.0);
        let (area, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
        let mut panes = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
        // Left: the files, ticked to be changed, with their count and what happened.
        egui::SidePanel::left(egui::Id::new("replace_files")).resizable(true).default_width(300.0).width_range(180.0..=area.width() * 0.6).show_inside(&mut panes, |ui| {
            ui.label(RichText::new(trf!("ARCHIVOS ({})", files.len())).small().strong().color(theme::p().sub));
            let scroll_file = if self.scroll_to { self.cur.and_then(|(f, _)| files.iter().position(|&x| x == f)) } else { None };
            let mut sa = egui::ScrollArea::both().id_salt("replace_file_list").auto_shrink([false, false]);
            if let Some(i) = scroll_file {
                sa = sa.vertical_scroll_offset((i as f32 * 40.0 - ui.available_height() / 2.0).max(0.0));
            }
            sa.show_rows(ui, 40.0, files.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for &fi in &files[range] {
                    let selected = self.cur.map(|(c, _)| c == fi).unwrap_or(false);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(260.0), 40.0), egui::Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 4.0, theme::accent().gamma_multiply(0.22));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, theme::p().hover);
                    }
                    let f = &mut self.results[fi];
                    let mut text_left = rect.left() + 8.0;
                    if !applied {
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 0.0), egui::vec2(24.0, rect.height()))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                        child.checkbox(&mut f.on, "").on_hover_text(tr("Cambiar este archivo"));
                        text_left += 24.0;
                    }
                    let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let rel = roots.iter().find_map(|r| f.path.parent().and_then(|p| p.strip_prefix(r).ok())).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| f.path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
                    let (state, color) = match (&f.error, f.changed, applied) {
                        (Some(_), _, _) => (tr("✖ error").to_string(), theme::p().danger),
                        (_, true, true) => (tr("✔ modificado").to_string(), theme::p().ok),
                        (_, false, true) => (tr("sin cambios").to_string(), theme::p().muted),
                        _ => (f.count.to_string(), theme::accent()),
                    };
                    let name_color = if f.on || applied { theme::p().text } else { theme::p().muted };
                    let p = ui.painter();
                    p.text(egui::pos2(text_left, rect.top() + 4.0), egui::Align2::LEFT_TOP, name, FontId::proportional(14.0), name_color);
                    p.text(egui::pos2(text_left, rect.bottom() - 4.0), egui::Align2::LEFT_BOTTOM, rel, FontId::proportional(11.0), theme::p().muted);
                    p.text(rect.right_top() + egui::vec2(-8.0, 4.0), egui::Align2::RIGHT_TOP, state, FontId::proportional(12.0), color);
                    let hover = match &f.error {
                        Some(e) => format!("{}\n{e}", f.path.display()),
                        None => f.path.to_string_lossy().to_string(),
                    };
                    let resp = resp.on_hover_text(hover);
                    if resp.clicked() {
                        self.cur = Some((fi, 0));
                        self.scroll_to = true;
                    }
                    if resp.double_clicked() {
                        open = Some(f.path.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button(tr("Abrir con la aplicación predeterminada")).clicked() {
                            open = Some(f.path.clone());
                            ui.close_menu();
                        }
                        if ui.button(tr("Ir a la carpeta")).clicked() {
                            folder = f.path.parent().map(Path::to_path_buf);
                            ui.close_menu();
                        }
                    });
                }
            });
        });
        // Right: each changed line as it is now (−) and as it will be (+), whole, scrolling
        // both ways.
        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, ..Default::default() })).show_inside(&mut panes, |ui| {
            let Some((fi, hi)) = self.cur else { return };
            // Only this file is read and previewed, live as the texts change.
            self.ensure_detail(fi);
            let d = self.detail.as_ref().map(|d| d.1.clone()).unwrap_or_default();
            let path = self.results[fi].path.clone();
            ui.add(egui::Label::new(RichText::new(path.to_string_lossy()).strong()).truncate());
            ui.horizontal(|ui| {
                if ui.small_button(tr("↗ Abrir")).on_hover_text(tr("Abrir con la aplicación predeterminada")).clicked() {
                    open = Some(path.clone());
                }
                if ui.small_button(tr("📁 Carpeta")).on_hover_text(tr("Ir a la carpeta")).clicked() {
                    folder = path.parent().map(Path::to_path_buf);
                }
                let f = &d;
                if let Some(n) = &f.note {
                    ui.label(RichText::new(n).small().color(theme::p().danger));
                }
                if f.hits.len() >= MAX_HITS_PER_FILE {
                    ui.label(RichText::new(trf!("se muestran las primeras {} líneas de {} coincidencias", f.hits.len(), f.count)).small().color(theme::p().muted));
                }
            });
            let font = FontId::monospace(13.0);
            let row_h = 44.0;
            let hits_len = d.hits.len();
            let list_h = ui.available_height();
            let mut sa = egui::ScrollArea::both().id_salt(("replace_hits", fi)).auto_shrink([false, false]);
            if self.scroll_to {
                sa = sa.vertical_scroll_offset((hi as f32 * row_h - list_h / 2.0 + row_h).max(0.0)).horizontal_scroll_offset(0.0);
            }
            let mut pick = None;
            sa.show_rows(ui, row_h, hits_len, |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for h in range {
                    let hit = &d.hits[h];
                    let old = ui.fonts(|f| f.layout_job(highlighted(&hit.old, &hit.ranges, font.clone(), theme::p().text, Color32::BLACK, Color32::from_rgb(255, 150, 130))));
                    let new = ui.fonts(|f| f.layout_no_wrap(hit.new.clone(), font.clone(), theme::p().ok));
                    let w = (old.size().x.max(new.size().x) + 90.0).max(ui.available_width());
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), egui::Sense::click());
                    if h == hi {
                        ui.painter().rect_filled(rect, 3.0, theme::accent().gamma_multiply(0.22));
                    } else if h % 2 == 1 {
                        ui.painter().rect_filled(rect, 0.0, theme::p().stripe.gamma_multiply(0.5));
                    }
                    let gutter = egui::Rect::from_min_size(rect.min, egui::vec2(56.0, rect.height()));
                    let p = ui.painter();
                    p.rect_filled(gutter, 0.0, theme::p().hover.gamma_multiply(0.6));
                    p.text(gutter.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, hit.line.to_string(), FontId::monospace(12.0), theme::p().sub);
                    let x = gutter.right() + 8.0;
                    p.text(egui::pos2(x, rect.top() + 11.0), egui::Align2::LEFT_CENTER, "−", font.clone(), theme::p().danger);
                    p.galley(egui::pos2(x + 16.0, rect.top() + 11.0 - old.size().y / 2.0), old, theme::p().text);
                    p.text(egui::pos2(x, rect.bottom() - 11.0), egui::Align2::LEFT_CENTER, "+", font.clone(), theme::p().ok);
                    p.galley(egui::pos2(x + 16.0, rect.bottom() - 11.0 - new.size().y / 2.0), new, theme::p().ok);
                    if resp.clicked() {
                        pick = Some(h);
                    }
                }
            });
            if let Some(h) = pick {
                self.cur = Some((fi, h));
            }
        });
        self.scroll_to = false;
        if let Some(p) = open {
            if let Err(e) = fsops::open_with_system(&p) {
                self.status = Some((trf!("No se pudo abrir «{}»: {}", p.display(), e), true));
            }
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
