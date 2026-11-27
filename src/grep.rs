//! Search text inside files: plain text or regular expressions, case and whole-word options,
//! filters by name and size, binary files skipped, several files at once, with the matching
//! lines shown (the found text highlighted) and one click to open a file or its folder.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::RootList;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, text::LayoutJob, Color32, FontId, RichText, TextFormat};
use regex::{Regex, RegexBuilder};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_HITS: usize = 50_000;
const MAX_PER_FILE: usize = 200;

#[derive(Clone)]
struct Hit {
    line: usize,
    text: String,
    ranges: Vec<(usize, usize)>,
}

#[derive(Clone)]
struct FileHit {
    path: PathBuf,
    hits: Vec<Hit>,
    count: usize,
}

#[derive(Clone)]
struct Cfg {
    re: Regex,
    include: Option<Regex>,
    exclude: Option<Regex>,
    exclude_dirs: Option<Regex>,
    hidden: bool,
    recursive: bool,
    max_bytes: u64,
    skip_binary: bool,
}

/// Shortens a very long line around the first match, keeping the highlight positions right.
fn clip(line: &str, ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    const MAX: usize = 2000;
    if line.len() <= MAX {
        return (line.to_string(), ranges.to_vec());
    }
    let mut start = ranges.first().map(|r| r.0).unwrap_or(0).saturating_sub(400);
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + MAX).min(line.len());
    while end < line.len() && !line.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = if start > 0 { "…" } else { "" };
    let shifted = ranges.iter().filter(|(s, e)| *s >= start && *e <= end).map(|(s, e)| (s - start + prefix.len(), e - start + prefix.len())).collect();
    (format!("{prefix}{}{}", &line[start..end], if end < line.len() { "…" } else { "" }), shifted)
}

/// `text` with the given byte ranges painted as found text.
pub(crate) fn highlighted(text: &str, ranges: &[(usize, usize)], font: FontId, color: Color32, found_color: Color32, found_bg: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut at = 0;
    for &(s, e) in ranges {
        if s < at || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        job.append(&text[at..s], 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
        job.append(&text[s..e], 0.0, TextFormat { font_id: font.clone(), color: found_color, background: found_bg, ..Default::default() });
        at = e;
    }
    job.append(&text[at..], 0.0, TextFormat { font_id: font, color, ..Default::default() });
    job
}

fn scan_file(path: &Path, c: &Cfg) -> Option<FileHit> {
    let mut f = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    f.by_ref().take(c.max_bytes + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > c.max_bytes {
        return None;
    }
    if c.skip_binary && bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut hit = FileHit { path: path.to_path_buf(), hits: vec![], count: 0 };
    for (i, line) in text.lines().enumerate() {
        let ranges: Vec<(usize, usize)> = c.re.find_iter(line).filter(|m| m.end() > m.start()).map(|m| (m.start(), m.end())).collect();
        if ranges.is_empty() {
            continue;
        }
        hit.count += ranges.len();
        if hit.hits.len() < MAX_PER_FILE {
            let (t, r) = clip(line, &ranges);
            hit.hits.push(Hit { line: i + 1, text: t, ranges: r });
        }
    }
    (hit.count > 0).then_some(hit)
}

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    files: AtomicUsize,
    done: AtomicUsize,
    total: AtomicUsize,
    hits: AtomicUsize,
}

fn collect_files(roots: &[PathBuf], c: &Cfg, cancel: &AtomicBool, errs: &mut Vec<String>) -> Vec<PathBuf> {
    let mut files = vec![];
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return files;
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
                if !c.hidden && name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
                if meta.is_dir() {
                    if c.recursive && !c.exclude_dirs.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                        stack.push(e.path());
                    }
                } else if meta.is_file() && meta.len() <= c.max_bytes && c.include.as_ref().map(|re| re.is_match(&name)).unwrap_or(true) && !c.exclude.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                    files.push(e.path());
                }
            }
        }
    }
    files.sort();
    files
}

fn search(roots: &[PathBuf], c: &Cfg, prog: &Progress, cancel: &AtomicBool) -> (Vec<FileHit>, Vec<String>, usize, bool) {
    let mut errs = vec![];
    let set = |s: &str| {
        if let Ok(mut st) = prog.stage.lock() {
            *st = s.to_string();
        }
    };
    set(tr("Buscando archivos"));
    let files = collect_files(roots, c, cancel, &mut errs);
    prog.total.store(files.len(), Ordering::Relaxed);
    prog.files.store(files.len(), Ordering::Relaxed);
    set(tr("Leyendo archivos"));
    let found: Mutex<Vec<FileHit>> = Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() || cancel.load(Ordering::Relaxed) || prog.hits.load(Ordering::Relaxed) >= MAX_HITS {
                    break;
                }
                if let Some(h) = scan_file(&files[i], c) {
                    prog.hits.fetch_add(h.count, Ordering::Relaxed);
                    if let Ok(mut v) = found.lock() {
                        v.push(h);
                    }
                }
                prog.done.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
    let mut v = found.into_inner().unwrap_or_default();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = prog.hits.load(Ordering::Relaxed) >= MAX_HITS;
    (v, errs, files.len(), truncated)
}

struct Job {
    rx: Receiver<(Vec<FileHit>, Vec<String>, usize, bool)>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct Searcher {
    roots: RootList,
    query: String,
    regex: bool,
    case: bool,
    whole: bool,
    include: String,
    exclude: String,
    exclude_dirs: String,
    name_regex: bool,
    recursive: bool,
    hidden: bool,
    max_mb: u32,
    skip_binary: bool,
    job: Option<Job>,
    results: Vec<FileHit>,
    scanned: usize,
    truncated: bool,
    secs: f32,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    filter: String,
    /// The chosen match: (file, line among its matches).
    cur: Option<(usize, usize)>,
    /// Scroll the lists to the chosen match on the next frame.
    scroll_to: bool,
    /// Lines shown before and after the chosen match.
    context: usize,
    file_cache: Option<(PathBuf, Vec<String>)>,
    /// Open or fold the search form on the next frame.
    fold_form: Option<bool>,
    last_re: Option<Regex>,
    base: PathBuf,
    sort: u8,
}

impl Searcher {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Searcher {
        Searcher {
            roots: RootList::new(targets.into_iter().filter(|p| p.is_dir()).collect(), "grep_roots"),
            query: String::new(),
            regex: false,
            case: false,
            whole: false,
            include: String::new(),
            exclude: String::new(),
            exclude_dirs: ".git; node_modules; target; .cache".into(),
            name_regex: false,
            recursive: true,
            hidden: false,
            max_mb: 20,
            skip_binary: true,
            job: None,
            results: vec![],
            scanned: 0,
            truncated: false,
            secs: 0.0,
            status: None,
            problems: vec![],
            filter: String::new(),
            cur: None,
            scroll_to: false,
            context: 4,
            file_cache: None,
            fold_form: None,
            last_re: None,
            base,
            sort: 0,
        }
    }

    /// Searches a word in the folder at once, for trying the tool (`--grep`).
    pub fn demo(&mut self, dir: &Path, word: &str) {
        self.roots.add(dir.to_path_buf());
        self.query = word.to_string();
        self.start();
    }

    fn config(&self) -> Result<Cfg, String> {
        if self.roots.roots.is_empty() {
            return Err(tr("Añade al menos una carpeta donde buscar.").to_string());
        }
        if self.query.is_empty() {
            return Err(tr("Escribe el texto que quieres buscar.").to_string());
        }
        let mut pat = if self.regex { self.query.clone() } else { regex::escape(&self.query) };
        if self.whole {
            pat = format!(r"\b(?:{pat})\b");
        }
        let re = RegexBuilder::new(&pat).case_insensitive(!self.case).build().map_err(|e| trf!("Expresión no válida: {}", e.to_string().lines().last().unwrap_or("").trim()))?;
        let m = |t: &str| crate::rename::name_matcher(t, self.name_regex).map_err(|e| trf!("Expresión no válida: {}", e));
        Ok(Cfg { re, include: m(&self.include)?, exclude: m(&self.exclude)?, exclude_dirs: m(&self.exclude_dirs)?, hidden: self.hidden, recursive: self.recursive, max_bytes: self.max_mb as u64 * 1024 * 1024, skip_binary: self.skip_binary })
    }

    fn start(&mut self) {
        let cfg = match self.config() {
            Ok(c) => c,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let roots = self.roots.roots.clone();
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        self.last_re = Some(cfg.re.clone());
        std::thread::spawn(move || {
            let _ = tx.send(search(&roots, &cfg, &p2, &c2));
        });
        self.results.clear();
        self.cur = None;
        self.file_cache = None;
        self.status = None;
        self.problems.clear();
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok((results, errs, scanned, truncated)) = job.rx.try_recv() else { return };
        self.secs = job.started.elapsed().as_secs_f32();
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        self.results = results;
        self.problems = errs;
        if !self.results.is_empty() {
            self.fold_form = Some(false);
        }
        self.scanned = scanned;
        self.truncated = truncated;
        if cancelled {
            self.status = Some((tr("Búsqueda cancelada: se muestra lo encontrado hasta ahora").to_string(), true));
        }
    }

    fn export_csv(&mut self, events: &mut Vec<ToolEvent>) {
        let mut out = String::from("ruta;linea;texto\n");
        for f in &self.results {
            for h in &f.hits {
                out.push_str(&format!("\"{}\";{};\"{}\"\n", f.path.to_string_lossy().replace('"', "\"\""), h.line, h.text.replace('"', "\"\"")));
            }
        }
        let file = fsops::unique_name(&self.base, "busqueda.csv");
        self.status = Some(match fs::write(&file, out) {
            Ok(()) => {
                events.push(ToolEvent::Reload);
                (trf!("Guardado en {}", file.display()), false)
            }
            Err(e) => (trf!("No se pudo guardar: {}", e), true),
        });
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll();
        let busy = self.job.is_some();
        // The search form folds away once there are results, to leave the room to them.
        let header = egui::CollapsingHeader::new(RichText::new(tr("DÓNDE Y QUÉ BUSCAR")).small().strong().color(theme::p().sub)).id_salt("grep_form").open(self.fold_form.take()).default_open(true);
        header.show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.roots.ui(ui, selection, cwd, tr("CARPETAS DONDE BUSCAR"));
                ui.separator();
                self.query_ui(ui);
            });
        });
        self.action_ui(ui, events);
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                egui::ScrollArea::vertical().max_height(90.0).id_salt("grep_err").show(ui, |ui| {
                    for p in &self.problems {
                        ui.label(RichText::new(p).small().color(theme::p().danger));
                    }
                });
            });
        }
        self.results_ui(ui, events);
    }

    fn query_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("QUÉ BUSCAR")).small().strong().color(theme::p().sub));
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.query).hint_text(tr("texto a buscar dentro de los archivos")).desired_width(420.0).font(egui::TextStyle::Monospace));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.start();
            }
            ui.checkbox(&mut self.case, tr("Distinguir mayúsculas"));
            ui.checkbox(&mut self.whole, tr("Palabra completa"));
            ui.checkbox(&mut self.regex, tr("Expresión regular"));
        });
        ui.add_space(2.0);
        egui::Grid::new("grep_filters").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            crate::ui_kit::label(ui, tr("Incluir solo"));
            ui.add(egui::TextEdit::singleline(&mut self.include).hint_text(if self.name_regex { "\\.(txt|md|rs)$" } else { "*.txt; *.md; *.rs" }).desired_width(f32::INFINITY)).on_hover_text(tr("Patrones de nombre separados por «;», con * y ?. Vacío = todos los archivos."));
            ui.end_row();
            crate::ui_kit::label(ui, tr("Excluir"));
            ui.add(egui::TextEdit::singleline(&mut self.exclude).hint_text(if self.name_regex { "\\.min\\.js$|\\.log$" } else { "*.min.js; *.log" }).desired_width(f32::INFINITY));
            ui.end_row();
            crate::ui_kit::label(ui, tr("Saltar carpetas"));
            ui.add(egui::TextEdit::singleline(&mut self.exclude_dirs).desired_width(f32::INFINITY));
            ui.end_row();
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas"));
            ui.checkbox(&mut self.hidden, tr("Incluir ocultos"));
            ui.checkbox(&mut self.skip_binary, tr("Saltar binarios"));
            ui.checkbox(&mut self.name_regex, tr("Filtros con expresiones regulares")).on_hover_text(tr("Incluir, excluir y carpetas a saltar se leen como expresiones regulares sobre el nombre (sin distinguir mayúsculas)."));
            ui.label(tr("Tamaño máximo"));
            ui.add(egui::DragValue::new(&mut self.max_mb).range(1..=2048).suffix(" MB")).on_hover_text(tr("Los archivos más grandes no se leen"));
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🔍 Buscar")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy, btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let (d, t) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                if t > 0 {
                    ui.add(egui::ProgressBar::new(d as f32 / t as f32).desired_width(180.0).show_percentage());
                }
                let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
                ui.label(RichText::new(trf!("{} · {} coincidencias", stage, p.hits.load(Ordering::Relaxed))).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
            if !self.results.is_empty() && !busy {
                if ui.button(tr("💾 Exportar CSV")).clicked() {
                    self.export_csv(events);
                }
                if ui.button(tr("📋 Copiar rutas")).clicked() {
                    events.push(ToolEvent::Clipboard(self.results.iter().map(|f| f.path.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n")));
                }
                if ui.button(tr("✏ Reemplazar en estos archivos…")).on_hover_text(tr("Abre «Buscar y reemplazar» con estos archivos y este texto, para cambiarlo con vista previa")).clicked() {
                    events.push(ToolEvent::ReplaceIn(self.results.iter().map(|f| f.path.clone()).collect(), self.query.clone(), self.regex, self.case, self.whole));
                }
            }
        });
    }

    /// Files that pass the path filter, in the current order.
    fn shown_files(&self) -> Vec<usize> {
        let q = self.filter.to_lowercase();
        (0..self.results.len()).filter(|&i| q.is_empty() || self.results[i].path.to_string_lossy().to_lowercase().contains(&q)).collect()
    }

    /// Moves the current match by `step` (±1) across the shown files, wrapping around.
    fn step(&mut self, files: &[usize], step: i64) {
        let flat: Vec<(usize, usize)> = files.iter().flat_map(|&f| (0..self.results[f].hits.len()).map(move |h| (f, h))).collect();
        if flat.is_empty() {
            return;
        }
        let at = self.cur.and_then(|c| flat.iter().position(|x| *x == c));
        let next = match at {
            Some(i) => (i as i64 + step).rem_euclid(flat.len() as i64) as usize,
            None if step < 0 => flat.len() - 1,
            None => 0,
        };
        self.cur = Some(flat[next]);
        self.scroll_to = true;
    }

    /// The file's lines, read once when it becomes current, for the context view.
    fn file_lines(&mut self, path: &Path) -> &[String] {
        if self.file_cache.as_ref().map(|(p, _)| p != path).unwrap_or(true) {
            let mut bytes = vec![];
            let _ = fs::File::open(path).and_then(|f| f.take(self.max_mb as u64 * 1024 * 1024).read_to_end(&mut bytes));
            let lines = String::from_utf8_lossy(&bytes).lines().map(|l| l.chars().take(4000).collect()).collect();
            self.file_cache = Some((path.to_path_buf(), lines));
        }
        &self.file_cache.as_ref().unwrap().1
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.job.is_some() {
            return;
        }
        if self.results.is_empty() {
            if self.last_re.is_some() && self.status.is_none() {
                ui.label(trf!("No se encontró nada en {} archivos.", self.scanned));
            }
            return;
        }
        let total: usize = self.results.iter().map(|f| f.count).sum();
        ui.separator();
        ui.label(RichText::new(trf!("{} coincidencias en {} archivos (de {} revisados) · {} s", total, self.results.len(), self.scanned, format!("{:.1}", self.secs))).strong());
        if self.truncated {
            ui.label(RichText::new(tr("Hay demasiadas coincidencias: la búsqueda se detuvo en las primeras. Afina el texto o los filtros.")).small().color(theme::p().danger));
        }
        let files = self.shown_files();
        if self.cur.map(|(f, _)| !files.contains(&f)).unwrap_or(true) {
            self.cur = files.first().map(|&f| (f, 0));
            self.scroll_to = true;
        }
        // Navigation bar: previous / next match across every file (F3 and Shift+F3 too).
        let flat_len: usize = files.iter().map(|&f| self.results[f].hits.len()).sum();
        let pos = self.cur.map(|(cf, ch)| files.iter().take_while(|&&f| f != cf).map(|&f| self.results[f].hits.len()).sum::<usize>() + ch + 1).unwrap_or(0);
        let mut step = 0i64;
        ui.horizontal(|ui| {
            if ui.button(tr("⏮")).on_hover_text(tr("Primera coincidencia")).clicked() {
                self.cur = None;
                step = 1;
            }
            if ui.button(tr("◀ Anterior")).on_hover_text(tr("Mayús + F3")).clicked() {
                step = -1;
            }
            ui.label(RichText::new(trf!("{} de {}", pos, flat_len)).strong());
            if ui.button(tr("Siguiente ▶")).on_hover_text(tr("F3")).clicked() {
                step = 1;
            }
            if ui.button(tr("⏭")).on_hover_text(tr("Última coincidencia")).clicked() {
                self.cur = None;
                step = -1;
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar por ruta")).desired_width(160.0));
            let before = self.sort;
            egui::ComboBox::from_id_salt("grep_sort").selected_text(tr(["Ordenar por ruta", "Más coincidencias primero", "Por nombre"][self.sort as usize])).show_ui(ui, |ui| {
                for (k, l) in ["Ordenar por ruta", "Más coincidencias primero", "Por nombre"].iter().enumerate() {
                    ui.selectable_value(&mut self.sort, k as u8, tr(l));
                }
            });
            if before != self.sort {
                let cur = self.cur.map(|(f, h)| (self.results[f].path.clone(), h));
                match self.sort {
                    1 => self.results.sort_by(|a, b| b.count.cmp(&a.count).then(a.path.cmp(&b.path))),
                    2 => self.results.sort_by(|a, b| a.path.file_name().map(|n| n.to_ascii_lowercase()).cmp(&b.path.file_name().map(|n| n.to_ascii_lowercase()))),
                    _ => self.results.sort_by(|a, b| a.path.cmp(&b.path)),
                }
                self.cur = cur.and_then(|(p, h)| self.results.iter().position(|f| f.path == p).map(|f| (f, h)));
                self.scroll_to = true;
            }
            ui.label(RichText::new(tr("Contexto")).small().color(theme::p().sub));
            ui.add(egui::DragValue::new(&mut self.context).range(0..=50).suffix(tr(" líneas"))).on_hover_text(tr("Líneas que se ven antes y después de la coincidencia elegida"));
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
        let files = self.shown_files();
        let mut open = None;
        let mut folder = None;
        let mut copy = None;
        let roots = self.roots.roots.clone();
        let height = ui.available_height().max(320.0);
        let (area, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
        let mut panes = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
        // Left: the files, with how many matches each has.
        egui::SidePanel::left(egui::Id::new("grep_files")).resizable(true).default_width(280.0).width_range(160.0..=area.width() * 0.6).show_inside(&mut panes, |ui| {
            ui.label(RichText::new(trf!("ARCHIVOS ({})", files.len())).small().strong().color(theme::p().sub));
            let scroll_file = if self.scroll_to { self.cur.and_then(|(f, _)| files.iter().position(|&x| x == f)) } else { None };
            let mut sa = egui::ScrollArea::both().id_salt("grep_file_list").auto_shrink([false, false]);
            if let Some(i) = scroll_file {
                sa = sa.vertical_scroll_offset((i as f32 * 40.0 - ui.available_height() / 2.0).max(0.0));
            }
            sa.show_rows(ui, 40.0, files.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for &fi in &files[range] {
                    let f = &self.results[fi];
                    let selected = self.cur.map(|(c, _)| c == fi).unwrap_or(false);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(240.0), 40.0), egui::Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 4.0, theme::accent().gamma_multiply(0.22));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, theme::p().hover);
                    }
                    let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let rel = roots.iter().find_map(|r| f.path.parent().and_then(|p| p.strip_prefix(r).ok())).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| f.path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
                    let p = ui.painter();
                    p.text(rect.left_top() + egui::vec2(8.0, 4.0), egui::Align2::LEFT_TOP, name, FontId::proportional(14.0), theme::p().text);
                    p.text(rect.left_bottom() + egui::vec2(8.0, -4.0), egui::Align2::LEFT_BOTTOM, rel, FontId::proportional(11.0), theme::p().muted);
                    p.text(rect.right_top() + egui::vec2(-8.0, 4.0), egui::Align2::RIGHT_TOP, f.count.to_string(), FontId::proportional(12.0), theme::accent());
                    let resp = resp.on_hover_text(f.path.to_string_lossy());
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
                        if ui.button(tr("Copiar ruta")).clicked() {
                            copy = Some(f.path.to_string_lossy().to_string());
                            ui.close_menu();
                        }
                    });
                }
            });
        });
        // Right: the matches of the current file (scroll both ways, nothing cut) and the
        // lines around the chosen one.
        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, ..Default::default() })).show_inside(&mut panes, |ui| {
            let Some((fi, hi)) = self.cur else { return };
            let path = self.results[fi].path.clone();
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(path.to_string_lossy()).strong()).truncate());
            });
            ui.horizontal(|ui| {
                if ui.small_button(tr("↗ Abrir")).on_hover_text(tr("Abrir con la aplicación predeterminada")).clicked() {
                    open = Some(path.clone());
                }
                if ui.small_button(tr("📁 Carpeta")).on_hover_text(tr("Ir a la carpeta")).clicked() {
                    folder = path.parent().map(Path::to_path_buf);
                }
                if ui.small_button(tr("📋 Ruta")).on_hover_text(tr("Copiar ruta")).clicked() {
                    copy = Some(path.to_string_lossy().to_string());
                }
                if ui.small_button(tr("📋 Línea")).on_hover_text(tr("Copiar la línea elegida")).clicked() {
                    copy = self.results[fi].hits.get(hi).map(|h| h.text.clone());
                }
                let f = &self.results[fi];
                if f.hits.len() >= MAX_PER_FILE {
                    ui.label(RichText::new(trf!("se muestran las primeras {} líneas de {} coincidencias", f.hits.len(), f.count)).small().color(theme::p().muted));
                }
            });
            let ctx_lines = self.context;
            let ctx_h = if ctx_lines == 0 { 0.0 } else { (ctx_lines as f32 * 2.0 + 1.0) * 18.0 + 34.0 };
            let list_h = (ui.available_height() - ctx_h).max(120.0);
            let font = FontId::monospace(13.0);
            let row_h = 22.0;
            let hits_len = self.results[fi].hits.len();
            let mut sa = egui::ScrollArea::both().id_salt(("grep_hits", fi)).max_height(list_h).auto_shrink([false, false]);
            if self.scroll_to {
                sa = sa.vertical_scroll_offset((hi as f32 * row_h - list_h / 2.0 + row_h).max(0.0)).horizontal_scroll_offset(0.0);
            }
            let mut pick = None;
            sa.show_rows(ui, row_h, hits_len, |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for h in range {
                    let hit = &self.results[fi].hits[h];
                    let job = highlighted(&hit.text, &hit.ranges, font.clone(), theme::p().text, Color32::BLACK, Color32::from_rgb(255, 214, 90));
                    let galley = ui.fonts(|f| f.layout_job(job));
                    let w = (galley.size().x + 70.0).max(ui.available_width());
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), egui::Sense::click());
                    if h == hi {
                        ui.painter().rect_filled(rect, 3.0, theme::accent().gamma_multiply(0.22));
                    } else if h % 2 == 1 {
                        ui.painter().rect_filled(rect, 0.0, theme::p().stripe.gamma_multiply(0.5));
                    }
                    let gutter = egui::Rect::from_min_size(rect.min, egui::vec2(56.0, rect.height()));
                    ui.painter().rect_filled(gutter, 0.0, theme::p().hover.gamma_multiply(0.6));
                    ui.painter().text(gutter.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, hit.line.to_string(), FontId::monospace(12.0), theme::p().sub);
                    ui.painter().galley(egui::pos2(gutter.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, theme::p().text);
                    if resp.clicked() {
                        pick = Some(h);
                    }
                    if resp.double_clicked() {
                        open = Some(self.results[fi].path.clone());
                    }
                }
            });
            if let Some(h) = pick {
                self.cur = Some((fi, h));
            }
            if ctx_lines > 0 {
                let Some((fi, hi)) = self.cur else { return };
                let line_no = self.results[fi].hits.get(hi).map(|h| h.line).unwrap_or(1);
                ui.separator();
                ui.label(RichText::new(trf!("CONTEXTO · línea {}", line_no)).small().strong().color(theme::p().sub));
                let re = self.last_re.clone();
                let lines: Vec<(usize, String)> = {
                    let all = self.file_lines(&path);
                    let from = line_no.saturating_sub(ctx_lines + 1);
                    let to = (line_no + ctx_lines).min(all.len());
                    (from..to).map(|i| (i + 1, all[i].clone())).collect()
                };
                egui::Frame::new().fill(theme::p().stripe).corner_radius(4.0).inner_margin(4.0).show(ui, |ui| {
                    egui::ScrollArea::both().id_salt("grep_context").auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (n, text) in lines {
                            let ranges: Vec<(usize, usize)> = if n == line_no { re.as_ref().map(|r| r.find_iter(&text).map(|m| (m.start(), m.end())).collect()).unwrap_or_default() } else { vec![] };
                            let color = if n == line_no { theme::p().text } else { theme::p().sub };
                            let job = highlighted(&text, &ranges, FontId::monospace(13.0), color, Color32::BLACK, Color32::from_rgb(255, 214, 90));
                            ui.horizontal(|ui| {
                                ui.add_sized([48.0, 18.0], egui::Label::new(RichText::new(n.to_string()).monospace().small().color(if n == line_no { theme::accent() } else { theme::p().muted })));
                                ui.add(egui::Label::new(job).extend());
                            });
                        }
                    });
                });
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
        if let Some(t) = copy {
            events.push(ToolEvent::Clipboard(t));
        }
        let _ = human_size(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> PathBuf {
        let d = std::env::temp_dir().join(format!("grep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::create_dir_all(d.join("node_modules")).unwrap();
        fs::write(d.join("a.txt"), "Hola mundo\nsegunda línea con Mundo\nsin nada\nsubmundo\n").unwrap();
        fs::write(d.join("sub/b.md"), "el mundo es grande\n").unwrap();
        fs::write(d.join("node_modules/x.js"), "mundo\n").unwrap();
        let mut bin = b"mundo".to_vec();
        bin.extend([0u8; 20]);
        fs::write(d.join("c.bin"), bin).unwrap();
        d
    }

    fn cfg(q: &str, regex: bool, case: bool, whole: bool) -> Cfg {
        let mut pat = if regex { q.to_string() } else { regex::escape(q) };
        if whole {
            pat = format!(r"\b(?:{pat})\b");
        }
        Cfg {
            re: RegexBuilder::new(&pat).case_insensitive(!case).build().unwrap(),
            include: None,
            exclude: None,
            exclude_dirs: crate::rename::name_matcher("node_modules", false).unwrap(),
            hidden: false,
            recursive: true,
            max_bytes: 1 << 20,
            skip_binary: true,
        }
    }

    fn run(d: &Path, c: &Cfg) -> Vec<FileHit> {
        search(&[d.to_path_buf()], c, &Progress::default(), &AtomicBool::new(false)).0
    }

    #[test]
    fn plain_regex_case_whole_word_and_skips() {
        let d = setup();
        let r = run(&d, &cfg("mundo", false, false, false));
        let names: Vec<String> = r.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["a.txt", "b.md"], "binary and skipped folders left out: {names:?}");
        assert_eq!(r[0].count, 3, "mundo, Mundo and submundo");
        assert_eq!(run(&d, &cfg("mundo", false, true, false))[0].count, 2, "case sensitive: not «Mundo»");
        assert_eq!(run(&d, &cfg("mundo", false, false, true))[0].count, 2, "whole word: not «submundo»");
        let re = run(&d, &cfg(r"^seg\w+ l[ií]nea", true, false, false));
        assert_eq!(re.len(), 1);
        assert_eq!((re[0].hits[0].line, re[0].hits[0].ranges.clone()), (2, vec![(0, 14)]));
        let mut c = cfg("mundo", false, false, false);
        c.skip_binary = false;
        assert!(run(&d, &c).iter().any(|f| f.path.ends_with("c.bin")));
        c.include = crate::rename::name_matcher("*.md", false).unwrap();
        assert_eq!(run(&d, &c).len(), 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn long_lines_are_clipped_around_the_match() {
        let line = format!("{}OBJETIVO{}", "ñ".repeat(2000), "é".repeat(2000));
        let start = line.find("OBJETIVO").unwrap();
        let (t, r) = clip(&line, &[(start, start + 8)]);
        assert!(t.len() < 2100 && t.starts_with('…') && t.ends_with('…'));
        assert_eq!(&t[r[0].0..r[0].1], "OBJETIVO");
        let (t, r) = clip("corta", &[(0, 3)]);
        assert_eq!((t.as_str(), r), ("corta", vec![(0, 3)]));
    }
}
