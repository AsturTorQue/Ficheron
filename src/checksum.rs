//! Checksums: CRC32, SHA-1, SHA-256, SHA-512 and BLAKE3 of many files at once, read once and
//! hashed with several workers. Files and folders come from a list with an "Examinar…" button and
//! name filters; the result can be checked against a value, a pasted list or a sums file, and
//! saved as a standard `SHA256SUMS`-style file.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::{picker_window, Picker};
use crate::sources::Sources;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ALGOS: [(&str, &str); 5] = [("CRC32", "crc32"), ("SHA-1", "sha1"), ("SHA-256", "sha256"), ("SHA-512", "sha512"), ("BLAKE3", "blake3")];

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Hashes of one file for the chosen algorithms (same order as `ALGOS`, empty when not chosen),
/// reading it only once.
fn hash_file(p: &Path, on: [bool; 5], done: &AtomicU64, cancel: &AtomicBool) -> Result<Vec<String>, String> {
    use sha2::Digest;
    let mut f = fs::File::open(p).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut crc = crc32fast::Hasher::new();
    let mut s1 = sha1::Sha1::new();
    let mut s256 = sha2::Sha256::new();
    let mut s512 = sha2::Sha512::new();
    let mut b3 = blake3::Hasher::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(tr("cancelado").to_string());
        }
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        done.fetch_add(n as u64, Ordering::Relaxed);
        let b = &buf[..n];
        if on[0] {
            crc.update(b);
        }
        if on[1] {
            s1.update(b);
        }
        if on[2] {
            s256.update(b);
        }
        if on[3] {
            s512.update(b);
        }
        if on[4] {
            b3.update(b);
        }
    }
    Ok(vec![
        if on[0] { format!("{:08x}", crc.finalize()) } else { String::new() },
        if on[1] { to_hex(&s1.finalize()) } else { String::new() },
        if on[2] { to_hex(&s256.finalize()) } else { String::new() },
        if on[3] { to_hex(&s512.finalize()) } else { String::new() },
        if on[4] { b3.finalize().to_hex().to_string() } else { String::new() },
    ])
}

/// A fixed-width cell with its content on the left (`add_sized` would centre it).
fn cell<R>(ui: &mut egui::Ui, w: f32, h: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(w, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(w);
        add(ui)
    })
    .inner
}

#[derive(Clone, Default)]
struct Row {
    path: PathBuf,
    size: u64,
    hashes: Vec<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Verdict {
    None,
    Ok,
    Mismatch,
    NoReference,
}

/// Lines of "hash", "hash  name", "hash *name" or "ALGO (name) = hash" (lower-cased hashes).
fn parse_expected(text: &str) -> Vec<(String, Option<String>)> {
    let is_hex = |s: &str| matches!(s.len(), 8 | 32 | 40 | 64 | 128) && s.chars().all(|c| c.is_ascii_hexdigit());
    let mut out = vec![];
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((left, right)) = l.rsplit_once(" = ") {
            if is_hex(right.trim()) {
                let name = left.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(n, _)| n.to_string());
                out.push((right.trim().to_lowercase(), name));
                continue;
            }
        }
        let mut it = l.splitn(2, char::is_whitespace);
        let first = it.next().unwrap_or("");
        if is_hex(first) {
            let name = it.next().map(|n| n.trim().trim_start_matches('*').trim().to_string()).filter(|n| !n.is_empty());
            out.push((first.to_lowercase(), name));
        }
    }
    out
}

fn verdict(row: &Row, expected: &[(String, Option<String>)]) -> Verdict {
    if expected.is_empty() || row.error.is_some() {
        return Verdict::None;
    }
    let name = row.path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mine = |h: &str| row.hashes.iter().any(|x| !x.is_empty() && x == h);
    let named: Vec<&(String, Option<String>)> = expected.iter().filter(|(_, n)| n.as_ref().map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).to_lowercase() == name).unwrap_or(false)).collect();
    if !named.is_empty() {
        return if named.iter().any(|(h, _)| mine(h)) { Verdict::Ok } else { Verdict::Mismatch };
    }
    let bare: Vec<&(String, Option<String>)> = expected.iter().filter(|(_, n)| n.is_none()).collect();
    if bare.is_empty() {
        return Verdict::NoReference;
    }
    if bare.iter().any(|(h, _)| mine(h)) {
        Verdict::Ok
    } else {
        Verdict::Mismatch
    }
}

struct Job {
    rx: Receiver<(Vec<Row>, Vec<String>, f32)>,
    done_bytes: Arc<AtomicU64>,
    total_bytes: Arc<AtomicU64>,
    files_done: Arc<AtomicUsize>,
    files_total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    current: Arc<Mutex<String>>,
    started: Instant,
}

pub struct Checksummer {
    src: Sources,
    on: [bool; 5],
    expected: String,
    load_picker: Option<Picker>,
    job: Option<Job>,
    rows: Vec<Row>,
    problems: Vec<String>,
    secs: f32,
    filter: String,
    only: u8,
    verdicts: Vec<Verdict>,
    verdict_key: Option<(String, usize)>,
    view: Vec<usize>,
    view_key: Option<(String, u8, usize, String)>,
    status: Option<(String, bool)>,
    base: PathBuf,
}

impl Checksummer {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Checksummer {
        let mut src = Sources::new("checksum_src", targets);
        src.exclude_dirs.clear();
        Checksummer {
            src,
            on: [false, false, true, false, false],
            expected: String::new(),
            load_picker: None,
            job: None,
            rows: vec![],
            problems: vec![],
            secs: 0.0,
            filter: String::new(),
            only: 0,
            verdicts: vec![],
            verdict_key: None,
            view: vec![],
            view_key: None,
            status: None,
            base,
        }
    }

    /// Hashes the files of a folder at once, for trying the tool (`--checksum`).
    pub fn demo(&mut self, dir: &Path) {
        self.src.items = vec![dir.to_path_buf()];
        self.on = [true, false, true, false, true];
        self.start();
    }

    fn start(&mut self) {
        if !self.on.iter().any(|b| *b) {
            self.status = Some((tr("Elige al menos un algoritmo.").to_string(), true));
            return;
        }
        if let Err(e) = self.src.filters() {
            self.status = Some((e, true));
            return;
        }
        if self.src.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas.").to_string(), true));
            return;
        }
        let (done_bytes, total_bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (files_done, files_total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let cancel = Arc::new(AtomicBool::new(false));
        let current = Arc::new(Mutex::new(tr("Buscando archivos…").to_string()));
        let (tx, rx) = channel();
        let on = self.on;
        let sources = {
            let s = &self.src;
            (s.items.clone(), s.filters().ok(), s.recursive, s.hidden)
        };
        let (db, tb, fd, ft, c2, cur) = (done_bytes.clone(), total_bytes.clone(), files_done.clone(), files_total.clone(), cancel.clone(), current.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let (items, filters, recursive, hidden) = sources;
            let Some(filters) = filters else { return };
            let (files, mut errs) = crate::sources::collect_files(&items, &filters, recursive, hidden, &c2);
            let sizes: Vec<u64> = files.iter().map(|f| fs::metadata(f).map(|m| m.len()).unwrap_or(0)).collect();
            tb.store(sizes.iter().sum(), Ordering::Relaxed);
            ft.store(files.len(), Ordering::Relaxed);
            let next = AtomicUsize::new(0);
            let out: Mutex<Vec<(usize, Row)>> = Mutex::new(Vec::with_capacity(files.len()));
            let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 6);
            std::thread::scope(|sc| {
                for _ in 0..workers {
                    sc.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() || c2.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(mut c) = cur.try_lock() {
                            *c = files[i].file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        }
                        let row = match hash_file(&files[i], on, &db, &c2) {
                            Ok(h) => Row { path: files[i].clone(), size: sizes[i], hashes: h, error: None },
                            Err(e) => Row { path: files[i].clone(), size: sizes[i], hashes: vec![], error: Some(e) },
                        };
                        fd.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut o) = out.lock() {
                            o.push((i, row));
                        }
                    });
                }
            });
            let mut v = out.into_inner().unwrap_or_default();
            v.sort_by_key(|(i, _)| *i);
            for (_, r) in &v {
                if let Some(e) = &r.error {
                    errs.push(format!("{}: {e}", r.path.display()));
                }
            }
            let _ = tx.send((v.into_iter().map(|(_, r)| r).collect(), errs, started.elapsed().as_secs_f32()));
        });
        self.rows.clear();
        self.view_key = None;
        self.verdict_key = None;
        self.status = None;
        self.problems.clear();
        self.job = Some(Job { rx, done_bytes, total_bytes, files_done, files_total, cancel, current, started: Instant::now() });
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok((rows, errs, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        let _ = job.started;
        self.job = None;
        self.secs = secs;
        self.rows = rows;
        self.problems = errs;
        self.view_key = None;
        self.verdict_key = None;
        if cancelled {
            self.status = Some((tr("Cancelado: se muestra lo calculado hasta ahora").to_string(), true));
        } else if self.rows.is_empty() {
            self.status = Some((tr("No hay archivos que calcular con esos filtros.").to_string(), true));
        }
    }

    fn refresh(&mut self) {
        let key = (self.expected.clone(), self.rows.len());
        if self.verdict_key.as_ref() != Some(&key) {
            let exp = parse_expected(&self.expected);
            self.verdicts = self.rows.iter().map(|r| verdict(r, &exp)).collect();
            self.verdict_key = Some(key);
            self.view_key = None;
        }
        let vkey = (self.filter.clone(), self.only, self.rows.len(), self.expected.clone());
        if self.view_key.as_ref() != Some(&vkey) {
            let q = self.filter.to_lowercase();
            self.view = (0..self.rows.len())
                .filter(|&i| {
                    let r = &self.rows[i];
                    (q.is_empty() || r.path.to_string_lossy().to_lowercase().contains(&q))
                        && match self.only {
                            1 => self.verdicts[i] == Verdict::Ok,
                            2 => self.verdicts[i] == Verdict::Mismatch,
                            3 => r.error.is_some(),
                            _ => true,
                        }
                })
                .collect();
            self.view_key = Some(vkey);
        }
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.base).unwrap_or(p).to_string_lossy().to_string()
    }

    fn save_sums(&mut self, algo: usize, events: &mut Vec<ToolEvent>) {
        let mut out = String::new();
        for r in self.rows.iter().filter(|r| r.error.is_none() && !r.hashes.get(algo).map(|h| h.is_empty()).unwrap_or(true)) {
            out.push_str(&format!("{}  {}\n", r.hashes[algo], self.rel(&r.path).replace('\\', "/")));
        }
        let file = fsops::unique_name(&self.base, &format!("{}SUMS.txt", ALGOS[algo].0.replace('-', "")));
        self.status = Some(match fs::write(&file, out) {
            Ok(()) => {
                events.push(ToolEvent::Reload);
                (trf!("Guardado en {}", file.display()), false)
            }
            Err(e) => (trf!("No se pudo guardar: {}", e), true),
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll();
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            self.src.ui(ui, selection, cwd, tr("ARCHIVOS Y CARPETAS"));
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("ALGORITMOS")).small().strong().color(theme::p().sub));
                for (k, (name, _)) in ALGOS.iter().enumerate() {
                    ui.checkbox(&mut self.on[k], *name);
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("COMPARAR CON")).small().strong().color(theme::p().sub));
                if ui.small_button(tr("📂 Cargar archivo de sumas…")).on_hover_text(tr("Un archivo con líneas «suma  nombre», como SHA256SUMS o los .md5/.sha1")).clicked() {
                    self.load_picker = Some(Picker::new(self.base.clone()));
                }
                if !self.expected.is_empty() && ui.small_button(tr("Borrar")).clicked() {
                    self.expected.clear();
                }
            });
            ui.add(egui::TextEdit::multiline(&mut self.expected).hint_text(tr("Pega aquí una suma, o varias líneas «suma  nombre»")).desired_rows(2).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
        });
        let ctx = ui.ctx().clone();
        let loaded = picker_window(&ctx, "checksum_load", &mut self.load_picker);
        if let Some(f) = loaded.iter().find(|p| p.is_file()) {
            match fs::read(f) {
                Ok(b) => self.expected = String::from_utf8_lossy(&b).to_string(),
                Err(e) => self.status = Some((trf!("No se pudo leer «{}»: {}", f.display(), e), true)),
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("▶ Calcular")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy, btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let (d, t) = (job.done_bytes.load(Ordering::Relaxed), job.total_bytes.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { (d as f64 / t as f64) as f32 } else { 0.0 }).desired_width(200.0).show_percentage());
                let secs = job.started.elapsed().as_secs_f64().max(0.1);
                ui.label(RichText::new(format!("{}/{} · {}/s · {}", job.files_done.load(Ordering::Relaxed), job.files_total.load(Ordering::Relaxed), human_size((d as f64 / secs) as u64), job.current.lock().map(|c| c.clone()).unwrap_or_default())).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if self.rows.is_empty() {
            return;
        }
        self.refresh();
        let (ok, bad, errs) = (self.verdicts.iter().filter(|v| **v == Verdict::Ok).count(), self.verdicts.iter().filter(|v| **v == Verdict::Mismatch).count(), self.rows.iter().filter(|r| r.error.is_some()).count());
        let total: u64 = self.rows.iter().map(|r| r.size).sum();
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("{} archivo(s) · {} · {} s", self.rows.len(), human_size(total), format!("{:.1}", self.secs))).strong());
            if !self.expected.trim().is_empty() {
                ui.label(RichText::new(trf!("✔ {} coinciden", ok)).color(theme::p().ok));
                if bad > 0 {
                    ui.label(RichText::new(trf!("✘ {} no coinciden", bad)).color(theme::p().danger).strong());
                }
            }
            if errs > 0 {
                ui.label(RichText::new(trf!("⚠ {} con error", errs)).color(theme::p().danger));
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar por nombre")).desired_width(180.0));
            for (k, l) in ["Todos", "Coinciden", "No coinciden", "Con error"].iter().enumerate() {
                ui.selectable_value(&mut self.only, k as u8, tr(l));
            }
            ui.separator();
            let first = self.on.iter().position(|b| *b);
            if ui.add_enabled(first.is_some(), egui::Button::new(tr("📋 Copiar"))).on_hover_text(tr("Copia «suma  nombre» de cada archivo, con el primer algoritmo elegido")).clicked() {
                if let Some(a) = first {
                    let t: Vec<String> = self.rows.iter().filter(|r| r.error.is_none()).map(|r| format!("{}  {}", r.hashes[a], self.rel(&r.path).replace('\\', "/"))).collect();
                    events.push(ToolEvent::Clipboard(t.join("\n")));
                }
            }
            for (k, (name, _)) in ALGOS.iter().enumerate() {
                if self.on[k] && ui.button(trf!("💾 {}SUMS", name.replace('-', ""))).on_hover_text(tr("Guarda una lista «suma  nombre» en la carpeta actual")).clicked() {
                    self.save_sums(k, events);
                }
            }
        });
        // One column per chosen algorithm; hashes are long, so they are cut and copied on click.
        let cols: Vec<usize> = (0..5).filter(|k| self.on[*k]).collect();
        let row_h = 22.0;
        let mut copy = None;
        let mut nav = None;
        let name_w = 230.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            cell(ui, 18.0, 18.0, |_| {});
            cell(ui, name_w, 18.0, |ui| ui.label(RichText::new(tr("Archivo")).small().strong().color(theme::p().sub)));
            cell(ui, 70.0, 18.0, |ui| ui.label(RichText::new(tr("Tamaño")).small().strong().color(theme::p().sub)));
            for &k in &cols {
                cell(ui, if k == 0 { 80.0 } else { 330.0 }, 18.0, |ui| ui.label(RichText::new(ALGOS[k].0).small().strong().color(theme::p().sub)));
            }
        });
        egui::ScrollArea::both().id_salt("checksum_rows").max_height(crate::tools::grow(ui, 300.0)).auto_shrink([false, false]).show_rows(ui, row_h, self.view.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &i in &self.view[range] {
                let r = &self.rows[i];
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    let (txt, col) = match (self.verdicts[i], &r.error) {
                        (_, Some(_)) => ("⚠", theme::p().danger),
                        (Verdict::Ok, _) => ("✔", theme::p().ok),
                        (Verdict::Mismatch, _) => ("✘", theme::p().danger),
                        (Verdict::NoReference, _) => ("?", theme::p().muted),
                        _ => ("", theme::p().muted),
                    };
                    cell(ui, 18.0, row_h, |ui| ui.label(RichText::new(txt).color(col).strong()));
                    let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let l = cell(ui, name_w, row_h, |ui| ui.add(egui::Label::new(RichText::new(name).color(theme::p().text)).truncate().sense(egui::Sense::click()))).on_hover_text(format!("{}\n{}", r.path.display(), tr("Clic: ir a su carpeta")));
                    if l.clicked() {
                        nav = r.path.parent().map(Path::to_path_buf);
                    }
                    cell(ui, 70.0, row_h, |ui| ui.label(RichText::new(human_size(r.size)).small().color(theme::p().sub)));
                    match &r.error {
                        Some(e) => {
                            ui.label(RichText::new(e).small().color(theme::p().danger));
                        }
                        None => {
                            for &k in &cols {
                                let h = r.hashes.get(k).cloned().unwrap_or_default();
                                let shown = if h.len() > 40 { format!("{}…", &h[..40]) } else { h.clone() };
                                let w = if k == 0 { 80.0 } else { 330.0 };
                                if cell(ui, w, row_h, |ui| ui.add(egui::Label::new(RichText::new(shown).monospace().small()).truncate().sense(egui::Sense::click()))).on_hover_text(format!("{}: {}\n{}", ALGOS[k].0, h, tr("Clic: copiar"))).clicked() {
                                    copy = Some(h);
                                }
                            }
                        }
                    }
                });
            }
        });
        if let Some(h) = copy {
            events.push(ToolEvent::Clipboard(h));
        }
        if let Some(p) = nav {
            events.push(ToolEvent::Navigate(p));
        }
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in self.problems.iter().take(50) {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hashes_parsing_and_verdicts() {
        let d = std::env::temp_dir().join(format!("sum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("abc.txt"), b"abc").unwrap();
        let h = hash_file(&d.join("abc.txt"), [true; 5], &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(h[0], "352441c2");
        assert_eq!(h[1], "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(h[2], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(h[3].starts_with("ddaf35a193617aba") && h[3].len() == 128);
        assert_eq!(h[4], "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
        let only = hash_file(&d.join("abc.txt"), [false, false, true, false, false], &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        assert!(only[0].is_empty() && !only[2].is_empty());
        let row = Row { path: d.join("abc.txt"), size: 3, hashes: h.clone(), error: None };
        // A bare value, a "hash  name" line, a BSD line and a wrong one.
        assert_eq!(verdict(&row, &parse_expected(&h[2].to_uppercase())), Verdict::Ok);
        assert_eq!(verdict(&row, &parse_expected(&format!("{}  abc.txt", h[1]))), Verdict::Ok);
        assert_eq!(verdict(&row, &parse_expected(&format!("SHA256 (sub/abc.txt) = {}", h[2]))), Verdict::Ok);
        assert_eq!(verdict(&row, &parse_expected(&format!("{}  *abc.txt", "0".repeat(64)))), Verdict::Mismatch);
        assert_eq!(verdict(&row, &parse_expected(&format!("{}  otro.txt", h[2]))), Verdict::NoReference);
        assert_eq!(verdict(&row, &parse_expected("")), Verdict::None);
        assert_eq!(verdict(&row, &parse_expected("no es una suma")), Verdict::None);
        fs::remove_dir_all(&d).unwrap();
    }
}
