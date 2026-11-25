//! Duplicate finder: several folders at once (some can be references that are never touched),
//! by content, name, name and size, or size; filters; staged parallel hashing (size, then the
//! ends of each file, then the whole file); automatic marking and actions on the marked copies.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const MODES: [&str; 4] = ["Contenido idéntico", "Mismo nombre", "Mismo nombre y tamaño", "Mismo tamaño"];
const KEEP: [&str; 6] = [
    "Conservar el más reciente",
    "Conservar el más antiguo",
    "Conservar la ruta más corta",
    "Conservar la ruta más larga",
    "Conservar según el orden de las carpetas",
    "Conservar el de nombre más corto",
];

struct Root {
    path: PathBuf,
    reference: bool,
}

#[derive(Clone)]
pub struct FileInfo {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    root: usize,
}

#[derive(Clone)]
pub struct Group {
    files: Vec<FileInfo>,
}

impl Group {
    fn wasted(&self) -> u64 {
        let total: u64 = self.files.iter().map(|f| f.size).sum();
        total - self.files.iter().map(|f| f.size).max().unwrap_or(0)
    }
}

#[derive(Clone)]
struct Settings {
    roots: Vec<PathBuf>,
    mode: u8,
    name_ci: bool,
    name_no_ext: bool,
    min_bytes: u64,
    max_bytes: u64,
    exts: Option<HashSet<String>>,
    min_age_secs: u64,
    include: Option<Regex>,
    exclude: Option<Regex>,
    exclude_dirs: Option<Regex>,
    skip_hidden: bool,
    skip_empty: bool,
    hardlinks_as_one: bool,
    cross_only: bool,
}

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    files: AtomicUsize,
    done: AtomicUsize,
    total: AtomicUsize,
    bytes: AtomicU64,
}

enum Msg {
    Found(Vec<Group>, Vec<String>, usize),
    Acted(Vec<PathBuf>, Vec<String>, String),
}

struct Job {
    rx: Receiver<Msg>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: std::time::Instant,
}

pub struct Dupes {
    roots: Vec<Root>,
    root_input: String,
    mode: u8,
    name_ci: bool,
    name_no_ext: bool,
    min_text: String,
    max_text: String,
    kinds: [bool; 5],
    min_age_days: u32,
    verify: bool,
    keep_one: bool,
    sort: u8,
    browser: Option<Browser>,
    include: String,
    exclude: String,
    exclude_dirs: String,
    pat_regex: bool,
    skip_hidden: bool,
    skip_empty: bool,
    hardlinks_as_one: bool,
    cross_only: bool,
    job: Option<Job>,
    groups: Vec<Group>,
    scanned: usize,
    marks: HashSet<PathBuf>,
    filter: String,
    keep: usize,
    move_dest: String,
    confirm_delete: bool,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    base: PathBuf,
    collapsed: HashSet<usize>,
}

impl Dupes {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Dupes {
        // Only folders the person selected: nothing is inherited silently from the main window.
        let roots: Vec<Root> = targets.into_iter().filter(|p| p.is_dir()).map(|path| Root { path, reference: false }).collect();
        Dupes {
            roots,
            root_input: String::new(),
            mode: 0,
            name_ci: true,
            name_no_ext: false,
            min_text: "1 KB".into(),
            max_text: String::new(),
            kinds: [false; 5],
            min_age_days: 0,
            verify: true,
            keep_one: true,
            sort: 0,
            browser: None,
            include: String::new(),
            exclude: String::new(),
            exclude_dirs: ".git; node_modules; .cache; $RECYCLE.BIN; .Trash*".into(),
            pat_regex: false,
            skip_hidden: true,
            skip_empty: true,
            hardlinks_as_one: true,
            cross_only: false,
            job: None,
            groups: vec![],
            scanned: 0,
            marks: HashSet::new(),
            filter: String::new(),
            keep: 0,
            move_dest: String::new(),
            confirm_delete: false,
            status: None,
            problems: vec![],
            base,
            collapsed: HashSet::new(),
        }
    }

    /// Starts a search right away, for trying the tool (`--dupes`).
    pub fn demo(&mut self) {
        self.min_text = "0".into();
        let b = self.base.clone();
        self.add_root(b);
        self.start_scan();
    }

    fn add_root(&mut self, p: PathBuf) {
        if p.is_dir() && !self.roots.iter().any(|r| r.path == p) {
            self.roots.push(Root { path: p, reference: false });
        }
    }

    fn settings(&self) -> Result<Settings, String> {
        let m = |t: &str| crate::rename::name_matcher(t, self.pat_regex).map_err(|e| trf!("Expresión no válida: {}", e));
        let size = |t: &str, what: &str| parse_size(t).map_err(|e| format!("{what}: {e}"));
        let exts: HashSet<String> = KINDS.iter().zip(self.kinds).filter(|(_, on)| *on).flat_map(|((_, l), _)| l.split(' ')).map(str::to_string).collect();
        Ok(Settings {
            roots: self.roots.iter().map(|r| r.path.clone()).collect(),
            mode: self.mode,
            name_ci: self.name_ci,
            name_no_ext: self.name_no_ext,
            min_bytes: size(&self.min_text, tr("Tamaño mínimo"))?.unwrap_or(0),
            max_bytes: size(&self.max_text, tr("máximo"))?.unwrap_or(u64::MAX),
            exts: (!exts.is_empty()).then_some(exts),
            min_age_secs: self.min_age_days as u64 * 86_400,
            include: m(&self.include)?,
            exclude: m(&self.exclude)?,
            exclude_dirs: crate::rename::name_matcher(&self.exclude_dirs, false).map_err(|e| trf!("Expresión no válida: {}", e))?,
            skip_hidden: self.skip_hidden,
            skip_empty: self.skip_empty,
            hardlinks_as_one: self.hardlinks_as_one,
            cross_only: self.cross_only,
        })
    }

    fn start_scan(&mut self) {
        let s = match self.settings() {
            Ok(s) => s,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        std::thread::spawn(move || {
            let (groups, errs, scanned) = scan(&s, &p2, &c2);
            let _ = tx.send(Msg::Found(groups, errs, scanned));
        });
        self.groups.clear();
        self.marks.clear();
        self.collapsed.clear();
        self.status = None;
        self.problems.clear();
        self.job = Some(Job { rx, progress, cancel, started: std::time::Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok(msg) = job.rx.try_recv() else { return };
        let secs = job.started.elapsed().as_secs_f32();
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        match msg {
            Msg::Found(groups, errs, scanned) => {
                self.groups = groups;
                self.scanned = scanned;
                self.problems = errs;
                let msg = if cancelled { tr("Búsqueda cancelada: se muestra lo encontrado hasta ahora").to_string() } else { trf!("Búsqueda terminada en {} s", format!("{secs:.1}")) };
                self.status = Some((msg, cancelled));
                self.sort_groups();
                self.auto_mark();
            }
            Msg::Acted(gone, errs, msg) => {
                let gone: HashSet<PathBuf> = gone.into_iter().collect();
                for g in &mut self.groups {
                    g.files.retain(|f| !gone.contains(&f.path));
                }
                self.groups.retain(|g| g.files.len() > 1);
                self.marks.retain(|m| !gone.contains(m));
                self.problems = errs;
                self.status = Some((msg, !self.problems.is_empty()));
                events.push(ToolEvent::Reload);
            }
        }
    }

    fn is_reference(&self, f: &FileInfo) -> bool {
        self.roots.get(f.root).map(|r| r.reference).unwrap_or(false)
    }

    /// Marks every copy but one in each group (never files in reference folders).
    fn auto_mark(&mut self) {
        self.marks.clear();
        for g in &self.groups {
            let has_ref = g.files.iter().any(|f| self.is_reference(f));
            let keeper = if has_ref {
                None
            } else {
                let cmp = |a: &&FileInfo, b: &&FileInfo| match self.keep {
                    0 => b.modified.cmp(&a.modified),
                    1 => a.modified.cmp(&b.modified),
                    2 => a.path.as_os_str().len().cmp(&b.path.as_os_str().len()),
                    3 => b.path.as_os_str().len().cmp(&a.path.as_os_str().len()),
                    4 => a.root.cmp(&b.root).then(a.path.cmp(&b.path)),
                    _ => a.path.file_name().map(|n| n.len()).cmp(&b.path.file_name().map(|n| n.len())),
                };
                g.files.iter().min_by(|a, b| cmp(a, b).then(a.path.cmp(&b.path))).map(|f| f.path.clone())
            };
            for f in &g.files {
                if Some(&f.path) != keeper.as_ref() && !self.is_reference(f) {
                    self.marks.insert(f.path.clone());
                }
            }
        }
    }

    /// Groups whose every copy is marked: acting on them would leave nothing.
    fn fully_marked(&self, g: &Group) -> bool {
        g.files.iter().all(|f| self.marks.contains(&f.path) || self.is_reference(f)) && !g.files.iter().any(|f| self.is_reference(f))
    }

    fn marked_files(&self) -> Vec<FileInfo> {
        self.groups
            .iter()
            .filter(|g| !(self.keep_one && self.fully_marked(g)))
            .flat_map(|g| g.files.iter())
            .filter(|f| self.marks.contains(&f.path) && !self.is_reference(f))
            .cloned()
            .collect()
    }

    fn sort_groups(&mut self) {
        match self.sort {
            0 => self.groups.sort_by_key(|g| std::cmp::Reverse(g.wasted())),
            1 => self.groups.sort_by_key(|g| std::cmp::Reverse(g.files.len())),
            2 => self.groups.sort_by_key(|g| std::cmp::Reverse(g.files.iter().map(|f| f.size).max().unwrap_or(0))),
            _ => self.groups.sort_by_key(|g| g.files.first().and_then(|f| f.path.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()),
        }
        self.collapsed.clear();
    }

    fn run_action(&mut self, kind: u8) {
        let files = self.marked_files();
        if files.is_empty() {
            return;
        }
        // For hard links: each marked file points at the first unmarked copy of its group.
        let keepers: HashMap<PathBuf, PathBuf> = self
            .groups
            .iter()
            .flat_map(|g| {
                let keeper = g.files.iter().find(|f| !self.marks.contains(&f.path)).map(|f| f.path.clone());
                g.files.iter().filter_map(move |f| keeper.clone().map(|k| (f.path.clone(), k)))
            })
            .collect();
        let dest = PathBuf::from(self.move_dest.trim());
        let verify = self.verify && self.mode == 0;
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        progress.total.store(files.len(), Ordering::Relaxed);
        std::thread::spawn(move || {
            let mut gone = vec![];
            let mut errs = vec![];
            let mut freed = 0u64;
            for f in &files {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                p2.done.fetch_add(1, Ordering::Relaxed);
                if verify {
                    // Read both again: the files may have changed since the search.
                    let same = keepers.get(&f.path).filter(|k| **k != f.path).map(|k| match (fs::metadata(k), fs::metadata(&f.path)) {
                        (Ok(a), Ok(b)) if a.len() == b.len() => matches!((hash_full(k, &p2, &c2), hash_full(&f.path, &p2, &c2)), (Ok(x), Ok(y)) if x == y),
                        _ => false,
                    });
                    if same != Some(true) {
                        errs.push(format!("{}: {}", f.path.display(), tr("ya no es idéntico a la copia que se conserva (o esta ya no existe); no se ha tocado")));
                        continue;
                    }
                }
                let r: Result<(), String> = match kind {
                    0 => trash::delete(&f.path).map_err(|e| e.to_string()),
                    1 => fs::remove_file(&f.path).map_err(|e| e.to_string()),
                    2 => fs::create_dir_all(&dest)
                        .and_then(|_| fsops::move_path(&f.path, &fsops::unique_name(&dest, &f.path.file_name().unwrap_or_default().to_string_lossy())))
                        .map_err(|e| e.to_string()),
                    _ => match keepers.get(&f.path) {
                        Some(k) => replace_with_hardlink(k, &f.path).map_err(|e| e.to_string()),
                        None => Err(tr("no queda ninguna copia sin marcar en su grupo").to_string()),
                    },
                };
                match r {
                    Ok(()) => {
                        freed += f.size;
                        gone.push(f.path.clone());
                    }
                    Err(e) => errs.push(format!("{}: {e}", f.path.display())),
                }
            }
            let msg = match kind {
                0 => trf!("{} archivo(s) a la papelera · {} liberados al vaciarla", gone.len(), human_size(freed)),
                1 => trf!("{} archivo(s) eliminados · {} liberados", gone.len(), human_size(freed)),
                2 => trf!("{} archivo(s) movidos", gone.len()),
                _ => trf!("{} archivo(s) cambiados por enlaces · {} liberados", gone.len(), human_size(freed)),
            };
            let _ = tx.send(Msg::Acted(gone, errs, msg));
        });
        self.confirm_delete = false;
        self.job = Some(Job { rx, progress, cancel, started: std::time::Instant::now() });
    }

    fn export_csv(&mut self, events: &mut Vec<ToolEvent>) {
        let mut out = String::from("grupo;tamaño_bytes;marcado;modificado;ruta\n");
        for (k, g) in self.groups.iter().enumerate() {
            for f in &g.files {
                let date = fsops::format_time(f.modified);
                let path = f.path.to_string_lossy().replace('"', "\"\"");
                out.push_str(&format!("{};{};{};{};\"{}\"\n", k + 1, f.size, if self.marks.contains(&f.path) { "sí" } else { "no" }, date, path));
            }
        }
        let file = fsops::unique_name(&self.base, "duplicados.csv");
        match fs::write(&file, out) {
            Ok(()) => {
                self.status = Some((trf!("Guardado en {}", file.display()), false));
                events.push(ToolEvent::Reload);
            }
            Err(e) => self.status = Some((trf!("No se pudo guardar: {}", e), true)),
        }
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for p in dropped {
            self.add_root(p);
        }
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            self.roots_ui(ui, selection, cwd);
            ui.separator();
            self.options_ui(ui);
        });
        ui.separator();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🔍 Buscar duplicados")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.roots.is_empty(), btn).clicked() {
                self.start_scan();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
                let total = p.total.load(Ordering::Relaxed);
                let done = p.done.load(Ordering::Relaxed);
                let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).desired_width(180.0).show_percentage());
                ui.label(RichText::new(trf!("{} · {} archivos · {} leídos", stage, p.files.load(Ordering::Relaxed), human_size(p.bytes.load(Ordering::Relaxed)))).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        self.results_ui(ui, events);
        let ctx = ui.ctx().clone();
        self.browser_ui(&ctx);
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                egui::ScrollArea::vertical().max_height(90.0).id_salt("dup_err").show(ui, |ui| {
                    for p in &self.problems {
                        ui.label(RichText::new(p).small().color(theme::p().danger));
                    }
                });
            });
        }
    }

    fn roots_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("CARPETAS DONDE BUSCAR")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("raices").max_height(96.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, r) in self.roots.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la búsqueda")).clicked() {
                        remove = Some(k);
                    }
                    ui.checkbox(&mut r.reference, tr("referencia"))
                        .on_hover_text(tr("Carpeta de referencia: sus archivos nunca se marcan ni se tocan. Sirve para limpiar otras carpetas comparándolas con esta."));
                    let col = if r.reference { theme::accent() } else { theme::p().text };
                    ui.label(RichText::new(format!("{}. {}", k + 1, r.path.display())).color(col));
                });
            }
            if self.roots.is_empty() {
                ui.label(RichText::new(tr("Todavía no hay ninguna carpeta. Pulsa «Examinar» o «Añadir rápido» para elegir dónde buscar.")).color(theme::p().muted));
            }
        });
        if let Some(k) = remove {
            self.roots.remove(k);
        }
        let mut add: Vec<PathBuf> = vec![];
        let mut browse = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("📂 Examinar…")).on_hover_text(tr("Navega por tus carpetas y añade las que quieras, sin escribir rutas")).clicked() {
                browse = true;
            }
            ui.menu_button(tr("➕ Añadir rápido ⏷"), |ui| {
                let label = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().to_string());
                ui.label(RichText::new(tr("Abierta ahora en la ventana principal")).small().color(theme::p().muted));
                if ui.button(format!("📁 {}", label(cwd))).on_hover_text(cwd.to_string_lossy()).clicked() {
                    add.push(cwd.to_path_buf());
                    ui.close_menu();
                }
                let sel_dirs: Vec<PathBuf> = selection.iter().filter(|p| p.is_dir()).cloned().collect();
                if !sel_dirs.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(tr("Seleccionadas en la ventana principal")).small().color(theme::p().muted));
                    for p in &sel_dirs {
                        if ui.button(format!("📁 {}", label(p))).on_hover_text(p.to_string_lossy()).clicked() {
                            add.push(p.clone());
                            ui.close_menu();
                        }
                    }
                    if sel_dirs.len() > 1 && ui.button(trf!("Todas ({})", sel_dirs.len())).clicked() {
                        add.extend(sel_dirs.iter().cloned());
                        ui.close_menu();
                    }
                }
                ui.separator();
                ui.label(RichText::new(tr("Lugares")).small().color(theme::p().muted));
                for (name, p) in quick_places() {
                    if ui.button(name).on_hover_text(p.to_string_lossy()).clicked() {
                        add.push(p);
                        ui.close_menu();
                    }
                }
                let favs = favorite_folders();
                if !favs.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(tr("Favoritos")).small().color(theme::p().muted));
                    for p in favs {
                        if ui.button(format!("★ {}", label(&p))).on_hover_text(p.to_string_lossy()).clicked() {
                            add.push(p);
                            ui.close_menu();
                        }
                    }
                }
            });
            let r = ui.add(egui::TextEdit::singleline(&mut self.root_input).hint_text(tr("o pega una ruta aquí")).desired_width(220.0));
            if (ui.button(tr("Añadir")).clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))) && !self.root_input.trim().is_empty() {
                let p = PathBuf::from(self.root_input.trim());
                if p.is_dir() {
                    add.push(p);
                    self.root_input.clear();
                } else {
                    self.status = Some((trf!("«{}» no es una carpeta", p.display()), true));
                }
            }
            ui.label(RichText::new(tr("También puedes arrastrar carpetas aquí.")).small().color(theme::p().muted));
        });
        for p in add {
            self.add_root(p);
        }
        if browse {
            let start = self.roots.last().and_then(|r| r.path.parent().map(Path::to_path_buf)).or_else(dirs::home_dir).unwrap_or_else(|| cwd.to_path_buf());
            self.browser = Some(Browser::new(start, 0));
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(250.0);
                ui.label(RichText::new(tr("CRITERIO")).small().strong().color(theme::p().sub));
                for (k, l) in MODES.iter().enumerate() {
                    ui.radio_value(&mut self.mode, k as u8, tr(l));
                }
                if self.mode == 1 || self.mode == 2 {
                    ui.checkbox(&mut self.name_ci, tr("Sin distinguir mayúsculas"));
                    ui.checkbox(&mut self.name_no_ext, tr("Ignorar la extensión"));
                }
                if self.mode == 0 {
                    ui.label(RichText::new(tr("Compara tamaño, luego el principio y el final, y solo entonces el archivo entero (BLAKE3).")).small().color(theme::p().muted));
                } else {
                    ui.label(RichText::new(tr("Más rápido, pero no garantiza que el contenido sea igual.")).small().color(theme::p().danger));
                }
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.set_width(360.0);
                ui.label(RichText::new(tr("FILTROS")).small().strong().color(theme::p().sub));
                size_field(ui, tr("Tamaño mínimo"), &mut self.min_text, "1 KB");
                size_field(ui, tr("Tamaño máximo"), &mut self.max_text, tr("sin límite"));
                let hint_inc = if self.pat_regex { "\\.(jpe?g|png)$" } else { "*.jpg; *.png" };
                ui.horizontal(|ui| {
                    ui.add_sized([100.0, 18.0], egui::Label::new(tr("Incluir")));
                    ui.add(egui::TextEdit::singleline(&mut self.include).hint_text(hint_inc).desired_width(220.0));
                });
                ui.horizontal(|ui| {
                    ui.add_sized([100.0, 18.0], egui::Label::new(tr("Excluir")));
                    ui.add(egui::TextEdit::singleline(&mut self.exclude).hint_text(if self.pat_regex { "\\.tmp$" } else { "*.tmp; thumbs.db" }).desired_width(220.0));
                });
                ui.checkbox(&mut self.pat_regex, tr("Los filtros son expresiones regulares"));
                ui.horizontal(|ui| {
                    ui.add_sized([100.0, 18.0], egui::Label::new(tr("Saltar carpetas")));
                    ui.add(egui::TextEdit::singleline(&mut self.exclude_dirs).desired_width(220.0)).on_hover_text(tr("Nombres de carpetas que no se recorren, con comodines, separados por «;»"));
                });
                ui.horizontal(|ui| {
                    ui.add_sized([100.0, 18.0], egui::Label::new(tr("Antigüedad mínima")));
                    ui.add(egui::DragValue::new(&mut self.min_age_days).range(0..=36500).suffix(tr(" días"))).on_hover_text(tr("Ignora lo modificado hace menos de estos días (0 = todo). Útil para no tocar descargas recientes."));
                });
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(tr("OPCIONES")).small().strong().color(theme::p().sub));
                ui.checkbox(&mut self.skip_hidden, tr("Saltar archivos y carpetas ocultos"));
                ui.checkbox(&mut self.skip_empty, tr("Ignorar archivos vacíos"));
                ui.checkbox(&mut self.hardlinks_as_one, tr("Contar los enlaces duros como un solo archivo"))
                    .on_hover_text(tr("Dos nombres que ya apuntan a los mismos datos no ocupan el doble; así no salen como duplicados."));
                ui.checkbox(&mut self.cross_only, tr("Solo duplicados entre carpetas distintas"))
                    .on_hover_text(tr("Ignora los repetidos que están todos dentro de una misma carpeta de la lista."));
                ui.add_enabled(self.mode == 0, egui::Checkbox::new(&mut self.verify, tr("Verificar antes de actuar")))
                    .on_hover_text(tr("Antes de borrar, mover o enlazar, vuelve a leer cada copia y la que se conserva y comprueba que siguen siendo idénticas. Recomendado."));
                ui.checkbox(&mut self.keep_one, tr("Conservar siempre una copia"))
                    .on_hover_text(tr("Si marcas todas las copias de un grupo, ese grupo se omite para no perder el archivo. Recomendado."));
                if ui.button(tr("↺ Valores recomendados")).on_hover_text(tr("Restablece criterio, filtros y opciones")).clicked() {
                    let (roots, browser) = (std::mem::take(&mut self.roots), self.browser.take());
                    let fresh = Dupes::new(vec![], self.base.clone());
                    let (groups, marks, status) = (std::mem::take(&mut self.groups), std::mem::take(&mut self.marks), self.status.take());
                    *self = fresh;
                    self.roots = roots;
                    self.browser = browser;
                    self.groups = groups;
                    self.marks = marks;
                    self.status = status;
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr("TIPOS")).small().strong().color(theme::p().sub));
            for (k, (name, _)) in KINDS.iter().enumerate() {
                ui.toggle_value(&mut self.kinds[k], tr(name));
            }
            if self.kinds.iter().any(|k| *k) {
                if ui.small_button(tr("todos los tipos")).clicked() {
                    self.kinds = [false; 5];
                }
            } else {
                ui.label(RichText::new(tr("(sin marcar: se revisan todos)")).small().color(theme::p().muted));
            }
        });
    }

    fn browser_ui(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.browser.take() else { return };
        let mut open = true;
        let (mut nav, mut add, mut pick): (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) = (None, None, None);
        let mut close = false;
        egui::Window::new(tr("Elegir carpeta")).id(egui::Id::new("dup_browser")).open(&mut open).default_width(480.0).default_height(440.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(b.path.parent().is_some(), egui::Button::new("⬆")).on_hover_text(tr("Carpeta superior")).clicked() {
                    nav = b.path.parent().map(Path::to_path_buf);
                }
                if ui.button("🏠").on_hover_text(tr("Carpeta personal")).clicked() {
                    nav = dirs::home_dir();
                }
                let r = ui.add(egui::TextEdit::singleline(&mut b.input).desired_width(ui.available_width() - 4.0));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    nav = Some(PathBuf::from(b.input.trim()));
                }
            });
            ui.horizontal_wrapped(|ui| {
                for (name, p) in quick_places().into_iter().take(8) {
                    if ui.small_button(name).on_hover_text(p.to_string_lossy()).clicked() {
                        nav = Some(p);
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(270.0).auto_shrink([false, false]).show(ui, |ui| {
                if b.entries.is_empty() {
                    ui.label(RichText::new(tr("No hay subcarpetas.")).color(theme::p().muted));
                }
                for (name, p) in &b.entries {
                    ui.horizontal(|ui| {
                        if b.purpose == 0 {
                            if self.roots.iter().any(|r| &r.path == p) {
                                ui.label(RichText::new("✔").color(theme::p().ok)).on_hover_text(tr("Ya está en la búsqueda"));
                            } else if ui.small_button("+").on_hover_text(tr("Añadir esta carpeta a la búsqueda")).clicked() {
                                add = Some(p.clone());
                            }
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
                if b.purpose == 0 {
                    if ui.button(trf!("➕ Añadir «{}»", name)).on_hover_text(b.path.to_string_lossy()).clicked() {
                        add = Some(b.path.clone());
                    }
                } else if ui.button(trf!("✔ Usar «{}»", name)).on_hover_text(b.path.to_string_lossy()).clicked() {
                    pick = Some(b.path.clone());
                }
                if ui.button(tr("Cerrar")).clicked() {
                    close = true;
                }
            });
        });
        if let Some(p) = nav.filter(|p| p.is_dir()) {
            b.go(p);
        }
        if let Some(p) = add {
            self.add_root(p);
        }
        if let Some(p) = &pick {
            self.move_dest = p.to_string_lossy().to_string();
        }
        if open && !close && pick.is_none() {
            self.browser = Some(b);
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.groups.is_empty() {
            if self.job.is_none() && self.status.is_some() {
                ui.label(trf!("No se encontraron duplicados entre {} archivos revisados.", self.scanned));
            }
            return;
        }
        let wasted: u64 = self.groups.iter().map(Group::wasted).sum();
        let copies: usize = self.groups.iter().map(|g| g.files.len() - 1).sum();
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("{} grupos · {} copias de más · {} recuperables · {} archivos revisados", self.groups.len(), copies, human_size(wasted), self.scanned)).strong());
        });
        ui.horizontal_wrapped(|ui| {
            let before = self.sort;
            egui::ComboBox::from_id_salt("orden_grupos").width(190.0).selected_text(tr(SORTS[self.sort as usize])).show_ui(ui, |ui| {
                for (k, l) in SORTS.iter().enumerate() {
                    ui.selectable_value(&mut self.sort, k as u8, tr(l));
                }
            });
            if before != self.sort {
                self.sort_groups();
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar resultados")).desired_width(170.0));
            egui::ComboBox::from_id_salt("conservar").width(260.0).selected_text(tr(KEEP[self.keep])).show_ui(ui, |ui| {
                for (k, l) in KEEP.iter().enumerate() {
                    ui.selectable_value(&mut self.keep, k, tr(l));
                }
            });
            if ui.button(tr("Marcar automáticamente")).on_hover_text(tr("En cada grupo marca todas las copias menos una, según el criterio elegido. Nunca marca archivos de carpetas de referencia.")).clicked() {
                self.auto_mark();
            }
            if ui.button(tr("Desmarcar todo")).clicked() {
                self.marks.clear();
            }
        });

        // Flattened rows: a header per group, then its files.
        let q = self.filter.to_lowercase();
        let mut rows: Vec<(usize, Option<usize>)> = vec![];
        for (gi, g) in self.groups.iter().enumerate() {
            if !q.is_empty() && !g.files.iter().any(|f| f.path.to_string_lossy().to_lowercase().contains(&q)) {
                continue;
            }
            rows.push((gi, None));
            if !self.collapsed.contains(&gi) {
                rows.extend((0..g.files.len()).map(|fi| (gi, Some(fi))));
            }
        }
        let row_h = 22.0;
        let mut toggle_mark: Option<PathBuf> = None;
        let mut toggle_group = None;
        let mut nav = None;
        egui::ScrollArea::vertical().id_salt("dup_res").max_height(260.0).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            for &(gi, fi) in &rows[range] {
                let g = &self.groups[gi];
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                match fi {
                    None => {
                        ui.painter().rect_filled(rect, 3.0, theme::p().hover);
                        let arrow = if self.collapsed.contains(&gi) { "⏵" } else { "⏷" };
                        let size = g.files.first().map(|f| f.size).unwrap_or(0);
                        let name = g.files.first().and_then(|f| f.path.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        child.label(RichText::new(trf!("{} Grupo {} · {} copias · {} cada una · {} de más · {}", arrow, gi + 1, g.files.len(), human_size(size), human_size(g.wasted()), name)).strong().small());
                        if resp.clicked() {
                            toggle_group = Some(gi);
                        }
                    }
                    Some(fi) => {
                        let f = &g.files[fi];
                        let reference = self.is_reference(f);
                        let mut m = self.marks.contains(&f.path);
                        child.add_space(14.0);
                        if child.add_enabled(!reference, egui::Checkbox::new(&mut m, "")).changed() {
                            toggle_mark = Some(f.path.clone());
                        }
                        let col = if reference {
                            theme::accent()
                        } else if m {
                            theme::p().danger
                        } else {
                            theme::p().ok
                        };
                        let tag = if reference { tr("referencia") } else if m { tr("se quitará") } else { tr("se conserva") };
                        child.add_sized([78.0, row_h], egui::Label::new(RichText::new(tag).small().color(col)));
                        child.add_sized([120.0, row_h], egui::Label::new(RichText::new(fsops::format_time(f.modified)).small().color(theme::p().sub)));
                        if self.mode != 0 {
                            child.add_sized([70.0, row_h], egui::Label::new(RichText::new(human_size(f.size)).small().color(theme::p().sub)));
                        }
                        let r = child.add(egui::Label::new(RichText::new(f.path.to_string_lossy()).color(theme::p().text)).truncate().sense(egui::Sense::click()));
                        if r.on_hover_text(tr("Doble clic: ir a su carpeta")).double_clicked() {
                            nav = f.path.parent().map(Path::to_path_buf);
                        }
                    }
                }
            }
        });
        if let Some(p) = toggle_mark {
            if !self.marks.remove(&p) {
                self.marks.insert(p);
            }
        }
        if let Some(g) = toggle_group {
            if !self.collapsed.remove(&g) {
                self.collapsed.insert(g);
            }
        }
        if let Some(n) = nav {
            events.push(ToolEvent::Navigate(n));
        }

        // Actions on the marked copies.
        let marked = self.marked_files();
        let bytes: u64 = marked.iter().map(|f| f.size).sum();
        let busy = self.job.is_some();
        let skipped = if self.keep_one { self.groups.iter().filter(|g| self.fully_marked(g)).count() } else { 0 };
        ui.separator();
        if skipped > 0 {
            ui.label(RichText::new(trf!("⚠ {} grupo(s) tienen todas las copias marcadas: se omiten para no perder el archivo.", skipped)).small().color(theme::p().danger));
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("{} marcado(s) · {}", marked.len(), human_size(bytes))).strong());
            let any = !marked.is_empty() && !busy;
            if ui.add_enabled(any, egui::Button::new(RichText::new(tr("🗑 A la papelera")).color(Color32::WHITE)).fill(theme::p().danger)).clicked() {
                self.run_action(0);
            }
            ui.checkbox(&mut self.confirm_delete, tr("sin papelera"));
            if self.confirm_delete && ui.add_enabled(any, egui::Button::new(tr("⚠ Eliminar definitivamente"))).clicked() {
                self.run_action(1);
            }
            if ui
                .add_enabled(any && self.mode == 0, egui::Button::new(tr("🔗 Cambiar por enlaces duros")))
                .on_hover_text(tr("Cada copia marcada pasa a ser otro nombre de la que se conserva: siguen apareciendo en su sitio pero ocupan una sola vez. Solo con contenido idéntico y en el mismo disco."))
                .clicked()
            {
                self.run_action(3);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.move_dest).hint_text(tr("carpeta destino")).desired_width(220.0));
            if ui.button(tr("📂 Elegir…")).clicked() {
                let start = PathBuf::from(self.move_dest.trim());
                let start = if start.is_dir() { start } else { dirs::home_dir().unwrap_or_else(|| self.base.clone()) };
                self.browser = Some(Browser::new(start, 1));
            }
            let ok = !marked.is_empty() && !busy && !self.move_dest.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new(tr("📦 Mover marcados a esa carpeta"))).clicked() {
                self.run_action(2);
            }
            if ui.add_enabled(!busy, egui::Button::new(tr("💾 Exportar informe CSV"))).clicked() {
                self.export_csv(events);
            }
            if ui.add_enabled(!marked.is_empty(), egui::Button::new(tr("📋 Copiar rutas marcadas"))).clicked() {
                events.push(ToolEvent::Clipboard(marked.iter().map(|f| f.path.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n")));
            }
        });
    }
}

const SORTS: [&str; 4] = ["Más espacio recuperable primero", "Más copias primero", "Archivos más grandes primero", "Por nombre"];

const KINDS: [(&str, &str); 5] = [
    ("Imágenes", "jpg jpeg png gif bmp webp tif tiff heic heif avif svg raw dng cr2 nef arw"),
    ("Vídeo", "mp4 mkv avi mov wmv flv webm m4v mpg mpeg 3gp"),
    ("Audio", "mp3 flac wav ogg opus m4a aac wma aiff ape"),
    ("Documentos", "pdf doc docx odt rtf txt md xls xlsx ods csv ppt pptx odp epub"),
    ("Comprimidos", "zip rar 7z tar gz bz2 xz zst iso"),
];

/// "500 KB", "1,5 GB", "2m", "1024" (bytes)… empty means no value.
pub fn parse_size(text: &str) -> Result<Option<u64>, String> {
    let t = text.trim().to_lowercase().replace(',', ".");
    if t.is_empty() {
        return Ok(None);
    }
    let cut = t.find(|c: char| !(c.is_ascii_digit() || c == '.' || c.is_whitespace())).unwrap_or(t.len());
    let (num, unit) = t.split_at(cut);
    let n: f64 = num.trim().parse().map_err(|_| tr("escribe un número y una unidad, p. ej. 500 KB o 1,5 GB").to_string())?;
    let mult: f64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" | "o" => 1.0,
        "k" | "kb" | "kib" | "ko" => 1024.0,
        "m" | "mb" | "mib" | "mo" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" | "go" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" | "to" => 1024.0_f64.powi(4),
        other => return Err(trf!("unidad «{}» no reconocida (usa B, KB, MB, GB o TB)", other)),
    };
    Ok(Some((n * mult) as u64))
}

pub(crate) fn size_field(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([100.0, 18.0], egui::Label::new(label));
        ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(90.0)).on_hover_text(tr("Escribe el número y la unidad: 500 KB, 1,5 MB, 2 GB…"));
        match parse_size(text) {
            Ok(Some(n)) => {
                ui.label(RichText::new(format!("= {}", human_size(n))).small().color(theme::p().sub));
            }
            Ok(None) => {}
            Err(e) => {
                ui.label(RichText::new(e).small().color(theme::p().danger));
            }
        }
    });
}

/// Folders worth offering: the usual places and any mounted drives.
pub(crate) fn quick_places() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = vec![];
    let named = [
        ("🏠", tr("Carpeta personal"), dirs::home_dir()),
        ("🖥", tr("Escritorio"), dirs::desktop_dir()),
        ("📄", tr("Documentos"), dirs::document_dir()),
        ("⬇", tr("Descargas"), dirs::download_dir()),
        ("🖼", tr("Imágenes"), dirs::picture_dir()),
        ("🎵", tr("Música"), dirs::audio_dir()),
        ("🎞", tr("Vídeos"), dirs::video_dir()),
    ];
    for (icon, name, p) in named {
        if let Some(p) = p.filter(|p| p.is_dir()) {
            v.push((format!("{icon} {name}"), p));
        }
    }
    #[cfg(unix)]
    {
        v.push((format!("🖴 {}", tr("Sistema de archivos")), PathBuf::from("/")));
        for m in ["/media", "/mnt", "/run/media", "/Volumes"] {
            if let Ok(rd) = fs::read_dir(m) {
                for d in rd.flatten().take(8) {
                    let p = d.path();
                    // /run/media/<user>/<disk>
                    if m == "/run/media" {
                        if let Ok(inner) = fs::read_dir(&p) {
                            for d2 in inner.flatten().take(8) {
                                v.push((format!("💽 {}", d2.file_name().to_string_lossy()), d2.path()));
                            }
                        }
                    } else if p.is_dir() {
                        v.push((format!("💽 {}", d.file_name().to_string_lossy()), p));
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    for l in b'C'..=b'Z' {
        let p = PathBuf::from(format!("{}:\\", l as char));
        if p.exists() {
            v.push((format!("🖴 {}", trf!("Disco ({}:)", l as char)), p));
        }
    }
    v
}

pub(crate) fn favorite_folders() -> Vec<PathBuf> {
    crate::config_file()
        .and_then(|f| fs::read_to_string(f).ok())
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

/// The built-in folder picker's state.
pub(crate) struct Browser {
    pub(crate) path: PathBuf,
    pub(crate) input: String,
    pub(crate) entries: Vec<(String, PathBuf)>,
    /// 0: add to the search; 1: choose the move destination.
    pub(crate) purpose: u8,
}

impl Browser {
    pub(crate) fn new(path: PathBuf, purpose: u8) -> Browser {
        let mut b = Browser { path: PathBuf::new(), input: String::new(), entries: vec![], purpose };
        b.go(path);
        b
    }

    pub(crate) fn go(&mut self, p: PathBuf) {
        let p = crate::fsops::canonical(&p);
        let mut entries: Vec<(String, PathBuf)> = fs::read_dir(&p)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
                    .filter(|(n, _)| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|(n, _)| n.to_lowercase());
        self.input = p.to_string_lossy().to_string();
        self.path = p;
        self.entries = entries;
    }
}

// ---------------- scanning ----------------

fn scan(s: &Settings, p: &Progress, cancel: &AtomicBool) -> (Vec<Group>, Vec<String>, usize) {
    let set_stage = |t: &str| {
        if let Ok(mut st) = p.stage.lock() {
            *st = t.to_string();
        }
    };
    set_stage(tr("Recorriendo carpetas"));
    let mut errs = vec![];
    let mut files: Vec<FileInfo> = vec![];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    #[cfg(unix)]
    let mut inodes: HashSet<(u64, u64)> = HashSet::new();
    for (ri, root) in s.roots.iter().enumerate() {
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let rd = match fs::read_dir(&d) {
                Ok(rd) => rd,
                Err(e) => {
                    errs.push(format!("{}: {e}", d.display()));
                    continue;
                }
            };
            for item in rd.flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if s.skip_hidden && name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = item.path().symlink_metadata() else { continue };
                let path = item.path();
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    if s.exclude_dirs.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if !meta.is_file() {
                    continue;
                }
                let size = meta.len();
                if (s.skip_empty && size == 0) || size < s.min_bytes || size > s.max_bytes {
                    continue;
                }
                if let Some(exts) = &s.exts {
                    let e = Path::new(&name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                    if !exts.contains(&e) {
                        continue;
                    }
                }
                if s.min_age_secs > 0 {
                    let recent = meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).map(|age| age.as_secs() < s.min_age_secs).unwrap_or(false);
                    if recent {
                        continue;
                    }
                }
                if let Some(re) = &s.include {
                    if !re.is_match(&name) {
                        continue;
                    }
                }
                if let Some(re) = &s.exclude {
                    if re.is_match(&name) {
                        continue;
                    }
                }
                // Overlapping folders in the list would otherwise report a file as its own copy.
                if !seen.insert(path.clone()) {
                    continue;
                }
                #[cfg(unix)]
                if s.hardlinks_as_one {
                    use std::os::unix::fs::MetadataExt;
                    if !inodes.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                }
                files.push(FileInfo { path, size, modified: meta.modified().ok(), root: ri });
                p.files.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let scanned = files.len();

    let name_key = |f: &FileInfo| {
        let mut n = f.path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if s.name_no_ext {
            if let Some(i) = n.rfind('.').filter(|&i| i > 0) {
                n.truncate(i);
            }
        }
        if s.name_ci {
            n = n.to_lowercase();
        }
        n
    };
    let mut groups: Vec<Vec<FileInfo>> = match s.mode {
        1 => bucket(files, |f| name_key(f)),
        2 => bucket(files, |f| format!("{}\u{0}{}", name_key(f), f.size)),
        3 => bucket(files, |f| f.size),
        _ => {
            let by_size = bucket(files, |f| f.size);
            // Ends of each file first: cheap, and enough to tell most different files apart.
            set_stage(tr("Comparando principio y final"));
            let cands: Vec<FileInfo> = by_size.into_iter().flatten().collect();
            let partial = hash_all(&cands, true, p, cancel, &mut errs);
            let mut stage2: Vec<Vec<FileInfo>> = vec![];
            for g in bucket(cands.into_iter().zip(partial).filter_map(|(f, h)| h.map(|h| (f, h))).collect::<Vec<_>>(), |(f, h)| (f.size, *h)) {
                stage2.push(g.into_iter().map(|(f, _)| f).collect());
            }
            set_stage(tr("Comparando el contenido completo"));
            let (small, big): (Vec<Vec<FileInfo>>, Vec<Vec<FileInfo>>) = stage2.into_iter().partition(|g| g[0].size <= 2 * PARTIAL as u64);
            let big_files: Vec<FileInfo> = big.into_iter().flatten().collect();
            let full = hash_all(&big_files, false, p, cancel, &mut errs);
            let mut out = small;
            for g in bucket(big_files.into_iter().zip(full).filter_map(|(f, h)| h.map(|h| (f, h))).collect::<Vec<_>>(), |(f, h)| (f.size, *h)) {
                out.push(g.into_iter().map(|(f, _)| f).collect());
            }
            out
        }
    };
    if s.cross_only {
        groups.retain(|g| g.iter().map(|f| f.root).collect::<HashSet<_>>().len() > 1);
    }
    let mut groups: Vec<Group> = groups
        .into_iter()
        .map(|mut files| {
            files.sort_by(|a, b| a.root.cmp(&b.root).then(a.path.cmp(&b.path)));
            Group { files }
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    (groups, errs, scanned)
}

/// Groups by a key and keeps only groups with two or more members.
fn bucket<T, K: std::hash::Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<Vec<T>> {
    let mut map: HashMap<K, Vec<T>> = HashMap::new();
    for it in items {
        map.entry(key(&it)).or_default().push(it);
    }
    map.into_values().filter(|v| v.len() > 1).collect()
}

const PARTIAL: usize = 16 * 1024;

/// Hashes every file on several threads; `None` for files that couldn't be read.
fn hash_all(files: &[FileInfo], partial: bool, p: &Progress, cancel: &AtomicBool, errs: &mut Vec<String>) -> Vec<Option<[u8; 32]>> {
    p.total.store(files.len(), Ordering::Relaxed);
    p.done.store(0, Ordering::Relaxed);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<[u8; 32]>>> = Mutex::new(vec![None; files.len()]);
    let errors: Mutex<Vec<String>> = Mutex::new(vec![]);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = if partial { hash_ends(&files[i].path, files[i].size, p) } else { hash_full(&files[i].path, p, cancel) };
                match r {
                    Ok(h) => results.lock().unwrap()[i] = Some(h),
                    Err(e) => errors.lock().unwrap().push(format!("{}: {e}", files[i].path.display())),
                }
                p.done.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
    errs.extend(errors.into_inner().unwrap_or_default());
    results.into_inner().unwrap_or_default()
}

fn hash_ends(path: &Path, size: u64, p: &Progress) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; PARTIAL];
    let n = read_full(&mut f, &mut buf)?;
    h.update(&buf[..n]);
    p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    if size > 2 * PARTIAL as u64 {
        f.seek(SeekFrom::End(-(PARTIAL as i64)))?;
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    } else if size > PARTIAL as u64 {
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(*h.finalize().as_bytes())
}

fn hash_full(path: &Path, p: &Progress, cancel: &AtomicBool) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(tr("cancelado")));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(*h.finalize().as_bytes())
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Swaps `dup` for a hard link to `keep`, through a temporary name so nothing is lost on failure.
fn replace_with_hardlink(keep: &Path, dup: &Path) -> io::Result<()> {
    let tmp = dup.with_file_name(format!(".dup-link-{}", std::process::id()));
    fs::hard_link(keep, &tmp)?;
    if let Err(e) = fs::rename(&tmp, dup) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_with_units() {
        assert_eq!(parse_size("").unwrap(), None);
        assert_eq!(parse_size("1024").unwrap(), Some(1024));
        assert_eq!(parse_size("1 KB").unwrap(), Some(1024));
        assert_eq!(parse_size("1,5 mb").unwrap(), Some(1_572_864));
        assert_eq!(parse_size("2G").unwrap(), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("10 bytes").unwrap(), Some(10));
        assert!(parse_size("5 parsecs").is_err());
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn finds_duplicates_across_folders() {
        let dir = std::env::temp_dir().join(format!("dup-test-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(b.join("node_modules")).unwrap();
        let big: Vec<u8> = (0..100_000u32).map(|x| (x % 251) as u8).collect();
        let mut big2 = big.clone();
        big2[50_000] ^= 1; // same size and same ends, different middle
        fs::write(a.join("foto.jpg"), &big).unwrap();
        fs::write(a.join("sub/copia.jpg"), &big).unwrap();
        fs::write(b.join("otra.jpg"), &big).unwrap();
        fs::write(b.join("casi.jpg"), &big2).unwrap();
        fs::write(b.join("node_modules/x.jpg"), &big).unwrap();
        fs::write(a.join("nota.txt"), "hola").unwrap();
        fs::write(b.join("nota.txt"), "adios").unwrap();
        let mut d = Dupes::new(vec![a.clone(), b.clone()], dir.clone());
        d.min_text = "0".into();
        let p = Progress::default();
        let never = AtomicBool::new(false);
        let (groups, _, scanned) = scan(&d.settings().unwrap(), &p, &never);
        assert_eq!(scanned, 6);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 3);
        // By name: the two notes.
        d.mode = 1;
        let (groups, _, _) = scan(&d.settings().unwrap(), &p, &never);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].files[0].path.ends_with("nota.txt"));
        // Reference folder a: only the copy in b gets marked.
        d.mode = 0;
        d.roots[0].reference = true;
        d.groups = scan(&d.settings().unwrap(), &p, &never).0;
        d.auto_mark();
        let marked = d.marked_files();
        assert_eq!(marked.len(), 1);
        assert!(marked[0].path.starts_with(&b));
        // Only across folders: copy inside a alone isn't enough.
        d.roots[0].reference = false;
        d.roots.retain(|r| r.path == a);
        d.cross_only = true;
        assert!(scan(&d.settings().unwrap(), &p, &never).0.is_empty());
        // Types and the keep-one safety.
        d.roots = vec![Root { path: a.clone(), reference: false }, Root { path: b.clone(), reference: false }];
        d.mode = 0;
        d.kinds[3] = true; // documents: only the notes, which differ in content
        assert!(scan(&d.settings().unwrap(), &p, &never).0.is_empty());
        d.kinds = [false; 5];
        d.groups = scan(&d.settings().unwrap(), &p, &never).0;
        d.marks = d.groups.iter().flat_map(|g| g.files.iter().map(|f| f.path.clone())).collect();
        assert!(d.marked_files().is_empty(), "a group with every copy marked is skipped");
        d.keep_one = false;
        assert_eq!(d.marked_files().len(), 3);
        d.keep_one = true;
        d.roots.retain(|r| r.path == a);
        // Hard link replacement keeps both names and the content.
        replace_with_hardlink(&a.join("foto.jpg"), &a.join("sub/copia.jpg")).unwrap();
        assert_eq!(fs::read(a.join("sub/copia.jpg")).unwrap(), big);
        #[cfg(unix)]
        {
            d.cross_only = false;
            assert!(scan(&d.settings().unwrap(), &p, &never).0.is_empty(), "hard links count as one file");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
