//! Organise: sorts the files of one or more folders into subfolders by type, extension, date,
//! first letter, size, or rules of your own. A preview first, then move or copy, and the last
//! move can be undone.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::{DestField, RootList};
use crate::theme;
use crate::tools::ToolEvent;
use chrono::{Datelike, Local};
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Instant, SystemTime};

const MODES: [(&str, &str); 13] = [
    ("Por tipo", "Imágenes, vídeos, música, documentos, comprimidos…"),
    ("Por extensión", "Una carpeta por cada extensión: PDF, JPG, MP3…"),
    ("Por fecha", "Carpetas por año y mes, o con la plantilla que quieras."),
    ("Por inicial", "Una carpeta por la primera letra del nombre: A, B, C…"),
    ("Por tamaño", "Pequeños, medianos, grandes y enormes."),
    ("Reglas propias", "Tú decides qué patrón va a qué carpeta."),
    ("Por tipo y año", "Primero el tipo y dentro el año: Imágenes/2024, Documentos/2023…"),
    ("Por antigüedad", "Hoy, esta semana, este mes, este año y más antiguos."),
    ("Por orientación", "Fotos horizontales, verticales y cuadradas."),
    ("Por artista y álbum", "Música ordenada con sus etiquetas: Artista/Álbum."),
    ("Por cámara", "Fotos agrupadas por la marca y el modelo de la cámara (EXIF)."),
    ("Por prefijo del nombre", "La primera palabra del nombre, hasta un espacio, guion o punto bajo."),
    ("Por megapíxeles", "Imágenes por resolución: menos de 1 MP, de 1 a 5, de 5 a 12 y más de 12."),
];
/// The order modes are shown in (the number of each is what profiles remember).
const ORDER: [u8; 13] = [0, 6, 1, 2, 7, 3, 4, 8, 12, 10, 9, 11, 5];
const DATE_SRC: [&str; 3] = ["Fecha de modificación", "Fecha de creación", "Fecha de la foto (EXIF), si la tiene"];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

const CATEGORIES: [(&str, &str); 9] = [
    ("Imágenes", "jpg jpeg png gif bmp webp tif tiff heic heif avif svg raw dng cr2 nef arw ico"),
    ("Vídeos", "mp4 mkv avi mov wmv flv webm m4v mpg mpeg 3gp"),
    ("Música", "mp3 flac wav ogg opus m4a aac wma aiff ape mid midi"),
    ("Documentos", "pdf doc docx odt rtf txt md epub mobi djvu tex"),
    ("Hojas de cálculo", "xls xlsx ods csv tsv"),
    ("Presentaciones", "ppt pptx odp key"),
    ("Comprimidos", "zip rar 7z tar gz bz2 xz zst tgz iso img"),
    ("Programas", "exe msi dmg deb rpm appimage flatpak apk pkg bat cmd"),
    ("Código", "rs py js ts c cpp h java go sh html css json xml yml yaml toml sql php rb cs kt swift"),
];

fn category(ext: &str) -> &'static str {
    CATEGORIES.iter().find(|(_, l)| l.split(' ').any(|e| e == ext)).map(|(n, _)| *n).unwrap_or("Otros")
}

const MONTHS: [[&str; 12]; 4] = [
    ["enero", "febrero", "marzo", "abril", "mayo", "junio", "julio", "agosto", "septiembre", "octubre", "noviembre", "diciembre"],
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"],
    ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"],
    ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre"],
];

fn clean_part(s: &str) -> String {
    let t: String = s.chars().map(|c| if matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0') { '-' } else { c }).collect();
    t.trim().trim_matches('.').to_string()
}

fn expand_date(tpl: &str, d: chrono::DateTime<Local>) -> String {
    let month_name = MONTHS[crate::i18n::lang().min(3)][d.month0() as usize];
    let mut out = tpl.to_string();
    for (k, v) in [
        ("{año}", format!("{:04}", d.year())),
        ("{year}", format!("{:04}", d.year())),
        ("{mes}", format!("{:02}", d.month())),
        ("{month}", format!("{:02}", d.month())),
        ("{mes_nombre}", format!("{:02} {}", d.month(), month_name)),
        ("{dia}", format!("{:02}", d.day())),
        ("{day}", format!("{:02}", d.day())),
        ("{semana}", format!("{:02}", d.iso_week().week())),
        ("{trimestre}", format!("T{}", d.month0() / 3 + 1)),
    ] {
        out = out.replace(k, &v);
    }
    out.split('/').map(clean_part).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")
}

#[derive(Clone)]
struct Cfg {
    mode: u8,
    date_src: u8,
    date_tpl: String,
    rules: Vec<(Regex, String)>,
    other: String,
    recursive: bool,
    hidden: bool,
    include: Option<Regex>,
    exclude: Option<Regex>,
    dest: Option<PathBuf>,
    conflict: u8,
    copy: bool,
    clean_empty: bool,
}

struct Mv {
    from: PathBuf,
    to: PathBuf,
    folder: String,
    size: u64,
    on: bool,
    skip: Option<&'static str>,
}

struct Plan {
    moves: Vec<Mv>,
    scanned: usize,
}

fn mtime_of(meta: &fs::Metadata, src: u8) -> SystemTime {
    match src {
        1 => meta.created().or_else(|_| meta.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
        _ => meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }
}

/// The folder (relative, «/» separated) a file belongs in; None leaves it where it is.
fn classify(path: &Path, meta: &fs::Metadata, c: &Cfg) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match c.mode {
        0 => Some(tr(category(&ext)).to_string()),
        1 => Some(if ext.is_empty() { tr("Sin extensión").to_string() } else { ext.to_uppercase() }),
        2 => {
            let t: chrono::DateTime<Local> = if c.date_src == 2 {
                crate::rename::read_tags(path).taken.and_then(|n| n.and_local_timezone(Local).single()).unwrap_or_else(|| mtime_of(meta, 0).into())
            } else {
                mtime_of(meta, c.date_src).into()
            };
            let f = expand_date(&c.date_tpl, t);
            (!f.is_empty()).then_some(f)
        }
        3 => {
            let first = name.trim_start_matches(|ch: char| !ch.is_alphanumeric()).chars().next();
            Some(match first {
                Some(ch) if ch.is_ascii_digit() => "0-9".to_string(),
                Some(ch) => ch.to_uppercase().collect(),
                None => "#".to_string(),
            })
        }
        4 => {
            let s = meta.len();
            Some(
                tr(if s < 1 << 20 {
                    "Pequeños (menos de 1 MB)"
                } else if s < 100 << 20 {
                    "Medianos (1 a 100 MB)"
                } else if s < 1 << 30 {
                    "Grandes (100 MB a 1 GB)"
                } else {
                    "Enormes (más de 1 GB)"
                })
                .to_string(),
            )
        }
        6 => {
            let t: chrono::DateTime<Local> = mtime_of(meta, c.date_src.min(1)).into();
            Some(format!("{}/{}", tr(category(&ext)), t.year()))
        }
        7 => {
            let t: chrono::DateTime<Local> = mtime_of(meta, c.date_src.min(1)).into();
            let days = (Local::now().date_naive() - t.date_naive()).num_days();
            Some(
                tr(if days <= 0 {
                    "Hoy"
                } else if days < 7 {
                    "Esta semana"
                } else if days < 31 {
                    "Este mes"
                } else if days < 365 {
                    "Este año"
                } else {
                    "Más antiguos"
                })
                .to_string(),
            )
        }
        8 => {
            let (w, h) = imagesize::size(path).ok().map(|d| (d.width, d.height))?;
            Some(tr(if w > h { "Horizontales" } else if h > w { "Verticales" } else { "Cuadradas" }).to_string())
        }
        9 => {
            let t = crate::rename::read_tags(path);
            Some(match (t.artist.map(|a| clean_part(&a)).filter(|a| !a.is_empty()), t.album.map(|a| clean_part(&a)).filter(|a| !a.is_empty())) {
                (Some(a), Some(b)) => format!("{a}/{b}"),
                (Some(a), None) => a,
                _ => tr("Sin etiquetas").to_string(),
            })
        }
        10 => {
            let cam = crate::rename::read_tags(path).camera.map(|a| clean_part(&a)).filter(|a| !a.is_empty());
            Some(cam.unwrap_or_else(|| tr("Sin cámara").to_string()))
        }
        11 => {
            let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let first = stem.split([' ', '-', '_', '.']).find(|p| !p.is_empty()).unwrap_or("");
            let f = clean_part(first);
            Some(if f.is_empty() { "#".to_string() } else { f })
        }
        12 => {
            let (w, h) = imagesize::size(path).ok().map(|d| (d.width, d.height))?;
            let mp = (w * h) as f64 / 1_000_000.0;
            Some(tr(if mp < 1.0 { "Menos de 1 MP" } else if mp < 5.0 { "De 1 a 5 MP" } else if mp < 12.0 { "De 5 a 12 MP" } else { "Más de 12 MP" }).to_string())
        }
        _ => c.rules.iter().find(|(re, _)| re.is_match(&name)).map(|(_, f)| f.clone()).or_else(|| (!c.other.trim().is_empty()).then(|| c.other.clone())).map(|f| f.split(['/', '\\']).map(clean_part).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")).filter(|f| !f.is_empty()),
    }
}

fn build_plan(roots: &[PathBuf], c: &Cfg, cancel: &AtomicBool) -> (Plan, Vec<String>) {
    let mut moves: Vec<Mv> = vec![];
    let mut errs = vec![];
    let mut scanned = 0;
    let mut used: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    errs.push(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                let path = e.path();
                if !c.hidden && name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                if meta.is_dir() {
                    if c.recursive {
                        stack.push(path);
                    }
                    continue;
                }
                if !meta.is_file() {
                    continue;
                }
                scanned += 1;
                if c.include.as_ref().map(|re| !re.is_match(&name)).unwrap_or(false) || c.exclude.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                    continue;
                }
                let Some(folder) = classify(&path, &meta, c) else { continue };
                let base = c.dest.clone().unwrap_or_else(|| root.clone());
                let dir_to = folder.split('/').fold(base, |p, part| p.join(part));
                if path.parent() == Some(dir_to.as_path()) {
                    continue; // already where it belongs
                }
                let mut to = dir_to.join(&name);
                let mut skip = None;
                let taken = |p: &PathBuf, used: &HashSet<PathBuf>| used.contains(p) || p.exists();
                if taken(&to, &used) {
                    match c.conflict {
                        2 => skip = Some("ya existe en la carpeta de destino"),
                        1 if !used.contains(&to) => {}
                        _ => {
                            let (stem, ext) = match name.rfind('.') {
                                Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
                                _ => (name.clone(), String::new()),
                            };
                            let mut k = 2;
                            loop {
                                let cand = dir_to.join(format!("{stem} ({k}){ext}"));
                                if !taken(&cand, &used) {
                                    to = cand;
                                    break;
                                }
                                k += 1;
                            }
                        }
                    }
                }
                used.insert(to.clone());
                moves.push(Mv { from: path, to, folder, size: meta.len(), on: skip.is_none(), skip });
            }
        }
    }
    (Plan { moves, scanned }, errs)
}

#[derive(Default)]
struct Report {
    done: usize,
    skipped: usize,
    bytes: u64,
    errors: Vec<String>,
    undo: Vec<(PathBuf, PathBuf)>,
    secs: f32,
}

fn execute(moves: Vec<Mv>, c: &Cfg, roots: &[PathBuf], cancel: &AtomicBool) -> Report {
    let start = Instant::now();
    let mut rep = Report::default();
    for m in moves.iter().filter(|m| m.on && m.skip.is_none()) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Some(p) = m.to.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                rep.errors.push(format!("{}: {e}", p.display()));
                continue;
            }
        }
        let r = if c.copy { fs::copy(&m.from, &m.to).map(|_| ()) } else { fsops::move_path(&m.from, &m.to) };
        match r {
            Ok(()) => {
                rep.done += 1;
                rep.bytes += m.size;
                if !c.copy {
                    rep.undo.push((m.to.clone(), m.from.clone()));
                    if c.clean_empty {
                        let root = roots.iter().find(|r| m.from.starts_with(r));
                        let mut cur = m.from.parent();
                        while let (Some(d), Some(root)) = (cur, root) {
                            if d == root.as_path() || fs::remove_dir(d).is_err() {
                                break;
                            }
                            cur = d.parent();
                        }
                    }
                }
            }
            Err(e) => rep.errors.push(format!("{}: {e}", m.from.display())),
        }
    }
    rep.skipped = moves.iter().filter(|m| m.skip.is_some() || !m.on).count();
    rep.secs = start.elapsed().as_secs_f32();
    rep
}

fn undo(pairs: &[(PathBuf, PathBuf)]) -> (usize, Vec<String>) {
    let (mut n, mut errs) = (0, vec![]);
    for (from, to) in pairs.iter().rev() {
        if let Some(p) = to.parent() {
            let _ = fs::create_dir_all(p);
        }
        if to.exists() {
            errs.push(format!("{}: {}", to.display(), tr("ya existe un archivo con ese nombre")));
            continue;
        }
        match fsops::move_path(from, to) {
            Ok(()) => {
                n += 1;
                let mut cur = from.parent();
                while let Some(d) = cur {
                    if fs::remove_dir(d).is_err() {
                        break;
                    }
                    cur = d.parent();
                }
            }
            Err(e) => errs.push(format!("{}: {e}", from.display())),
        }
    }
    (n, errs)
}

enum Msg {
    Plan(Plan, Vec<String>),
    Done(Report),
}

struct Job {
    rx: Receiver<Msg>,
    cancel: std::sync::Arc<AtomicBool>,
    started: Instant,
    executing: bool,
}

pub struct Organizer {
    roots: RootList,
    mode: u8,
    date_src: u8,
    date_tpl: String,
    rules: Vec<(String, String)>,
    other: String,
    recursive: bool,
    hidden: bool,
    include: String,
    exclude: String,
    filter_regex: bool,
    dest_other: bool,
    dest: DestField,
    copy: bool,
    conflict: u8,
    clean_empty: bool,
    job: Option<Job>,
    plan: Option<Plan>,
    plan_sig: String,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    undo: Vec<Vec<(PathBuf, PathBuf)>>,
    filter: String,
    view_cache: crate::tools::ViewCache,
    roots_snapshot: Vec<PathBuf>,
}

impl Organizer {
    pub fn new(targets: Vec<PathBuf>) -> Organizer {
        Organizer {
            roots: RootList::new(targets.into_iter().filter(|p| p.is_dir()).collect(), "organize_roots"),
            mode: 0,
            date_src: 0,
            date_tpl: "{año}/{mes}".into(),
            rules: vec![("*.pdf, *.docx".into(), "Documentos".into()), ("*factura*".into(), "Facturas".into())],
            other: String::new(),
            recursive: false,
            hidden: false,
            include: String::new(),
            exclude: String::new(),
            filter_regex: false,
            dest_other: false,
            dest: DestField::new("organize_dest"),
            copy: false,
            conflict: 0,
            clean_empty: true,
            job: None,
            plan: None,
            plan_sig: String::new(),
            status: None,
            problems: vec![],
            undo: vec![],
            filter: String::new(),
            view_cache: Default::default(),
            roots_snapshot: vec![],
        }
    }

    /// Adds the folder and previews at once, for trying the tool (`--organize`).
    pub fn demo(&mut self, dir: &Path) {
        self.roots.add(dir.to_path_buf());
        self.start_plan();
    }

    fn sig(&self) -> String {
        format!(
            "{:?}|{}|{}|{}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.roots.roots, self.mode, self.date_src, self.date_tpl, self.rules, self.other, self.recursive, self.hidden, self.include, self.exclude, self.filter_regex, self.dest_other, self.dest.text, self.copy, self.conflict, self.clean_empty
        )
    }

    fn config(&self) -> Result<Cfg, String> {
        if self.roots.roots.is_empty() {
            return Err(tr("Añade al menos una carpeta que organizar.").to_string());
        }
        let m = |t: &str| crate::rename::name_matcher(t, self.filter_regex).map_err(|e| trf!("Expresión no válida: {}", e));
        let mut rules = vec![];
        if self.mode == 5 {
            for (pat, folder) in &self.rules {
                if pat.trim().is_empty() || folder.trim().is_empty() {
                    continue;
                }
                if let Some(re) = m(pat)? {
                    rules.push((re, folder.trim().to_string()));
                }
            }
            if rules.is_empty() {
                return Err(tr("Escribe al menos una regla (patrón y carpeta).").to_string());
            }
        }
        if self.mode == 2 && expand_date(&self.date_tpl, Local::now()).is_empty() {
            return Err(tr("La plantilla de fecha está vacía.").to_string());
        }
        if self.dest_other && self.dest.text.trim().is_empty() {
            return Err(tr("Elige la carpeta de destino.").to_string());
        }
        Ok(Cfg {
            mode: self.mode,
            date_src: self.date_src,
            date_tpl: self.date_tpl.clone(),
            rules,
            other: self.other.clone(),
            recursive: self.recursive,
            hidden: self.hidden,
            include: m(&self.include)?,
            exclude: m(&self.exclude)?,
            dest: self.dest_other.then(|| self.dest.path()),
            conflict: self.conflict,
            copy: self.copy,
            clean_empty: self.clean_empty,
        })
    }

    fn start_plan(&mut self) {
        let cfg = match self.config() {
            Ok(c) => c,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let roots = self.roots.roots.clone();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let (p, e) = build_plan(&roots, &cfg, &c2);
            let _ = tx.send(Msg::Plan(p, e));
        });
        self.plan = None;
        self.status = None;
        self.problems.clear();
        self.plan_sig = self.sig();
        self.job = Some(Job { rx, cancel, started: Instant::now(), executing: false });
    }

    fn start_run(&mut self) {
        let Some(plan) = self.plan.take() else { return };
        let Ok(cfg) = self.config() else { return };
        let roots = self.roots.roots.clone();
        self.roots_snapshot = roots.clone();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Done(execute(plan.moves, &cfg, &roots, &c2)));
        });
        self.job = Some(Job { rx, cancel, started: Instant::now(), executing: true });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok(msg) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        let _ = job.started;
        self.job = None;
        match msg {
            Msg::Plan(plan, errs) => {
                self.problems = errs;
                let n = plan.moves.iter().filter(|m| m.skip.is_none()).count();
                self.status = Some(if cancelled {
                    (tr("Cancelado").to_string(), true)
                } else if n == 0 {
                    (trf!("No hay nada que mover entre {} archivos revisados.", plan.scanned), false)
                } else {
                    (tr("Revisa la vista previa y pulsa «Organizar ahora».").to_string(), false)
                });
                self.plan = Some(plan);
            }
            Msg::Done(rep) => {
                let verb = if self.copy { tr("copiados") } else { tr("movidos") };
                self.status = Some((trf!("{} archivos {} · {} · {} omitidos · {} error(es) · {} s", rep.done, verb, human_size(rep.bytes), rep.skipped, rep.errors.len(), format!("{:.1}", rep.secs)), !rep.errors.is_empty()));
                self.problems = rep.errors;
                if !rep.undo.is_empty() {
                    self.undo.push(rep.undo);
                }
                events.push(ToolEvent::Reload);
            }
        }
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("organize_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.roots.ui(ui, selection, cwd, tr("CARPETAS A ORGANIZAR"));
                ui.separator();
                self.mode_ui(ui);
                self.options_ui(ui, cwd, selection);
            });
            ui.separator();
            self.action_ui(ui, events);
            self.plan_ui(ui);
            if !self.problems.is_empty() {
                ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                    egui::ScrollArea::vertical().max_height(90.0).id_salt("organize_err").show(ui, |ui| {
                        for p in &self.problems {
                            ui.label(RichText::new(p).small().color(theme::p().danger));
                        }
                    });
                });
            }
        });
    }

    fn mode_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("ORGANIZAR")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for k in ORDER {
                let (name, desc) = MODES[k as usize];
                ui.selectable_value(&mut self.mode, k, tr(name)).on_hover_text(tr(desc));
            }
        });
        ui.label(RichText::new(tr(MODES[self.mode as usize].1)).small().color(theme::p().sub));
        match self.mode {
            2 => {
                ui.horizontal_wrapped(|ui| {
                    crate::ui_kit::label(ui, tr("Plantilla"));
                    ui.add(egui::TextEdit::singleline(&mut self.date_tpl).desired_width(240.0).font(egui::TextStyle::Monospace)).on_hover_text(tr("Admite {año}, {mes}, {mes_nombre}, {dia}, {semana} y {trimestre}; «/» crea subcarpetas"));
                    for t in ["{año}", "{año}/{mes}", "{año}/{mes_nombre}", "{año}-{mes}-{dia}"] {
                        if ui.small_button(t).clicked() {
                            self.date_tpl = t.to_string();
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    crate::ui_kit::label(ui, tr("Usar"));
                    for (k, l) in DATE_SRC.iter().enumerate() {
                        ui.selectable_value(&mut self.date_src, k as u8, tr(l));
                    }
                });
            }
            6 | 7 => {
                ui.horizontal_wrapped(|ui| {
                    crate::ui_kit::label(ui, tr("Usar"));
                    for (k, l) in DATE_SRC.iter().take(2).enumerate() {
                        ui.selectable_value(&mut self.date_src, k as u8, tr(l));
                    }
                });
            }
            8 | 12 => {
                ui.label(RichText::new(tr("Solo se tocan las imágenes; el resto se queda donde está.")).small().color(theme::p().muted));
            }
            5 => {
                let mut remove = None;
                for (k, (pat, folder)) in self.rules.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✖").clicked() {
                            remove = Some(k);
                        }
                        ui.add(egui::TextEdit::singleline(pat).hint_text("*.pdf, *factura*").desired_width(240.0).font(egui::TextStyle::Monospace));
                        ui.label("»");
                        ui.add(egui::TextEdit::singleline(folder).hint_text(tr("carpeta")).desired_width(200.0));
                    });
                }
                if let Some(k) = remove {
                    self.rules.remove(k);
                }
                ui.horizontal(|ui| {
                    if ui.small_button(tr("➕ Añadir regla")).clicked() {
                        self.rules.push((String::new(), String::new()));
                    }
                    ui.label(tr("Lo demás:"));
                    ui.add(egui::TextEdit::singleline(&mut self.other).hint_text(tr("carpeta (vacío = dejarlo donde está)")).desired_width(260.0));
                });
                ui.label(RichText::new(tr("Gana la primera regla que coincida. Los patrones usan * y ? y se comparan con el nombre.")).small().color(theme::p().muted));
            }
            _ => {}
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Crear las carpetas"));
            ui.selectable_value(&mut self.dest_other, false, tr("Dentro de cada carpeta"));
            ui.selectable_value(&mut self.dest_other, true, tr("En otra carpeta"));
            if self.dest_other {
                self.dest.ui(ui, cwd, selection);
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Los archivos"));
            ui.selectable_value(&mut self.copy, false, tr("Se mueven"));
            ui.selectable_value(&mut self.copy, true, tr("Se copian"));
            crate::ui_kit::label(ui, tr("Si ya existe"));
            for (k, l) in CONFLICTS.iter().enumerate() {
                ui.selectable_value(&mut self.conflict, k as u8, tr(l));
            }
        });
        egui::CollapsingHeader::new(tr("🔎 Qué incluir")).default_open(false).show(ui, |ui| {
            ui.checkbox(&mut self.recursive, tr("Incluir los archivos de las subcarpetas")).on_hover_text(tr("Se sacan de sus subcarpetas y se reparten en las carpetas nuevas."));
            ui.checkbox(&mut self.hidden, tr("Incluir archivos ocultos"));
            ui.checkbox(&mut self.filter_regex, tr("Expresión regular")).on_hover_text(tr("Los patrones de «Solo», «Excluir» y de las reglas propias se leen como expresiones regulares sobre el nombre (sin distinguir mayúsculas)."));
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Solo"));
                ui.add(egui::TextEdit::singleline(&mut self.include).hint_text(if self.filter_regex { "\\.(jpg|png)$" } else { "*.jpg, *.png" }).desired_width(260.0));
            });
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Excluir"));
                ui.add(egui::TextEdit::singleline(&mut self.exclude).hint_text(if self.filter_regex { "\\.tmp$|^importante" } else { "*.tmp, importante*" }).desired_width(260.0));
            });
            ui.checkbox(&mut self.clean_empty, tr("Quitar las subcarpetas que queden vacías")).on_hover_text(tr("Solo al mover archivos."));
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let busy = self.job.is_some();
        let stale = self.plan.is_some() && self.sig() != self.plan_sig;
        let n_on = self.plan.as_ref().map(|p| p.moves.iter().filter(|m| m.on && m.skip.is_none()).count()).unwrap_or(0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(tr("🔍 Vista previa")).strong())).on_hover_text(tr("Muestra qué se haría, sin tocar nada.")).clicked() {
                self.start_plan();
            }
            let btn = egui::Button::new(RichText::new(trf!("📂 Organizar ahora ({})", n_on)).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !stale && n_on > 0, btn).clicked() {
                self.start_run();
            }
            let can_undo = !self.undo.is_empty() && !busy;
            if ui.add_enabled(can_undo, egui::Button::new(tr("↩ Deshacer lo último"))).on_hover_text(tr("Devuelve los archivos movidos en la última organización a donde estaban.")).clicked() {
                if let Some(batch) = self.undo.pop() {
                    let (n, errs) = undo(&batch);
                    self.status = Some((trf!("Deshecho: {} archivos vuelven a su sitio", n), !errs.is_empty()));
                    self.problems = errs;
                    self.plan = None;
                    events.push(ToolEvent::Reload);
                }
            }
            if let Some(job) = &self.job {
                ui.spinner();
                ui.label(tr(if job.executing { "Organizando…" } else { "Revisando…" }));
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        if stale {
            ui.label(RichText::new(tr("Has cambiado las opciones desde la vista previa: vuelve a pulsar «Vista previa».")).small().color(theme::p().danger));
        }
        if let Some((s, err)) = &self.status {
            ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
        }
    }

    fn plan_ui(&mut self, ui: &mut egui::Ui) {
        let Some(plan) = &mut self.plan else { return };
        if plan.moves.is_empty() {
            return;
        }
        ui.separator();
        // Folders and how much goes into each.
        let mut groups: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
        for m in plan.moves.iter().filter(|m| m.on && m.skip.is_none()) {
            let g = groups.entry(m.folder.as_str()).or_default();
            g.0 += 1;
            g.1 += m.size;
        }
        ui.label(RichText::new(trf!("{} archivos revisados · {} se organizarán en {} carpeta(s)", plan.scanned, groups.values().map(|g| g.0).sum::<usize>(), groups.len())).strong());
        ui.horizontal_wrapped(|ui| {
            for (folder, (n, bytes)) in &groups {
                ui.label(RichText::new(format!("📁 {folder} ({n} · {})", human_size(*bytes))).small());
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar la lista")).desired_width(160.0));
            if ui.small_button(tr("Marcar todo")).clicked() {
                plan.moves.iter_mut().filter(|m| m.skip.is_none()).for_each(|m| m.on = true);
            }
            if ui.small_button(tr("Desmarcar todo")).clicked() {
                plan.moves.iter_mut().for_each(|m| m.on = false);
            }
        });
        let q = self.filter.to_lowercase();
        let salt = ((plan.moves.len() as u64) << 24) ^ ((plan.moves.as_ptr() as u64) << 8);
        let view = self.view_cache.get(&self.filter, salt, || plan.moves.iter().enumerate().filter(|(_, m)| q.is_empty() || m.from.to_string_lossy().to_lowercase().contains(&q) || m.folder.to_lowercase().contains(&q)).map(|(i, _)| i).collect());
        let mut toggles = vec![];
        egui::ScrollArea::vertical().id_salt("organize_plan").max_height(crate::tools::grow(ui, 260.0)).auto_shrink([false, false]).show_rows(ui, 22.0, view.len(), |ui, range| {
            for &i in &view[range] {
                let m = &plan.moves[i];
                ui.horizontal(|ui| {
                    let mut on = m.on;
                    if ui.add_enabled(m.skip.is_none(), egui::Checkbox::new(&mut on, "")).changed() {
                        toggles.push(i);
                    }
                    ui.add_sized([70.0, 18.0], egui::Label::new(RichText::new(human_size(m.size)).small().color(theme::p().sub)));
                    ui.add_sized([180.0, 18.0], egui::Label::new(RichText::new(format!("📁 {}", m.folder)).small().color(theme::accent())));
                    let name = m.from.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let tip = format!("{}\n» {}", m.from.display(), m.to.display());
                    let txt = match m.skip {
                        Some(s) => format!("{name}  ({})", tr(s)),
                        None => name,
                    };
                    ui.add(egui::Label::new(RichText::new(txt).color(if m.on { theme::p().text } else { theme::p().muted })).truncate()).on_hover_text(tip);
                });
            }
        });
        for i in toggles {
            plan.moves[i].on = !plan.moves[i].on;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("org-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        for (f, n) in [("foto.jpg", 10), ("video.mp4", 20), ("nota.txt", 5), ("factura-enero.pdf", 7), ("sub/otra.jpg", 3), ("sin_ext", 1), ("Zeta.bin", 2)] {
            fs::write(d.join(f), vec![b'x'; n]).unwrap();
        }
        d
    }

    fn cfg(mode: u8) -> Cfg {
        Cfg { mode, date_src: 0, date_tpl: "{año}/{mes}".into(), rules: vec![], other: String::new(), recursive: false, hidden: false, include: None, exclude: None, dest: None, conflict: 0, copy: false, clean_empty: true }
    }

    fn plan(d: &Path, c: &Cfg) -> Plan {
        build_plan(&[d.to_path_buf()], c, &AtomicBool::new(false)).0
    }

    #[test]
    fn by_type_extension_initial_size_and_date() {
        let d = setup("modes");
        let folders = |c: &Cfg| -> Vec<String> {
            let mut v: Vec<String> = plan(&d, c).moves.iter().map(|m| format!("{}={}", m.folder, m.from.file_name().unwrap().to_string_lossy())).collect();
            v.sort();
            v
        };
        let by_type = folders(&cfg(0));
        assert!(by_type.contains(&"Imágenes=foto.jpg".to_string()) && by_type.contains(&"Vídeos=video.mp4".to_string()) && by_type.contains(&"Otros=sin_ext".to_string()));
        assert!(!by_type.iter().any(|s| s.ends_with("otra.jpg")), "no subfolders unless asked");
        let mut c = cfg(0);
        c.recursive = true;
        assert!(folders(&c).contains(&"Imágenes=otra.jpg".to_string()));
        let by_ext = folders(&cfg(1));
        assert!(by_ext.contains(&"JPG=foto.jpg".to_string()) && by_ext.contains(&"Sin extensión=sin_ext".to_string()));
        let by_initial = folders(&cfg(3));
        assert!(by_initial.contains(&"F=foto.jpg".to_string()) && by_initial.contains(&"Z=Zeta.bin".to_string()));
        assert!(folders(&cfg(4)).iter().all(|s| s.starts_with("Pequeños")));
        let by_date = plan(&d, &cfg(2));
        let now = Local::now();
        assert!(by_date.moves.iter().all(|m| m.folder == format!("{:04}/{:02}", now.year(), now.month())));
        assert_eq!(expand_date("{año}-{mes}-{dia}/{trimestre}", chrono::TimeZone::with_ymd_and_hms(&Local, 2024, 5, 3, 0, 0, 0).unwrap()), "2024-05-03/T2");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn own_rules_conflicts_execute_and_undo() {
        let d = setup("run");
        let mut c = cfg(5);
        c.rules = vec![(crate::rename::name_matcher("*factura*", false).unwrap().unwrap(), "Facturas".into()), (crate::rename::name_matcher("*.jpg", false).unwrap().unwrap(), "Fotos/2024".into())];
        c.recursive = true;
        // A file with the same name already waits in the target folder.
        fs::create_dir_all(d.join("Fotos/2024")).unwrap();
        fs::write(d.join("Fotos/2024/foto.jpg"), "ya estaba").unwrap();
        let p = plan(&d, &c);
        assert_eq!(p.moves.len(), 3, "foto.jpg, otra.jpg and the invoice; the rest has no rule and stays");
        let renamed: Vec<String> = p.moves.iter().map(|m| m.to.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert!(renamed.contains(&"foto (2).jpg".to_string()), "{renamed:?}");
        let rep = execute(p.moves, &c, &[d.clone()], &AtomicBool::new(false));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(rep.done, 3);
        assert!(d.join("Facturas/factura-enero.pdf").exists() && !d.join("foto.jpg").exists());
        assert!(!d.join("sub").exists(), "emptied subfolder removed");
        // Running again finds nothing to move.
        assert!(plan(&d, &c).moves.is_empty());
        let (n, errs) = undo(&rep.undo);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(n, 3);
        assert!(d.join("foto.jpg").exists() && d.join("sub/otra.jpg").exists() && !d.join("Facturas").exists());
        // Skip policy and copy mode.
        c.conflict = 2;
        assert!(plan(&d, &c).moves.iter().any(|m| m.skip.is_some()));
        let mut c2 = cfg(0);
        c2.copy = true;
        c2.dest = Some(d.join("copia"));
        let rep = execute(plan(&d, &c2).moves, &c2, &[d.clone()], &AtomicBool::new(false));
        assert!(rep.undo.is_empty() && d.join("copia/Imágenes/foto.jpg").exists() && d.join("foto.jpg").exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
