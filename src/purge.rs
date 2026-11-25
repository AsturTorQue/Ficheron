//! Delete by pattern: several folders; wildcards or regular expressions on the name or the path;
//! files and/or folders; protected patterns; filters by size and age; ready-made templates;
//! a reviewable result list; deletion to the trash, for good, to another folder or securely.

use crate::dupes::{parse_size, size_field, Browser};
use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::sync::quick_menu;
use crate::theme;
use crate::tools::{shred_file, ToolEvent};
use eframe::egui::{self, Color32, RichText};
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

const METHODS: [&str; 4] = ["A la papelera", "Definitivamente", "Mover a otra carpeta", "Borrado seguro (sobrescribir)"];
const SORTS: [&str; 4] = ["Por ruta", "Más grandes primero", "Más antiguos primero", "Por nombre"];

type Template = (&'static str, &'static str, fn(&mut Purge));

fn set(p: &mut Purge, patterns: &str, exclude: &str, files: bool, folders: bool) {
    p.patterns = patterns.into();
    p.exclude = exclude.into();
    p.files = files;
    p.folders = folders;
    p.regex = false;
    p.on_path = false;
}

const TEMPLATES: [Template; 11] = [
    ("🕓 Archivos temporales", "*.tmp, *.temp, ~*, *~, bloqueos y archivos de intercambio de editores.", |p| set(p, "*.tmp; *.temp; ~*; *~; .~lock.*#; *.swp; *.swo", "", true, false)),
    ("💾 Copias de seguridad automáticas", "*.bak, *.old, *.orig, *.backup", |p| set(p, "*.bak; *.old; *.orig; *.backup; *.bkp", "", true, false)),
    ("🗑 Basura del sistema", "Thumbs.db, desktop.ini, .DS_Store, ._*", |p| set(p, "Thumbs.db; desktop.ini; .DS_Store; ._*; ehthumbs.db", "", true, false)),
    ("📜 Registros (logs)", "*.log y *.log.1, *.log.2…", |p| set(p, "*.log; *.log.*", "", true, false)),
    ("💥 Informes de fallos", "core, *.dmp, *.crash, hs_err_pid*", |p| set(p, "core; core.*; *.dmp; *.crash; hs_err_pid*", "", true, false)),
    ("⬇ Descargas incompletas", "*.part, *.crdownload, *.download, *.partial", |p| set(p, "*.part; *.crdownload; *.download; *.partial; *.!ut", "", true, false)),
    ("📄 Temporales de Office", "~$*, *.wbk, .~lock.*", |p| set(p, "~$*; *.wbk; .~lock.*#", "", true, false)),
    ("🔧 Restos de compilación", "*.o, *.obj, *.pyc, *.class, *.pdb y carpetas __pycache__", |p| set(p, "*.o; *.obj; *.pyc; *.pyo; *.class; *.pdb; __pycache__", "", true, true)),
    ("📦 Carpetas de dependencias", "node_modules, target, .gradle, venv, __pycache__ (carpetas enteras)", |p| set(p, "node_modules; target; .gradle; __pycache__; .pytest_cache; .mypy_cache", "", false, true)),
    ("📑 Archivos «copia»", "informe (copia).doc, informe - copia.doc, informe (1).doc", |p| {
        set(p, "^.*(\\s*\\(\\d+\\)|\\s*[-_]\\s*(copia|copy)(\\s*\\(?\\d+\\)?)?|\\s+\\((copia|copy)[^)]*\\))\\.[^.]+$", "", true, false);
        p.regex = true;
    }),
    ("📏 Archivos vacíos", "Archivos de 0 bytes (con cualquier nombre)", |p| {
        set(p, "*", "", true, false);
        p.max_text = "0".into();
    }),
];

struct Item {
    path: PathBuf,
    root: usize,
    is_dir: bool,
    size: u64,
    files: usize,
    modified: Option<SystemTime>,
    on: bool,
}

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    current: Mutex<String>,
    entries: AtomicUsize,
    done: AtomicUsize,
    total: AtomicUsize,
}

enum Msg {
    Found(Vec<Item>, Vec<String>, usize, bool),
    Done(Vec<PathBuf>, Vec<String>, String),
}

struct Job {
    rx: Receiver<Msg>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

#[derive(Clone)]
struct Cfg {
    pat: Option<Regex>,
    excl: Option<Regex>,
    on_path: bool,
    files: bool,
    folders: bool,
    recursive: bool,
    depth: u32,
    skip_dirs: Option<Regex>,
    skip_hidden: bool,
    readonly: bool,
    min: u64,
    max: u64,
    older_secs: u64,
    newer_secs: u64,
}

pub struct Purge {
    roots: Vec<PathBuf>,
    root_input: String,
    patterns: String,
    exclude: String,
    regex: bool,
    on_path: bool,
    case: bool,
    files: bool,
    folders: bool,
    recursive: bool,
    depth: u32,
    skip_dirs: String,
    skip_hidden: bool,
    readonly: bool,
    min_text: String,
    max_text: String,
    older_days: u32,
    newer_days: u32,
    method: u8,
    dest: String,
    passes: u32,
    clean_empty: bool,
    confirm: bool,
    job: Option<Job>,
    found: Vec<Item>,
    scanned: usize,
    truncated: bool,
    sort: u8,
    filter: String,
    view_cache: crate::tools::ViewCache,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    browser: Option<Browser>,
    base: PathBuf,
}

// ---------------- matching ----------------

/// `*` and `?` as a regular expression; in a path `*` stops at «/» and `**` doesn't.
fn glob(p: &str, on_path: bool) -> String {
    let cs: Vec<char> = p.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '*' if on_path && cs.get(i + 1) == Some(&'*') => {
                out.push_str(".*");
                i += 1;
            }
            '*' => out.push_str(if on_path { "[^/]*" } else { ".*" }),
            '?' => out.push_str(if on_path { "[^/]" } else { "." }),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

fn compile(text: &str, regex: bool, on_path: bool, case: bool) -> Result<Option<Regex>, String> {
    let parts: Vec<String> = if regex {
        text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| format!("(?:{l})")).collect()
    } else {
        text.split([';', '\n'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let body = glob(p, on_path);
                let body = if on_path && !p.contains('/') { format!("(?:.*/)?{body}") } else { body };
                format!("^(?:{body})$")
            })
            .collect()
    };
    if parts.is_empty() {
        return Ok(None);
    }
    RegexBuilder::new(&parts.join("|")).case_insensitive(!case).build().map(Some).map_err(|e| trf!("Expresión no válida: {}", e.to_string().lines().last().unwrap_or("").trim()))
}

fn scan(roots: &[PathBuf], c: &Cfg, prog: &Progress, cancel: &AtomicBool) -> (Vec<Item>, Vec<String>, usize, bool) {
    const LIMIT: usize = 100_000;
    let mut items = vec![];
    let mut errs = vec![];
    let mut scanned = 0usize;
    let mut truncated = false;
    let now = SystemTime::now();
    let age = |m: Option<SystemTime>| m.and_then(|m| now.duration_since(m).ok()).map(|d| d.as_secs());
    'roots: for (ri, root) in roots.iter().enumerate() {
        let mut stack: Vec<(PathBuf, u32, String)> = vec![(root.clone(), 0, String::new())];
        while let Some((dir, depth, rel)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                break 'roots;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    errs.push(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                scanned += 1;
                if scanned % 500 == 0 {
                    prog.entries.store(scanned, Ordering::Relaxed);
                    if let Ok(mut cur) = prog.current.lock() {
                        *cur = dir.to_string_lossy().to_string();
                    }
                }
                if c.skip_hidden && name.starts_with('.') {
                    continue;
                }
                let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                let subject = if c.on_path { &r } else { &name };
                let protected = c.excl.as_ref().map(|re| re.is_match(subject)).unwrap_or(false);
                let is_dir = meta.is_dir() && !meta.file_type().is_symlink();
                let modified = meta.modified().ok();
                if is_dir {
                    if protected || c.skip_dirs.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                        continue;
                    }
                    if c.folders && c.pat.as_ref().map(|re| re.is_match(subject)).unwrap_or(false) {
                        let mut budget = 300_000;
                        let (size, count) = fsops::dir_size(&path, &mut budget);
                        items.push(Item { path, root: ri, is_dir: true, size, files: count, modified, on: true });
                        continue; // the whole folder goes
                    }
                    if c.recursive && (c.depth == 0 || depth + 1 < c.depth) {
                        stack.push((path, depth + 1, r));
                    }
                    continue;
                }
                if !c.files || protected {
                    continue;
                }
                if !c.readonly && meta.permissions().readonly() {
                    continue;
                }
                let size = meta.len();
                if size < c.min || size > c.max {
                    continue;
                }
                if c.older_secs > 0 && !age(modified).map(|a| a >= c.older_secs).unwrap_or(false) {
                    continue;
                }
                if c.newer_secs > 0 && !age(modified).map(|a| a <= c.newer_secs).unwrap_or(false) {
                    continue;
                }
                if let Some(re) = &c.pat {
                    if !re.is_match(subject) {
                        continue;
                    }
                }
                if items.len() >= LIMIT {
                    truncated = true;
                    break 'roots;
                }
                items.push(Item { path, root: ri, is_dir: false, size, files: 1, modified, on: true });
            }
        }
    }
    (items, errs, scanned, truncated)
}

fn broad(p: &Path) -> bool {
    p.parent().is_none() || dirs::home_dir().map(|h| h == p).unwrap_or(false)
}

fn shred_tree(p: &Path, passes: u32) -> Result<(), String> {
    if p.is_dir() {
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
                let ep = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(ep);
                } else {
                    shred_file(&ep, passes).map_err(|e| e.to_string())?;
                }
            }
        }
        fs::remove_dir_all(p).map_err(|e| e.to_string())
    } else {
        shred_file(p, passes).map_err(|e| e.to_string())
    }
}

/// Removes the now-empty parent folders of `p`, up to (not including) the search folder.
fn clean_parents(p: &Path, root: &Path) {
    let mut cur = p.parent();
    while let Some(d) = cur {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            break;
        }
        cur = d.parent();
    }
}

impl Purge {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Purge {
        let roots: Vec<PathBuf> = targets.into_iter().filter(|p| p.is_dir()).collect();
        Purge {
            roots,
            root_input: String::new(),
            patterns: "*.tmp; *.bak; *~".into(),
            exclude: String::new(),
            regex: false,
            on_path: false,
            case: false,
            files: true,
            folders: false,
            recursive: true,
            depth: 0,
            skip_dirs: ".git".into(),
            skip_hidden: false,
            readonly: true,
            min_text: String::new(),
            max_text: String::new(),
            older_days: 0,
            newer_days: 0,
            method: 0,
            dest: String::new(),
            passes: 1,
            clean_empty: false,
            confirm: false,
            job: None,
            found: vec![],
            scanned: 0,
            truncated: false,
            sort: 0,
            filter: String::new(),
            view_cache: Default::default(),
            status: None,
            problems: vec![],
            browser: None,
            base,
        }
    }

    /// Adds the folder and searches at once, for trying the tool (`--purge`).
    pub fn demo(&mut self, dir: &Path) {
        self.add_root(dir.to_path_buf());
        self.start_scan();
    }

    fn add_root(&mut self, p: PathBuf) {
        if p.is_dir() && !self.roots.contains(&p) {
            self.roots.push(p);
        }
    }

    fn config(&self) -> Result<Cfg, String> {
        let pat = compile(&self.patterns, self.regex, self.on_path, self.case)?;
        let excl = compile(&self.exclude, self.regex, self.on_path, self.case)?;
        let min = parse_size(&self.min_text).map_err(|e| format!("{}: {e}", tr("Tamaño mínimo")))?;
        let max = parse_size(&self.max_text).map_err(|e| format!("{}: {e}", tr("Tamaño máximo")))?;
        if pat.is_none() && min.is_none() && max.is_none() && self.older_days == 0 && self.newer_days == 0 {
            return Err(tr("Escribe al menos un patrón o un filtro de tamaño o de fecha; si no, coincidiría todo.").to_string());
        }
        if !self.files && !self.folders {
            return Err(tr("Marca si buscar archivos, carpetas o ambos.").to_string());
        }
        Ok(Cfg {
            pat,
            excl,
            on_path: self.on_path,
            files: self.files,
            folders: self.folders && compile(&self.patterns, self.regex, self.on_path, self.case).map(|p| p.is_some()).unwrap_or(false),
            recursive: self.recursive,
            depth: self.depth,
            skip_dirs: crate::rename::name_matcher(&self.skip_dirs, self.regex).map_err(|e| trf!("Expresión no válida: {}", e))?,
            skip_hidden: self.skip_hidden,
            readonly: self.readonly,
            min: min.unwrap_or(0),
            max: max.unwrap_or(u64::MAX),
            older_secs: self.older_days as u64 * 86_400,
            newer_secs: self.newer_days as u64 * 86_400,
        })
    }

    fn start_scan(&mut self) {
        if self.roots.is_empty() {
            self.status = Some((tr("Añade al menos una carpeta donde buscar.").to_string(), true));
            return;
        }
        let cfg = match self.config() {
            Ok(c) => c,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let roots = self.roots.clone();
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        if let Ok(mut s) = progress.stage.lock() {
            *s = tr("Buscando").to_string();
        }
        std::thread::spawn(move || {
            let (items, errs, scanned, truncated) = scan(&roots, &cfg, &p2, &c2);
            let _ = tx.send(Msg::Found(items, errs, scanned, truncated));
        });
        self.found.clear();
        self.status = None;
        self.problems.clear();
        self.confirm = false;
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn sort_found(&mut self) {
        match self.sort {
            0 => self.found.sort_by(|a, b| a.path.cmp(&b.path)),
            1 => self.found.sort_by_key(|i| std::cmp::Reverse(i.size)),
            2 => self.found.sort_by_key(|i| i.modified),
            _ => self.found.sort_by_key(|i| i.path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()),
        }
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok(msg) = job.rx.try_recv() else { return };
        let secs = job.started.elapsed().as_secs_f32();
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        match msg {
            Msg::Found(items, errs, scanned, truncated) => {
                self.found = items;
                self.problems = errs;
                self.scanned = scanned;
                self.truncated = truncated;
                self.sort_found();
                let text = if cancelled { tr("Búsqueda cancelada: se muestra lo encontrado hasta ahora").to_string() } else { trf!("Búsqueda terminada en {} s", format!("{secs:.1}")) };
                self.status = Some((text, cancelled));
            }
            Msg::Done(gone, errs, msg) => {
                let gone: std::collections::HashSet<PathBuf> = gone.into_iter().collect();
                self.found.retain(|i| !gone.contains(&i.path));
                self.problems = errs;
                self.status = Some((msg, !self.problems.is_empty()));
                self.confirm = false;
                events.push(ToolEvent::Reload);
            }
        }
    }

    fn start_delete(&mut self) {
        let items: Vec<(PathBuf, PathBuf, bool, u64)> = self.found.iter().filter(|i| i.on).map(|i| (i.path.clone(), self.roots[i.root].clone(), i.is_dir, i.size)).collect();
        if items.is_empty() {
            return;
        }
        let (method, dest, passes, clean) = (self.method, PathBuf::from(self.dest.trim()), self.passes, self.clean_empty);
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        progress.total.store(items.len(), Ordering::Relaxed);
        if let Ok(mut s) = progress.stage.lock() {
            *s = tr("Eliminando").to_string();
        }
        std::thread::spawn(move || {
            let (mut gone, mut errs, mut freed) = (vec![], vec![], 0u64);
            for (path, root, is_dir, size) in &items {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(mut c) = p2.current.lock() {
                    *c = path.to_string_lossy().to_string();
                }
                p2.done.fetch_add(1, Ordering::Relaxed);
                let r: Result<(), String> = match method {
                    0 => trash::delete(path).map_err(|e| e.to_string()),
                    1 => fsops::remove_permanently(path).map_err(|e| e.to_string()),
                    2 => fs::create_dir_all(&dest)
                        .and_then(|_| fsops::move_path(path, &fsops::unique_name(&dest, &path.file_name().unwrap_or_default().to_string_lossy())))
                        .map_err(|e| e.to_string()),
                    _ => shred_tree(path, passes),
                };
                match r {
                    Ok(()) => {
                        freed += size;
                        gone.push(path.clone());
                        if clean && !*is_dir {
                            clean_parents(path, root);
                        }
                    }
                    Err(e) => errs.push(format!("{}: {e}", path.display())),
                }
            }
            let msg = match method {
                0 => trf!("{} elemento(s) a la papelera · {} liberados al vaciarla", gone.len(), human_size(freed)),
                2 => trf!("{} elemento(s) movidos · {} liberados aquí", gone.len(), human_size(freed)),
                _ => trf!("{} elemento(s) eliminados · {} liberados", gone.len(), human_size(freed)),
            };
            let _ = tx.send(Msg::Done(gone, errs, msg));
        });
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn export_csv(&mut self, events: &mut Vec<ToolEvent>) {
        let mut out = String::from("tipo;tamaño_bytes;modificado;marcado;ruta\n");
        for i in &self.found {
            out.push_str(&format!("{};{};{};{};\"{}\"\n", if i.is_dir { "carpeta" } else { "archivo" }, i.size, fsops::format_time(i.modified), if i.on { "sí" } else { "no" }, i.path.to_string_lossy().replace('"', "\"\"")));
        }
        let file = fsops::unique_name(&self.base, "coincidencias.csv");
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
        egui::ScrollArea::vertical().id_salt("purge_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.roots_ui(ui, selection, cwd);
                ui.separator();
                self.pattern_ui(ui);
                self.options_ui(ui);
            });
            ui.separator();
            self.search_ui(ui);
            self.results_ui(ui, events);
            if !self.problems.is_empty() {
                ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                    egui::ScrollArea::vertical().max_height(90.0).id_salt("purge_err").show(ui, |ui| {
                        for p in &self.problems {
                            ui.label(RichText::new(p).small().color(theme::p().danger));
                        }
                    });
                });
            }
        });
        let ctx = ui.ctx().clone();
        self.browser_ui(&ctx);
    }

    fn roots_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("CARPETAS DONDE BUSCAR")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("purge_roots").max_height(80.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, r) in self.roots.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la búsqueda")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(format!("{}. {}", k + 1, r.display()));
                    if broad(r) {
                        ui.label(RichText::new(tr("⚠ carpeta muy amplia")).small().color(theme::p().danger));
                    }
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
            let r = ui.add(egui::TextEdit::singleline(&mut self.root_input).hint_text(tr("o pega una ruta aquí")).desired_width(240.0));
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
            let start = self.roots.last().and_then(|r| r.parent().map(Path::to_path_buf)).or_else(dirs::home_dir).unwrap_or_else(|| cwd.to_path_buf());
            self.browser = Some(Browser::new(start, 0));
        }
    }

    fn pattern_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("QUÉ BORRAR")).small().strong().color(theme::p().sub));
            ui.menu_button(tr("✨ Plantillas"), |ui| {
                ui.label(RichText::new(tr("Rellenan los patrones; puedes ajustarlos después")).small().color(theme::p().muted));
                for (name, desc, f) in TEMPLATES {
                    if ui.button(tr(name)).on_hover_text(tr(desc)).clicked() {
                        f(self);
                        ui.close_menu();
                    }
                }
            });
        });
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(520.0);
                let hint = if self.regex { "\\.(tmp|bak)$\n^~\\$" } else { "*.tmp; *.bak; *~" };
                ui.add(egui::TextEdit::multiline(&mut self.patterns).hint_text(hint).font(egui::TextStyle::Monospace).desired_rows(3).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    crate::ui_kit::label(ui, tr("Proteger"));
                    ui.add(egui::TextEdit::singleline(&mut self.exclude).hint_text(tr("patrones que nunca se borran, p. ej. importante*")).desired_width(430.0))
                        .on_hover_text(tr("Lo que coincida con estos patrones no se borra; si es una carpeta, tampoco nada de lo que hay dentro."));
                });
                ui.label(RichText::new(tr(if self.regex { "Una expresión regular por línea; basta que coincida una parte del nombre." } else { "Varios patrones separados por «;» o en líneas distintas. * es cualquier texto y ? un carácter." })).small().color(theme::p().muted));
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.selectable_value(&mut self.regex, false, tr("Comodines * ?"));
                ui.selectable_value(&mut self.regex, true, tr("Expresión regular"));
                ui.checkbox(&mut self.case, tr("Distinguir mayúsculas"));
                ui.checkbox(&mut self.on_path, tr("Comparar con la ruta, no solo con el nombre"))
                    .on_hover_text(tr("Con la ruta puedes escribir cosas como «cache/*.dat» o «**/temp/*». La ruta se cuenta desde la carpeta de búsqueda."));
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.checkbox(&mut self.files, tr("Archivos"));
                ui.checkbox(&mut self.folders, tr("Carpetas enteras"))
                    .on_hover_text(tr("Si el nombre de una carpeta coincide, se borra la carpeta con todo lo que contiene."));
            });
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(tr("🔎 Filtros y alcance")).default_open(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(380.0);
                    size_field(ui, tr("Tamaño mínimo"), &mut self.min_text, tr("sin límite"));
                    size_field(ui, tr("Tamaño máximo"), &mut self.max_text, tr("sin límite"));
                    ui.horizontal(|ui| {
                        crate::ui_kit::label(ui, tr("Más antiguos de"));
                        ui.add(egui::DragValue::new(&mut self.older_days).range(0..=36500).suffix(tr(" días"))).on_hover_text(tr("Solo lo modificado hace al menos estos días (0 = sin filtro)"));
                    });
                    ui.horizontal(|ui| {
                        crate::ui_kit::label(ui, tr("Modificados hace"));
                        ui.add(egui::DragValue::new(&mut self.newer_days).range(0..=36500).suffix(tr(" días"))).on_hover_text(tr("Solo lo modificado en los últimos días indicados (0 = sin filtro)"));
                    });
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas"));
                    ui.add_enabled_ui(self.recursive, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(tr("Profundidad máxima"));
                            ui.add(egui::DragValue::new(&mut self.depth).range(0..=64)).on_hover_text(tr("0 = sin límite; 1 = solo la carpeta, sin subcarpetas"));
                        });
                    });
                    ui.horizontal(|ui| {
                        crate::ui_kit::label(ui, tr("Saltar carpetas"));
                        ui.add(egui::TextEdit::singleline(&mut self.skip_dirs).desired_width(200.0)).on_hover_text(tr("Nombres de carpetas que no se recorren, con comodines, separados por «;»"));
                    });
                    ui.checkbox(&mut self.skip_hidden, tr("Saltar archivos y carpetas ocultos"));
                    ui.checkbox(&mut self.readonly, tr("Incluir archivos de solo lectura"));
                });
            });
        });
    }

    fn search_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(tr("🔍 Buscar coincidencias")).strong())).on_hover_text(tr("Solo busca y lista: no se borra nada hasta que lo pidas.")).clicked() {
                self.start_scan();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
                let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                if total > 0 {
                    ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(180.0).show_percentage());
                }
                ui.label(RichText::new(trf!("{} · {} revisados", stage, p.entries.load(Ordering::Relaxed))).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                if let Ok(c) = p.current.lock() {
                    ui.label(RichText::new(c.as_str()).small().color(theme::p().muted));
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let busy = self.job.is_some();
        if self.found.is_empty() {
            if !busy && self.status.as_ref().map(|s| !s.1).unwrap_or(false) && self.scanned > 0 {
                ui.label(trf!("No hay nada que coincida entre {} elementos revisados.", self.scanned));
            }
            return;
        }
        let on_n = self.found.iter().filter(|i| i.on).count();
        let bytes: u64 = self.found.iter().filter(|i| i.on).map(|i| i.size).sum();
        let dirs = self.found.iter().filter(|i| i.on && i.is_dir).count();
        ui.separator();
        ui.label(RichText::new(trf!("{} coincidencias · {} marcadas ({} archivos, {} carpetas) · {}", self.found.len(), on_n, on_n - dirs, dirs, human_size(bytes))).strong());
        if self.truncated {
            ui.label(RichText::new(tr("Hay demasiadas coincidencias: solo se muestran las primeras 100 000. Afina el patrón.")).small().color(theme::p().danger));
        }
        ui.horizontal_wrapped(|ui| {
            let before = self.sort;
            egui::ComboBox::from_id_salt("purge_sort").width(170.0).selected_text(tr(SORTS[self.sort as usize])).show_ui(ui, |ui| {
                for (k, l) in SORTS.iter().enumerate() {
                    ui.selectable_value(&mut self.sort, k as u8, tr(l));
                }
            });
            if before != self.sort {
                self.sort_found();
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar la lista")).desired_width(160.0));
            if ui.small_button(tr("Marcar todo")).clicked() {
                self.found.iter_mut().for_each(|i| i.on = true);
            }
            if ui.small_button(tr("Desmarcar todo")).clicked() {
                self.found.iter_mut().for_each(|i| i.on = false);
            }
            if ui.small_button(tr("Invertir")).clicked() {
                self.found.iter_mut().for_each(|i| i.on = !i.on);
            }
            if ui.small_button(tr("📋 Copiar rutas")).clicked() {
                events.push(ToolEvent::Clipboard(self.found.iter().filter(|i| i.on).map(|i| i.path.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n")));
            }
            if ui.small_button(tr("💾 Exportar CSV")).clicked() {
                self.export_csv(events);
            }
        });
        let q = self.filter.to_lowercase();
        let salt = ((self.found.len() as u64) << 8) | self.sort as u64;
        let view = self.view_cache.get(&self.filter, salt, || self.found.iter().enumerate().filter(|(_, i)| q.is_empty() || i.path.to_string_lossy().to_lowercase().contains(&q)).map(|(k, _)| k).collect()); 
        let row_h = 22.0;
        let mut toggles = vec![];
        egui::ScrollArea::vertical().id_salt("purge_list").max_height(crate::tools::grow(ui, 240.0)).auto_shrink([false, false]).show_rows(ui, row_h, view.len(), |ui, range| {
            for &k in &view[range] {
                let it = &self.found[k];
                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                if k % 2 == 1 {
                    ui.painter().rect_filled(rect, 0.0, theme::p().stripe);
                }
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                let mut on = it.on;
                if child.checkbox(&mut on, "").changed() {
                    toggles.push(k);
                }
                child.add_sized([22.0, row_h], egui::Label::new(if it.is_dir { "📁" } else { "🗋" }));
                child.add_sized([70.0, row_h], egui::Label::new(RichText::new(human_size(it.size)).small().color(theme::p().sub)));
                child.add_sized([120.0, row_h], egui::Label::new(RichText::new(fsops::format_time(it.modified)).small().color(theme::p().sub)));
                let shown = it.path.strip_prefix(&self.roots[it.root]).map(|r| format!("{} · {}", self.roots[it.root].file_name().unwrap_or_default().to_string_lossy(), r.display())).unwrap_or_else(|_| it.path.to_string_lossy().to_string());
                let text = if it.is_dir { format!("{shown}  ({} {})", it.files, tr("archivos")) } else { shown };
                child.add(egui::Label::new(RichText::new(text).color(if it.on { theme::p().text } else { theme::p().muted })).truncate()).on_hover_text(it.path.to_string_lossy());
            }
        });
        for k in toggles {
            self.found[k].on = !self.found[k].on;
        }
        let on_n = self.found.iter().filter(|i| i.on).count();

        // Deletion
        ui.separator();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("Al borrar")).small().strong().color(theme::p().sub));
                for (k, l) in METHODS.iter().enumerate() {
                    ui.selectable_value(&mut self.method, k as u8, tr(l));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if self.method == 2 {
                    ui.add(egui::TextEdit::singleline(&mut self.dest).hint_text(tr("carpeta destino")).desired_width(260.0));
                    if ui.button(tr("📂 Elegir…")).clicked() {
                        let start = PathBuf::from(self.dest.trim());
                        let start = if start.is_dir() { start } else { dirs::home_dir().unwrap_or_else(|| self.base.clone()) };
                        self.browser = Some(Browser::new(start, 1));
                    }
                }
                if self.method == 3 {
                    ui.label(tr("Pasadas de sobrescritura:"));
                    ui.add(egui::DragValue::new(&mut self.passes).range(1..=7));
                    ui.label(RichText::new(tr("En SSD y algunos sistemas de archivos no se garantiza que los datos desaparezcan del todo.")).small().color(theme::p().muted));
                }
                ui.checkbox(&mut self.clean_empty, tr("Quitar también las carpetas que queden vacías"));
            });
        });
        let any_broad = self.found.iter().any(|i| i.on && broad(&self.roots[i.root]));
        let needs_confirm = matches!(self.method, 1 | 3) || any_broad;
        if needs_confirm && on_n > 0 {
            let text = if any_broad { tr("La búsqueda incluye una carpeta muy amplia (el disco entero o tu carpeta personal). Entiendo el riesgo.") } else { tr("Entiendo que esto no se puede deshacer.") };
            ui.checkbox(&mut self.confirm, RichText::new(text).color(theme::p().danger));
        }
        let dest_ok = self.method != 2 || !self.dest.trim().is_empty();
        let can = !busy && on_n > 0 && dest_ok && (!needs_confirm || self.confirm);
        let label = match self.method {
            0 => trf!("🗑 Mover {} a la papelera", on_n),
            1 => trf!("⚠ Eliminar definitivamente {}", on_n),
            2 => trf!("📦 Mover {} a la carpeta", on_n),
            _ => trf!("🔥 Destruir {}", on_n),
        };
        let danger = matches!(self.method, 1 | 3);
        let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(if danger { theme::p().danger } else { theme::accent() });
        if ui.add_enabled(can, btn).clicked() {
            self.start_delete();
        }
        if !dest_ok {
            ui.label(RichText::new(tr("Elige la carpeta destino.")).small().color(theme::p().danger));
        }
    }

    fn browser_ui(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.browser.take() else { return };
        let mut open = true;
        let (mut nav, mut pick): (Option<PathBuf>, Option<PathBuf>) = (None, None);
        let mut close = false;
        let purpose = b.purpose;
        egui::Window::new(tr("Elegir carpeta")).id(egui::Id::new("purge_browser")).open(&mut open).default_width(480.0).default_height(440.0).show(ctx, |ui| {
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
            ui.separator();
            egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 300.0)).auto_shrink([false, false]).show(ui, |ui| {
                if b.entries.is_empty() {
                    ui.label(RichText::new(tr("No hay subcarpetas.")).color(theme::p().muted));
                }
                for (name, p) in &b.entries {
                    ui.horizontal(|ui| {
                        if ui.small_button("✔").on_hover_text(tr("Elegir esta carpeta")).clicked() {
                            pick = Some(p.clone());
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
                if ui.button(trf!("✔ Usar «{}»", name)).on_hover_text(b.path.to_string_lossy()).clicked() {
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
        let mut keep = open && !close;
        if let Some(p) = pick {
            if purpose == 0 {
                self.add_root(p);
            } else {
                self.dest = p.to_string_lossy().to_string();
                keep = false;
            }
        }
        if keep {
            self.browser = Some(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tree() -> (PathBuf, Purge) {
        let dir = std::env::temp_dir().join(format!("purge-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
        let _ = fs::remove_dir_all(&dir);
        for d in ["a/sub", "b", "node_modules/x", "protegida"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for (f, n) in [("a/uno.tmp", 5), ("a/dos.TMP", 5), ("a/sub/tres.bak", 20), ("a/keep.txt", 3), ("b/viejo.tmp", 5), ("protegida/ok.tmp", 5), ("node_modules/x/lib.js", 9)] {
            fs::write(dir.join(f), vec![b'x'; n]).unwrap();
        }
        let mut p = Purge::new(vec![dir.clone()], dir.clone());
        p.skip_dirs = String::new();
        (dir, p)
    }

    fn names(items: &[Item], root: &Path) -> Vec<String> {
        let mut v: Vec<String> = items.iter().map(|i| i.path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")).collect();
        v.sort();
        v
    }

    fn run(p: &Purge) -> Vec<Item> {
        let cfg = p.config().unwrap();
        scan(&p.roots, &cfg, &Progress::default(), &AtomicBool::new(false)).0
    }

    #[test]
    fn wildcards_exclusions_and_case() {
        let (dir, mut p) = tree();
        p.patterns = "*.tmp".into();
        assert_eq!(names(&run(&p), &dir), vec!["a/dos.TMP", "a/uno.tmp", "b/viejo.tmp", "protegida/ok.tmp"], "case-insensitive by default");
        p.case = true;
        assert_eq!(run(&p).len(), 3);
        p.case = false;
        p.exclude = "protegida".into();
        assert_eq!(run(&p).len(), 3, "a protected folder isn't entered");
        p.exclude = "uno*".into();
        assert!(!names(&run(&p), &dir).contains(&"a/uno.tmp".to_string()));
        p.recursive = false;
        assert!(run(&p).is_empty(), "no subfolders: nothing at the top level");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_regex_folders_size_and_age() {
        let (dir, mut p) = tree();
        p.on_path = true;
        p.patterns = "a/*.tmp".into();
        assert_eq!(names(&run(&p), &dir), vec!["a/dos.TMP", "a/uno.tmp"]);
        p.patterns = "**/sub/*".into();
        assert_eq!(names(&run(&p), &dir), vec!["a/sub/tres.bak"]);
        p.on_path = false;
        p.regex = true;
        p.patterns = r"\.(bak|tmp)$".into();
        assert_eq!(run(&p).len(), 5);
        p.min_text = "10 B".into();
        assert_eq!(names(&run(&p), &dir), vec!["a/sub/tres.bak"]);
        p.min_text = String::new();
        // Whole folders by name.
        p.regex = false;
        p.patterns = "node_modules".into();
        p.files = false;
        p.folders = true;
        let found = run(&p);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_dir && found[0].files >= 1);
        // Age: only what is at least 10 days old.
        p.folders = false;
        p.files = true;
        p.patterns = "viejo.tmp".into();
        let f = fs::OpenOptions::new().write(true).open(dir.join("b/viejo.tmp")).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(30 * 86_400)).unwrap();
        p.older_days = 10;
        assert_eq!(run(&p).len(), 1);
        p.older_days = 0;
        p.newer_days = 10;
        assert!(run(&p).is_empty());
        // Nothing to match on is refused.
        p.patterns = String::new();
        p.newer_days = 0;
        assert!(p.config().is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_methods_and_empty_folder_cleanup() {
        let (dir, mut p) = tree();
        p.patterns = "*.bak".into();
        let items = run(&p);
        assert_eq!(items.len(), 1);
        // Move aside, then clean the emptied parent folder.
        let dest = dir.join("apartado");
        let item = &items[0];
        fsops::move_path(&item.path, &fsops::unique_name(&{ fs::create_dir_all(&dest).unwrap(); dest.clone() }, "tres.bak")).unwrap();
        clean_parents(&item.path, &dir);
        assert!(dest.join("tres.bak").exists());
        assert!(!dir.join("a/sub").exists(), "emptied folder removed");
        assert!(dir.join("a").exists(), "but not folders that still have files");
        // Secure erase of a folder tree.
        shred_tree(&dir.join("node_modules"), 1).unwrap();
        assert!(!dir.join("node_modules").exists());
        assert!(broad(Path::new("/")) && !broad(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }
}
