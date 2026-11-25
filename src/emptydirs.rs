//! Remove empty folders: choose the folders to clean with "Examinar…", with or without
//! subfolders, count junk files (Thumbs.db, .DS_Store…) as nothing, protect folders by name,
//! look at the list, tick what to remove and send it to the trash or delete it for good.

use crate::i18n::tr;
use crate::roots::RootList;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

#[derive(Clone)]
struct Cfg {
    recursive: bool,
    include_root: bool,
    junk: Option<Regex>,
    protect: Option<Regex>,
    hidden_is_junk: bool,
}

#[derive(Clone)]
struct Found {
    path: PathBuf,
    /// Empty folders inside it that go with it.
    nested: usize,
    /// Junk files inside it that go with it.
    junk: usize,
    on: bool,
}

/// What a folder holds, looking at everything below it.
struct Seen {
    empty: bool,
    nested: usize,
    junk: usize,
}

fn walk(dir: &Path, depth: usize, root: &Path, c: &Cfg, out: &mut Vec<Found>, errs: &mut Vec<String>, cancel: &AtomicBool) -> Seen {
    let none = Seen { empty: false, nested: 0, junk: 0 };
    if cancel.load(Ordering::Relaxed) {
        return none;
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            errs.push(format!("{}: {e}", dir.display()));
            return none;
        }
    };
    let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // A protected folder is never reported, and keeps its parents from counting as empty.
    if depth > 0 && c.protect.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
        return none;
    }
    let mut empty = true;
    let (mut nested, mut junk) = (0, 0);
    let mut kids: Vec<(PathBuf, usize)> = vec![]; // empty children and their own nested counts
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let Ok(ty) = e.file_type() else {
            empty = false;
            continue;
        };
        if ty.is_symlink() {
            empty = false;
        } else if ty.is_dir() {
            let before = out.len();
            let s = walk(&e.path(), depth + 1, root, c, out, errs, cancel);
            if s.empty {
                // Reported by the parent as one (its own entry is folded into us below).
                out.truncate(before);
                kids.push((e.path(), s.nested));
                nested += 1 + s.nested;
                junk += s.junk;
            } else {
                empty = false;
            }
        } else if c.junk.as_ref().map(|re| re.is_match(&n)).unwrap_or(false) || (c.hidden_is_junk && n.starts_with('.')) {
            junk += 1;
        } else {
            empty = false;
        }
    }
    if empty {
        return Seen { empty: true, nested, junk };
    }
    // This folder stays: its empty children are the topmost empty ones.
    for (p, n) in kids {
        if c.recursive || depth == 0 {
            // Without subfolders only the direct children of the chosen folder are listed.
            out.push(Found { path: p, nested: n, junk: 0, on: true });
        }
    }
    none
}

fn scan(roots: &[PathBuf], c: &Cfg, cancel: &AtomicBool) -> (Vec<Found>, Vec<String>) {
    let (mut out, mut errs) = (vec![], vec![]);
    for root in roots {
        let mut found = vec![];
        let s = walk(root, 0, root, c, &mut found, &mut errs, cancel);
        if s.empty && c.include_root {
            found.push(Found { path: root.clone(), nested: s.nested, junk: s.junk, on: true });
        }
        out.extend(found);
    }
    // Junk counts per reported folder, for the list.
    for f in &mut out {
        if f.junk == 0 {
            f.junk = count_junk(&f.path, c);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    (out, errs)
}

fn count_junk(dir: &Path, c: &Cfg) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) if c.junk.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) || (c.hidden_is_junk && name.starts_with('.')) => n += 1,
                _ => {}
            }
        }
    }
    n
}

/// Removes one reported folder with everything in it (all of it empty folders or junk).
fn remove(p: &Path, permanent: bool) -> Result<(), String> {
    if permanent {
        fs::remove_dir_all(p).map_err(|e| e.to_string())
    } else {
        trash::delete(p).map_err(|e| e.to_string())
    }
}

enum Msg {
    Found(Vec<Found>, Vec<String>),
    Removed(usize, Vec<String>),
}

pub struct EmptyDirs {
    roots: RootList,
    recursive: bool,
    include_root: bool,
    junk: String,
    protect: String,
    hidden_is_junk: bool,
    permanent: bool,
    confirm: bool,
    found: Vec<Found>,
    searched: bool,
    filter: String,
    problems: Vec<String>,
    rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    status: Option<(String, bool)>,
}

impl EmptyDirs {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> EmptyDirs {
        let folders: Vec<PathBuf> = if targets.iter().any(|p| p.is_dir()) { targets.into_iter().filter(|p| p.is_dir()).collect() } else { vec![base] };
        EmptyDirs {
            roots: RootList::new(folders, "emptydirs_roots"),
            recursive: true,
            include_root: false,
            junk: "Thumbs.db; desktop.ini; .DS_Store; ._*".into(),
            protect: ".git; .svn; node_modules".into(),
            hidden_is_junk: false,
            permanent: false,
            confirm: false,
            found: vec![],
            searched: false,
            filter: String::new(),
            problems: vec![],
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            status: None,
        }
    }

    /// Looks for empty folders at once, for trying the tool (`--empty`).
    pub fn demo(&mut self, dir: &Path) {
        self.roots = RootList::new(vec![dir.to_path_buf()], "emptydirs_roots");
        self.start();
    }

    fn cfg(&self) -> Result<Cfg, String> {
        let m = |t: &str| crate::rename::name_matcher(t, false).map_err(|e| trf!("Expresión no válida: {}", e));
        Ok(Cfg { recursive: self.recursive, include_root: self.include_root, junk: m(&self.junk)?, protect: m(&self.protect)?, hidden_is_junk: self.hidden_is_junk })
    }

    fn start(&mut self) {
        if self.roots.roots.is_empty() {
            self.status = Some((tr("Añade al menos una carpeta.").to_string(), true));
            return;
        }
        let cfg = match self.cfg() {
            Ok(c) => c,
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let roots = self.roots.roots.clone();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (f, e) = scan(&roots, &cfg, &cancel);
            let _ = tx.send(Msg::Found(f, e));
        });
        self.found.clear();
        self.problems.clear();
        self.status = None;
        self.rx = Some(rx);
    }

    fn remove_marked(&mut self) {
        let list: Vec<PathBuf> = self.found.iter().filter(|f| f.on).map(|f| f.path.clone()).collect();
        let permanent = self.permanent;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (mut ok, mut errs) = (0, vec![]);
            // Deepest first, in case two marked folders contain one another.
            let mut list = list;
            list.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for p in list {
                if !p.exists() {
                    continue;
                }
                match remove(&p, permanent) {
                    Ok(()) => ok += 1,
                    Err(e) => errs.push(format!("{}: {e}", p.display())),
                }
            }
            let _ = tx.send(Msg::Removed(ok, errs));
        });
        self.rx = Some(rx);
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(rx) = &self.rx else { return };
        let Ok(msg) = rx.try_recv() else { return };
        self.rx = None;
        match msg {
            Msg::Found(f, e) => {
                self.searched = true;
                self.found = f;
                self.problems = e;
            }
            Msg::Removed(ok, errs) => {
                self.status = Some((trf!("{} carpeta(s) eliminadas · {} error(es)", ok, errs.len()), !errs.is_empty()));
                self.problems = errs;
                self.confirm = false;
                events.push(ToolEvent::Reload);
                let gone: HashSet<PathBuf> = self.found.iter().filter(|f| f.on && !f.path.exists()).map(|f| f.path.clone()).collect();
                self.found.retain(|f| !gone.contains(&f.path));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let busy = self.rx.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            self.roots.ui(ui, selection, cwd, tr("CARPETAS A LIMPIAR"));
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas")).on_hover_text(tr("Si no, solo se miran las carpetas que están directamente dentro de las elegidas."));
                ui.checkbox(&mut self.include_root, tr("Incluir también la carpeta elegida si queda vacía"));
                ui.checkbox(&mut self.hidden_is_junk, tr("Los archivos ocultos cuentan como basura"));
            });
            egui::Grid::new("emptydirs_grid").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                ui.label(tr("Cuentan como vacío"));
                ui.add(egui::TextEdit::singleline(&mut self.junk).desired_width(f32::INFINITY)).on_hover_text(tr("Una carpeta que solo tiene archivos así se considera vacía y se elimina con ellos. Patrones separados por «;»."));
                ui.end_row();
                ui.label(tr("Carpetas protegidas"));
                ui.add(egui::TextEdit::singleline(&mut self.protect).desired_width(f32::INFINITY)).on_hover_text(tr("Estas carpetas nunca se tocan, y las que las contienen tampoco quedan vacías."));
                ui.end_row();
            });
        });
        ui.separator();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🔍 Buscar carpetas vacías")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy, btn).clicked() {
                self.start();
            }
            if busy {
                ui.spinner();
                if ui.button(tr("Cancelar")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if self.searched && self.found.is_empty() && !busy && self.status.is_none() {
            ui.label(tr("No hay carpetas vacías."));
        }
        if !self.found.is_empty() {
            self.results_ui(ui, busy);
        }
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in self.problems.iter().take(50) {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.separator();
        let marked = self.found.iter().filter(|f| f.on).count();
        let total_nested: usize = self.found.iter().filter(|f| f.on).map(|f| f.nested).sum();
        ui.label(RichText::new(trf!("{} carpeta(s) vacías · {} marcada(s) (con {} subcarpetas vacías dentro)", self.found.len(), marked, total_nested)).strong());
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar por ruta")).desired_width(180.0));
            if ui.small_button(tr("Marcar todo")).clicked() {
                self.found.iter_mut().for_each(|f| f.on = true);
            }
            if ui.small_button(tr("Desmarcar todo")).clicked() {
                self.found.iter_mut().for_each(|f| f.on = false);
            }
        });
        let q = self.filter.to_lowercase();
        let view: Vec<usize> = if q.is_empty() { (0..self.found.len()).collect() } else { (0..self.found.len()).filter(|&i| self.found[i].path.to_string_lossy().to_lowercase().contains(&q)).collect() };
        let base = self.roots.roots.first().cloned().unwrap_or_default();
        egui::ScrollArea::vertical().id_salt("emptydirs_rows").max_height(crate::tools::grow(ui, 280.0)).auto_shrink([false, false]).show_rows(ui, 22.0, view.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &i in &view[range] {
                let f = &mut self.found[i];
                ui.horizontal(|ui| {
                    ui.set_height(22.0);
                    ui.checkbox(&mut f.on, "");
                    let rel = self.roots.roots.iter().find_map(|r| f.path.strip_prefix(r).ok()).unwrap_or_else(|| f.path.strip_prefix(&base).unwrap_or(&f.path)).to_string_lossy().to_string();
                    ui.add(egui::Label::new(RichText::new(if rel.is_empty() { f.path.to_string_lossy().to_string() } else { rel })).truncate()).on_hover_text(f.path.to_string_lossy());
                    if f.nested > 0 {
                        ui.label(RichText::new(trf!("+{} subcarpetas", f.nested)).small().color(theme::p().muted));
                    }
                    if f.junk > 0 {
                        ui.label(RichText::new(trf!("+{} archivos basura", f.junk)).small().color(theme::p().muted));
                    }
                });
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.permanent, tr("Eliminar definitivamente (sin papelera)"));
            if self.permanent {
                ui.checkbox(&mut self.confirm, tr("Confirmo que no se podrán recuperar"));
            }
            let ready = marked > 0 && !busy && (!self.permanent || self.confirm);
            let label = if self.permanent { trf!("⚠ Eliminar {} carpeta(s)", marked) } else { trf!("🗑 Mover {} carpeta(s) a la papelera", marked) };
            let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(theme::p().danger);
            if ui.add_enabled(ready, btn).clicked() {
                self.remove_marked();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Cfg {
        Cfg { recursive: true, include_root: false, junk: crate::rename::name_matcher("Thumbs.db; .DS_Store", false).unwrap(), protect: crate::rename::name_matcher(".git", false).unwrap(), hidden_is_junk: false }
    }

    fn tree() -> PathBuf {
        let d = std::env::temp_dir().join(format!("empty-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
        let _ = fs::remove_dir_all(&d);
        for p in ["vacia", "anidada/uno/dos", "con_basura/sub", "con_archivo/sub", ".git/objects", "a/b/vacia2"] {
            fs::create_dir_all(d.join(p)).unwrap();
        }
        fs::write(d.join("con_basura/Thumbs.db"), "x").unwrap();
        fs::write(d.join("con_archivo/real.txt"), "x").unwrap();
        fs::write(d.join("a/b/dato.txt"), "x").unwrap();
        d
    }

    fn names(d: &Path, f: &[Found]) -> Vec<String> {
        f.iter().map(|f| f.path.strip_prefix(d).unwrap().to_string_lossy().replace('\\', "/")).collect()
    }

    #[test]
    fn topmost_empty_folders_junk_and_protection() {
        let d = tree();
        let (f, _) = scan(&[d.clone()], &cfg(), &AtomicBool::new(false));
        assert_eq!(names(&d, &f), ["a/b/vacia2", "anidada", "con_archivo/sub", "con_basura", "vacia"], "git is protected; folders with real files stay");
        let anidada = f.iter().find(|x| x.path.ends_with("anidada")).unwrap();
        assert_eq!(anidada.nested, 2, "uno and dos go with it");
        assert_eq!(f.iter().find(|x| x.path.ends_with("con_basura")).unwrap().junk, 1);
        // Without subfolders only the direct children are listed.
        let mut c = cfg();
        c.recursive = false;
        let (f, _) = scan(&[d.clone()], &c, &AtomicBool::new(false));
        assert_eq!(names(&d, &f), ["anidada", "con_basura", "vacia"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn root_itself_and_removal() {
        let d = tree();
        let mut c = cfg();
        let (f, _) = scan(&[d.join("anidada")], &c, &AtomicBool::new(false));
        assert!(f.is_empty(), "the root is only reported when asked");
        c.include_root = true;
        let (f, _) = scan(&[d.join("anidada")], &c, &AtomicBool::new(false));
        assert_eq!(f.len(), 1);
        assert!(f[0].path.ends_with("anidada") && f[0].nested == 2);
        remove(&d.join("con_basura"), true).unwrap();
        assert!(!d.join("con_basura").exists() && d.join("con_archivo").exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
