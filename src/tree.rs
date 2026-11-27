//! Folder tree and listings: choose folders (and loose files), with or without subfolders and
//! up to a depth, see them drawn as a tree like the `tree` command or as a plain list of names
//! or paths, with totals (files, folders, size, deepest level, biggest file, size by type),
//! then copy the text or save it to a file.

use crate::fsops::{format_time, human_size};
use crate::i18n::tr;
use crate::roots::RootList;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::SystemTime;

/// Most entries read in one go, so a whole disk can't freeze the program.
const MAX_ENTRIES: usize = 200_000;

#[derive(Clone)]
struct Cfg {
    /// 0 = no limit.
    depth: usize,
    hidden: bool,
    filter: Option<Regex>,
}

#[derive(Clone, Default)]
pub struct Node {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    children: Vec<Node>,
    /// The folder has more inside, below the depth limit.
    cut: bool,
}

#[derive(Clone, Default)]
pub struct Stats {
    files: u64,
    dirs: u64,
    bytes: u64,
    deepest: usize,
    biggest: Option<(PathBuf, u64)>,
    newest: Option<(PathBuf, SystemTime)>,
    /// Extension → (files, bytes).
    by_ext: HashMap<String, (u64, u64)>,
    truncated: bool,
}

fn walk(dir: &Path, depth: usize, c: &Cfg, seen: &mut usize, errs: &mut Vec<String>, cancel: &AtomicBool) -> (Vec<Node>, bool) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            errs.push(format!("{}: {e}", dir.display()));
            return (vec![], false);
        }
    };
    if c.depth > 0 && depth >= c.depth {
        // Below the limit: only say whether there is more.
        return (vec![], rd.flatten().any(|e| c.hidden || !e.file_name().to_string_lossy().starts_with('.')));
    }
    let mut out = vec![];
    for e in rd.flatten() {
        if cancel.load(Ordering::Relaxed) || *seen >= MAX_ENTRIES {
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if !c.hidden && name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let ty = e.file_type().ok();
        let is_dir = ty.map(|t| t.is_dir()).unwrap_or(false);
        *seen += 1;
        if is_dir {
            let (children, cut) = walk(&path, depth + 1, c, seen, errs, cancel);
            // With a filter, folders without anything that matches are left out.
            if c.filter.is_some() && children.is_empty() {
                continue;
            }
            let modified = e.metadata().ok().and_then(|m| m.modified().ok());
            out.push(Node { name, path, is_dir, size: 0, modified, children, cut });
        } else {
            if let Some(re) = &c.filter {
                if !re.is_match(&name) {
                    continue;
                }
            }
            let m = e.metadata().ok();
            out.push(Node { name, path, is_dir, size: m.as_ref().map(|m| m.len()).unwrap_or(0), modified: m.and_then(|m| m.modified().ok()), children: vec![], cut: false });
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| crate::rename::natural_cmp(&a.name, &b.name)));
    (out, false)
}

fn scan(roots: &[PathBuf], c: &Cfg, cancel: &AtomicBool) -> (Vec<Node>, Stats, Vec<String>) {
    let (mut out, mut errs, mut seen) = (vec![], vec![], 0usize);
    for r in roots {
        let name = r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| r.to_string_lossy().to_string());
        let m = fs::metadata(r).ok();
        if r.is_dir() {
            let (children, cut) = walk(r, 0, c, &mut seen, &mut errs, cancel);
            out.push(Node { name, path: r.clone(), is_dir: true, size: 0, modified: m.and_then(|m| m.modified().ok()), children, cut });
        } else {
            out.push(Node { name, path: r.clone(), is_dir: false, size: m.as_ref().map(|m| m.len()).unwrap_or(0), modified: m.and_then(|m| m.modified().ok()), children: vec![], cut: false });
        }
    }
    let mut s = Stats { truncated: seen >= MAX_ENTRIES, ..Default::default() };
    for n in &mut out {
        total(n, 0, &mut s);
    }
    (out, s, errs)
}

/// Fills in folder sizes and adds everything to the totals. Returns the folder's size.
fn total(n: &mut Node, depth: usize, s: &mut Stats) -> u64 {
    s.deepest = s.deepest.max(depth);
    if n.is_dir {
        if depth > 0 {
            s.dirs += 1;
        }
        n.size = n.children.iter_mut().map(|c| total(c, depth + 1, s)).sum();
        return n.size;
    }
    s.files += 1;
    s.bytes += n.size;
    if s.biggest.as_ref().map(|b| n.size > b.1).unwrap_or(true) {
        s.biggest = Some((n.path.clone(), n.size));
    }
    if let Some(t) = n.modified {
        if s.newest.as_ref().map(|b| t > b.1).unwrap_or(true) {
            s.newest = Some((n.path.clone(), t));
        }
    }
    let ext = Path::new(&n.name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let e = s.by_ext.entry(ext).or_default();
    e.0 += 1;
    e.1 += n.size;
    n.size
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Tree,
    Names,
    Relative,
    Full,
    Details,
    Network,
    Csv,
}

impl Format {
    const ALL: [Format; 7] = [Format::Tree, Format::Names, Format::Relative, Format::Full, Format::Details, Format::Network, Format::Csv];
    fn label(self) -> &'static str {
        match self {
            Format::Tree => tr("Árbol"),
            Format::Names => tr("Solo nombres"),
            Format::Relative => tr("Rutas relativas"),
            Format::Full => tr("Rutas completas"),
            Format::Details => tr("Nombre, tamaño y fecha"),
            Format::Network => tr("Rutas de red (UNC)"),
            Format::Csv => "CSV",
        }
    }
}

#[derive(Clone, Copy)]
struct Show {
    format: Format,
    files: bool,
    sizes: bool,
    dates: bool,
}

fn render(nodes: &[Node], o: Show) -> String {
    let mut out = String::new();
    if o.format == Format::Csv {
        out.push_str("ruta;tipo;bytes;modificado\n");
    }
    for n in nodes {
        match o.format {
            Format::Tree => {
                out.push_str(&tree_line(n, o, true));
                out.push('\n');
                tree(&n.children, "", o, &mut out);
                if n.cut {
                    out.push_str("└── …\n");
                }
            }
            _ => flat(n, if n.is_dir { &n.path } else { n.path.parent().unwrap_or(Path::new("")) }, o, &mut out, true),
        }
    }
    out.trim_end().to_string()
}

fn extra(n: &Node, o: Show) -> String {
    let mut v = vec![];
    if o.sizes {
        v.push(if n.cut { format!("≥ {}", human_size(n.size)) } else { human_size(n.size) });
    }
    if o.dates {
        v.push(format_time(n.modified));
    }
    if v.is_empty() {
        String::new()
    } else {
        format!("  [{}]", v.join(" · "))
    }
}

fn tree_line(n: &Node, o: Show, root: bool) -> String {
    let name = if root { n.path.to_string_lossy().to_string() } else { n.name.clone() };
    format!("{}{}{}", name, if n.is_dir && !root { "/" } else { "" }, extra(n, o))
}

fn tree(nodes: &[Node], prefix: &str, o: Show, out: &mut String) {
    let shown: Vec<&Node> = nodes.iter().filter(|n| o.files || n.is_dir).collect();
    for (i, n) in shown.iter().enumerate() {
        let last = i + 1 == shown.len();
        out.push_str(prefix);
        out.push_str(if last { "└── " } else { "├── " });
        out.push_str(&tree_line(n, o, false));
        out.push('\n');
        let next = format!("{prefix}{}", if last { "    " } else { "│   " });
        tree(&n.children, &next, o, out);
        if n.cut {
            out.push_str(&next);
            out.push_str("└── …\n");
        }
    }
}

fn flat(n: &Node, base: &Path, o: Show, out: &mut String, root: bool) {
    // The chosen folders themselves are only listed in the detailed formats.
    let skip = root && n.is_dir && !matches!(o.format, Format::Csv);
    if !skip && (o.files || n.is_dir) {
        let line = match o.format {
            Format::Names => n.name.clone(),
            Format::Relative => n.path.strip_prefix(base).unwrap_or(&n.path).to_string_lossy().to_string(),
            Format::Full => n.path.to_string_lossy().to_string(),
            Format::Network => crate::netpath::network_path(&n.path).unwrap_or_else(|| n.path.to_string_lossy().to_string()),
            Format::Details => format!("{}\t{}\t{}", n.path.strip_prefix(base).unwrap_or(&n.path).to_string_lossy(), if n.is_dir { tr("<carpeta>").to_string() } else { human_size(n.size) }, format_time(n.modified)),
            Format::Csv => {
                let p = n.path.to_string_lossy().replace('"', "\"\"");
                format!("\"{p}\";{};{};{}", if n.is_dir { "carpeta" } else { "archivo" }, n.size, format_time(n.modified))
            }
            Format::Tree => unreachable!(),
        };
        out.push_str(&line);
        out.push('\n');
    }
    for c in &n.children {
        flat(c, base, o, out, false);
    }
}

pub struct TreeTool {
    roots: RootList,
    /// Loose files chosen in the file list, listed after the folders.
    files: Vec<PathBuf>,
    recursive: bool,
    depth: usize,
    hidden: bool,
    filter: String,
    regex: bool,
    format: Format,
    show_files: bool,
    sizes: bool,
    dates: bool,
    nodes: Vec<Node>,
    stats: Option<Stats>,
    text: String,
    problems: Vec<String>,
    rx: Option<Receiver<(Vec<Node>, Stats, Vec<String>)>>,
    cancel: Arc<AtomicBool>,
    status: Option<(String, bool)>,
}

impl TreeTool {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> TreeTool {
        let dirs: Vec<PathBuf> = targets.iter().filter(|p| p.is_dir()).cloned().collect();
        let files: Vec<PathBuf> = targets.iter().filter(|p| !p.is_dir()).cloned().collect();
        let folders = if dirs.is_empty() && files.is_empty() { vec![base] } else { dirs };
        let mut t = TreeTool {
            roots: RootList::new(folders, "tree_roots"),
            files,
            recursive: true,
            depth: 3,
            hidden: false,
            filter: String::new(),
            regex: false,
            format: Format::Tree,
            show_files: true,
            sizes: true,
            dates: false,
            nodes: vec![],
            stats: None,
            text: String::new(),
            problems: vec![],
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            status: None,
        };
        t.start();
        t
    }

    fn start(&mut self) {
        let filter = match crate::rename::name_matcher(&self.filter, self.regex) {
            Ok(f) => f,
            Err(e) => {
                self.status = Some((trf!("Expresión no válida: {}", e), true));
                return;
            }
        };
        let mut roots = self.roots.roots.clone();
        roots.extend(self.files.iter().cloned());
        if roots.is_empty() {
            self.status = Some((tr("Añade al menos una carpeta.").to_string(), true));
            return;
        }
        let cfg = Cfg { depth: if self.recursive { self.depth } else { 1 }, hidden: self.hidden, filter };
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(scan(&roots, &cfg, &cancel));
        });
        self.status = None;
        self.rx = Some(rx);
    }

    fn show(&self) -> Show {
        Show { format: self.format, files: self.show_files, sizes: self.sizes, dates: self.dates }
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let Ok((nodes, stats, errs)) = rx.try_recv() else { return };
        self.rx = None;
        self.nodes = nodes;
        self.stats = Some(stats);
        self.problems = errs;
        self.text = render(&self.nodes, self.show());
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll();
        let before_roots = self.roots.roots.clone();
        self.roots.ui(ui, selection, cwd, tr("CARPETAS"));
        if !self.files.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(trf!("y {} archivo(s) sueltos", self.files.len())).small().color(theme::p().sub)).on_hover_text(self.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
                if ui.small_button(tr("Quitar")).clicked() {
                    self.files.clear();
                    self.start();
                }
            });
        }
        let mut rescan = self.roots.roots != before_roots;
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Incluir"));
            rescan |= ui.checkbox(&mut self.recursive, tr("Subcarpetas")).changed();
            ui.add_enabled_ui(self.recursive, |ui| {
                ui.label(tr("Niveles"));
                rescan |= ui.add(egui::DragValue::new(&mut self.depth).range(0..=64).custom_formatter(|v, _| if v == 0.0 { tr("todos").to_string() } else { format!("{v}") })).on_hover_text(tr("Cuántos niveles de subcarpetas leer (0 = todos)")).changed();
            });
            rescan |= ui.checkbox(&mut self.hidden, tr("Ocultos")).changed();
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Filtro"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(if self.regex { "\\.(jpe?g|png)$" } else { "*.jpg, *.png, foto*" }).desired_width(170.0).font(egui::TextStyle::Monospace)).on_hover_text(tr("Solo los archivos que coinciden; puedes poner varios separados por comas. Las carpetas sin ninguno se ocultan."));
            rescan |= r.lost_focus() || (r.changed() && self.filter.is_empty());
            rescan |= ui.checkbox(&mut self.regex, tr("Expresión regular")).changed();
        });
        let mut redraw = false;
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Formato"));
            for f in Format::ALL {
                redraw |= ui.selectable_value(&mut self.format, f, f.label()).changed();
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Mostrar"));
            redraw |= ui.checkbox(&mut self.show_files, tr("Archivos")).on_hover_text(tr("Si no, solo las carpetas")).changed();
            if matches!(self.format, Format::Tree) {
                redraw |= ui.checkbox(&mut self.sizes, tr("Tamaños")).changed();
                redraw |= ui.checkbox(&mut self.dates, tr("Fechas")).changed();
            }
        });
        if rescan {
            self.start();
        }
        if redraw && self.rx.is_none() {
            self.text = render(&self.nodes, self.show());
        }
        ui.separator();
        let busy = self.rx.is_some();
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("Leyendo carpetas…"));
                if ui.button(tr("Cancelar")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(s) = &self.stats {
            self.stats_ui(ui, s);
        }
        let h = (ui.available_height() - 60.0).max(160.0);
        egui::Frame::new().stroke(egui::Stroke::new(1.0_f32, theme::p().muted.gamma_multiply(0.4))).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
            egui::ScrollArea::both().id_salt("arbol_texto").max_height(h).auto_shrink([false, false]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&self.text).monospace()).extend());
            });
        });
        ui.horizontal(|ui| {
            let lines = if self.text.is_empty() { 0 } else { self.text.lines().count() };
            let copy = egui::Button::new(RichText::new(tr("📋 Copiar al portapapeles")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(lines > 0, copy).clicked() {
                events.push(ToolEvent::Clipboard(self.text.clone()));
                self.status = Some((trf!("{} línea(s) copiadas", lines), false));
            }
            if ui.add_enabled(lines > 0, egui::Button::new(tr("💾 Guardar en un archivo"))).on_hover_text(tr("Se guarda en la primera carpeta de la lista")).clicked() {
                let dir = self.roots.roots.first().cloned().or_else(|| self.files.first().and_then(|f| f.parent().map(Path::to_path_buf))).unwrap_or_else(|| cwd.to_path_buf());
                let name = if self.format == Format::Csv { "lista.csv" } else if self.format == Format::Tree { "arbol.txt" } else { "lista.txt" };
                let out = crate::fsops::unique_name(&dir, name);
                match fs::write(&out, &self.text) {
                    Ok(()) => {
                        self.status = Some((trf!("Guardado en {}", out.display()), false));
                        events.push(ToolEvent::Reload);
                    }
                    Err(e) => self.status = Some((trf!("No se pudo guardar: {}", e), true)),
                }
            }
            if ui.add_enabled(!busy, egui::Button::new(tr("⟳ Volver a leer"))).clicked() {
                self.start();
            }
            if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in self.problems.iter().take(50) {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
    }

    fn stats_ui(&self, ui: &mut egui::Ui, s: &Stats) {
        let sub = theme::p().sub;
        ui.horizontal_wrapped(|ui| {
            let fact = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(RichText::new(k).small().color(sub));
                ui.label(RichText::new(v).strong());
                ui.add_space(10.0);
            };
            fact(ui, tr("Archivos"), s.files.to_string());
            fact(ui, tr("Carpetas"), s.dirs.to_string());
            fact(ui, tr("Tamaño total"), human_size(s.bytes));
            fact(ui, tr("Niveles"), s.deepest.to_string());
            if let Some((p, n)) = &s.biggest {
                ui.label(RichText::new(tr("Más grande")).small().color(sub));
                ui.label(RichText::new(format!("{} ({})", p.file_name().unwrap_or_default().to_string_lossy(), human_size(*n))).strong()).on_hover_text(p.to_string_lossy());
                ui.add_space(10.0);
            }
            if let Some((p, t)) = &s.newest {
                ui.label(RichText::new(tr("Más reciente")).small().color(sub));
                ui.label(RichText::new(format!("{} ({})", p.file_name().unwrap_or_default().to_string_lossy(), format_time(Some(*t)))).strong()).on_hover_text(p.to_string_lossy());
            }
        });
        if self.recursive && self.depth > 0 || !self.recursive {
            ui.label(RichText::new(trf!("Totales de lo leído: hasta {} nivel(es) de subcarpetas. Pon «Niveles» en «todos» para contarlo todo.", if self.recursive { self.depth } else { 1 })).small().color(theme::p().muted));
        }
        if s.truncated {
            ui.label(RichText::new(trf!("Hay demasiados elementos: se han leído los primeros {}. Baja los niveles o elige una carpeta más concreta.", MAX_ENTRIES)).small().color(theme::p().danger));
        }
        if !s.by_ext.is_empty() {
            let mut v: Vec<(&String, &(u64, u64))> = s.by_ext.iter().collect();
            v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(b.1 .0.cmp(&a.1 .0)));
            ui.collapsing(trf!("Por tipo de archivo ({})", v.len()), |ui| {
                egui::ScrollArea::vertical().id_salt("arbol_tipos").max_height(160.0).show(ui, |ui| {
                    egui::Grid::new("arbol_tipos_grid").num_columns(3).striped(true).spacing([24.0, 2.0]).show(ui, |ui| {
                        for (ext, (n, b)) in v {
                            ui.label(if ext.is_empty() { tr("(sin extensión)").to_string() } else { format!(".{ext}") });
                            ui.label(trf!("{} archivo(s)", n));
                            ui.label(human_size(*b));
                            ui.end_row();
                        }
                    });
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for p in ["a/b/c", "vacia"] {
            fs::create_dir_all(d.join(p)).unwrap();
        }
        fs::write(d.join("uno.txt"), "12345").unwrap();
        fs::write(d.join("a/foto.jpg"), "123").unwrap();
        fs::write(d.join("a/b/c/hondo.txt"), "1").unwrap();
        fs::write(d.join(".oculto"), "1").unwrap();
        d
    }

    #[test]
    fn tree_totals_depth_and_filter() {
        let d = sample();
        let c = Cfg { depth: 0, hidden: false, filter: None };
        let (n, s, _) = scan(&[d.clone()], &c, &AtomicBool::new(false));
        assert_eq!((s.files, s.dirs, s.bytes, s.deepest), (3, 4, 9, 4));
        assert_eq!(s.biggest.as_ref().unwrap().1, 5);
        let o = Show { format: Format::Tree, files: true, sizes: false, dates: false };
        let t = render(&n, o);
        assert!(t.contains("├── a/\n│   ├── b/\n│   │   └── c/\n│   │       └── hondo.txt\n│   └── foto.jpg\n├── vacia/\n└── uno.txt"), "{t}");
        // Two levels: the deeper folder is marked as having more inside.
        let (n, s, _) = scan(&[d.clone()], &Cfg { depth: 2, ..c.clone() }, &AtomicBool::new(false));
        assert_eq!(s.files, 2);
        assert!(render(&n, o).contains("…"));
        // A filter keeps only matching files and the folders that hold them.
        let (n, s, _) = scan(&[d.clone()], &Cfg { filter: crate::rename::name_matcher("*.jpg", false).unwrap(), ..c.clone() }, &AtomicBool::new(false));
        assert_eq!((s.files, s.dirs), (1, 1));
        assert_eq!(render(&n, Show { format: Format::Relative, ..o }), "a\na/foto.jpg".replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::remove_dir_all(&d).unwrap();
    }
}
