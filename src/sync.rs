//! Folder synchronisation: mirror, update, add-only, two-way and move modes; configurable
//! comparison (size, date, content); filters; an editable plan that can be reviewed before
//! anything changes; safe copies (temporary file, optional verification, kept old versions);
//! deletion to the trash, for good or to a backup folder; saved profiles and quick presets.

use crate::dupes::{favorite_folders, parse_size, quick_places, size_field, Browser};
use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

const MODES: [(&str, &str); 5] = [
    ("Espejo", "Copia lo nuevo y lo modificado y borra del destino lo que ya no está en el origen. Ideal para copias de seguridad."),
    ("Actualizar", "Copia lo nuevo y lo modificado. No borra nada."),
    ("Solo añadir", "Copia únicamente lo que no existe en el destino. Nunca sobrescribe."),
    ("Bidireccional", "Lo más reciente de cada lado pasa al otro y las dos carpetas quedan iguales. No propaga los borrados."),
    ("Mover", "Copia al destino y quita del origen lo que ya está copiado."),
];
const COMPARE: [&str; 4] = ["Inteligente: tamaño y fecha, y contenido si solo cambia la fecha", "Tamaño y fecha", "Solo tamaño", "Contenido completo (más lento)"];
const OVERWRITE: [&str; 3] = ["Solo si el origen es más reciente", "Siempre que sean distintos", "Nunca (se omiten)"];
const DELETE: [&str; 3] = ["A la papelera", "Definitivamente", "A la carpeta .sync-backup"];

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opts {
    src: String,
    dst: String,
    mode: u8,
    compare: u8,
    tolerance: u32,
    ignore_hour: bool,
    overwrite: u8,
    delete_method: u8,
    keep_old: bool,
    delete_first: bool,
    preserve_mtime: bool,
    verify: bool,
    empty_dirs: bool,
    stop_on_error: bool,
    include: String,
    exclude: String,
    pat_regex: bool,
    exclude_dirs: String,
    skip_hidden: bool,
    min_text: String,
    max_text: String,
    last_days: u32,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            src: String::new(),
            dst: String::new(),
            mode: 1,
            compare: 0,
            tolerance: 2,
            ignore_hour: false,
            overwrite: 0,
            delete_method: 0,
            keep_old: false,
            delete_first: false,
            preserve_mtime: true,
            verify: false,
            empty_dirs: true,
            stop_on_error: false,
            include: String::new(),
            exclude: String::new(),
            pat_regex: false,
            exclude_dirs: "$RECYCLE.BIN; System Volume Information; .Trash*; .Spotlight-V100; .fseventsd".into(),
            skip_hidden: false,
            min_text: String::new(),
            max_text: String::new(),
            last_days: 0,
        }
    }
}

type Preset = (&'static str, &'static str, fn(&mut Opts));

const PRESETS: [Preset; 7] = [
    ("💾 Copia de seguridad con versiones", "Espejo: el destino queda igual que el origen, y lo que se sobrescribe o borra se guarda en .sync-backup.", |o| {
        o.mode = 0;
        o.keep_old = true;
        o.delete_method = 2;
        o.overwrite = 1;
    }),
    ("🔄 Actualizar sin borrar", "Copia lo nuevo y lo modificado; no borra nada.", |o| {
        o.mode = 1;
        o.overwrite = 0;
    }),
    ("➕ Solo añadir lo nuevo", "Copia lo que falta y no toca nada de lo que ya existe.", |o| o.mode = 2),
    ("🔁 Mantener dos carpetas iguales", "Bidireccional: lo más reciente de cada lado pasa al otro.", |o| o.mode = 3),
    ("🔌 A un USB, disco FAT o NAS", "Actualizar tolerando las diferencias de fecha de esos sistemas (2 s y horario de verano).", |o| {
        o.mode = 1;
        o.tolerance = 2;
        o.ignore_hour = true;
        o.compare = 0;
    }),
    ("🔒 Copia verificada", "Actualizar y comprobar cada archivo copiado leyéndolo de nuevo.", |o| {
        o.mode = 1;
        o.verify = true;
        o.preserve_mtime = true;
    }),
    ("📦 Mover al destino", "Copia al destino y quita del origen (a la papelera).", |o| {
        o.mode = 4;
        o.delete_method = 0;
    }),
];

// ---------------- plan ----------------

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Copy,
    Update,
    Delete,
    MkDir,
    Conflict,
    Skip,
}

impl OpKind {
    fn group(self) -> usize {
        match self {
            OpKind::Copy | OpKind::MkDir => 0,
            OpKind::Update => 1,
            OpKind::Delete => 2,
            OpKind::Conflict => 3,
            OpKind::Skip => 4,
        }
    }
}

const GROUPS: [&str; 5] = ["Nuevos", "Actualizar", "Borrar", "Conflictos", "Omitidos"];

#[derive(Clone)]
struct Op {
    kind: OpKind,
    /// File to read (copies); None for deletions and folders.
    from: Option<PathBuf>,
    /// File written, or deleted.
    to: PathBuf,
    from_root: PathBuf,
    root: PathBuf,
    rel: String,
    size: u64,
    on: bool,
    why: &'static str,
    /// Direction: true = first folder » second folder.
    right: bool,
    detail: String,
    /// Only delete if the folder is empty by then (protects what the filters hide).
    empty_only: bool,
    /// Move mode: remove the source after the copy.
    remove_source: bool,
}

#[derive(Clone)]
struct Ent {
    path: PathBuf,
    rel: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

type Tree = BTreeMap<String, Ent>;

struct Walked {
    tree: Tree,
    /// Folders in which something was left out by a filter.
    skipped: HashSet<String>,
}

struct Plan {
    ops: Vec<Op>,
    src_dirs: Vec<PathBuf>,
    identical: usize,
    src_files: usize,
    dst_files: usize,
    new_dest: bool,
}

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    current: Mutex<String>,
    done: AtomicUsize,
    total: AtomicUsize,
    bytes: AtomicU64,
    bytes_total: AtomicU64,
}

#[derive(Default)]
struct Report {
    copied: usize,
    updated: usize,
    deleted: usize,
    dirs: usize,
    bytes: u64,
    errors: Vec<String>,
    log: Vec<String>,
    cancelled: bool,
    secs: f32,
}

enum Msg {
    Planned(Result<(Plan, Vec<String>), String>),
    Done(Report),
}

struct Job {
    rx: Receiver<Msg>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    executing: bool,
}

pub struct Syncer {
    o: Opts,
    plan: Option<Plan>,
    plan_sig: String,
    job: Option<Job>,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    log: Vec<String>,
    show: [bool; 5],
    filter: String,
    view_cache: crate::tools::ViewCache,
    confirm: bool,
    browser: Option<Browser>,
    profile_name: String,
    base: PathBuf,
}

// ---------------- helpers ----------------

fn key(rel: &str) -> String {
    if cfg!(windows) {
        rel.to_lowercase()
    } else {
        rel.to_string()
    }
}

struct Filt {
    include: Option<Regex>,
    exclude: Option<Regex>,
    exclude_dirs: Option<Regex>,
    skip_hidden: bool,
    min: u64,
    max: u64,
    last_secs: u64,
}

fn filt(o: &Opts) -> Result<Filt, String> {
    let m = |t: &str| crate::rename::name_matcher(t, o.pat_regex).map_err(|e| trf!("Expresión no válida: {}", e));
    Ok(Filt {
        include: m(&o.include)?,
        exclude: m(&o.exclude)?,
        exclude_dirs: crate::rename::name_matcher(&o.exclude_dirs, o.pat_regex).map_err(|e| trf!("Expresión no válida: {}", e))?,
        skip_hidden: o.skip_hidden,
        min: parse_size(&o.min_text).map_err(|e| format!("{}: {e}", tr("Tamaño mínimo")))?.unwrap_or(0),
        max: parse_size(&o.max_text).map_err(|e| format!("{}: {e}", tr("Tamaño máximo")))?.unwrap_or(u64::MAX),
        last_secs: o.last_days as u64 * 86_400,
    })
}

fn walk(root: &Path, f: &Filt, cancel: &AtomicBool, errs: &mut Vec<String>) -> Walked {
    let mut tree = Tree::new();
    let mut skipped = HashSet::new();
    if !root.is_dir() {
        return Walked { tree, skipped };
    }
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
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
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            if name == ".sync-backup" || name.ends_with(".sync-tmp") {
                continue;
            }
            let path = item.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if f.skip_hidden && name.starts_with('.') {
                skipped.insert(rel.clone());
                continue;
            }
            if meta.is_dir() {
                if f.exclude_dirs.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                    skipped.insert(rel.clone());
                    continue;
                }
                tree.insert(key(&r), Ent { path: path.clone(), rel: r.clone(), is_dir: true, size: 0, modified: meta.modified().ok() });
                stack.push((path, r));
            } else if meta.is_file() {
                let size = meta.len();
                let modified = meta.modified().ok();
                let old = f.last_secs > 0 && modified.and_then(|m| SystemTime::now().duration_since(m).ok()).map(|a| a.as_secs() > f.last_secs).unwrap_or(false);
                let out = size < f.min
                    || size > f.max
                    || old
                    || f.include.as_ref().map(|re| !re.is_match(&name)).unwrap_or(false)
                    || f.exclude.as_ref().map(|re| re.is_match(&name)).unwrap_or(false);
                if out {
                    skipped.insert(rel.clone());
                    continue;
                }
                tree.insert(key(&r), Ent { path, rel: r, is_dir: false, size, modified });
            }
        }
    }
    Walked { tree, skipped }
}

/// (whole seconds apart, whether `a` is later than `b`)
fn secs_between(a: SystemTime, b: SystemTime) -> (u64, bool) {
    match a.duration_since(b) {
        Ok(d) => (d.as_secs(), true),
        Err(e) => (e.duration().as_secs(), false),
    }
}

fn same_time(a: &Ent, b: &Ent, o: &Opts) -> bool {
    let (Some(x), Some(y)) = (a.modified, b.modified) else { return false };
    let d = secs_between(x, y).0;
    let tol = o.tolerance as u64;
    d <= tol || (o.ignore_hour && d.abs_diff(3600) <= tol)
}

/// `a` is newer than `b` beyond the tolerance.
fn newer(a: &Ent, b: &Ent, o: &Opts) -> bool {
    let (Some(x), Some(y)) = (a.modified, b.modified) else { return false };
    secs_between(x, y).1 && !same_time(a, b, o)
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> io::Result<[u8; 32]> {
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
    }
    Ok(*h.finalize().as_bytes())
}

fn same_content(a: &Path, b: &Path, cancel: &AtomicBool) -> io::Result<bool> {
    Ok(hash_file(a, cancel)? == hash_file(b, cancel)?)
}

/// Whether two files differ, by the chosen comparison.
fn differs(a: &Ent, b: &Ent, o: &Opts, cancel: &AtomicBool) -> io::Result<bool> {
    Ok(match o.compare {
        2 => a.size != b.size,
        1 => a.size != b.size || !same_time(a, b, o),
        3 => a.size != b.size || !same_content(&a.path, &b.path, cancel)?,
        _ => {
            if a.size != b.size {
                true
            } else if same_time(a, b, o) {
                false
            } else {
                !same_content(&a.path, &b.path, cancel)?
            }
        }
    })
}

fn is_empty_dir(p: &Path) -> bool {
    fs::read_dir(p).map(|mut r| r.next().is_none()).unwrap_or(false)
}

fn check_paths(o: &Opts) -> Result<(PathBuf, PathBuf), String> {
    let (a, b) = (o.src.trim(), o.dst.trim());
    if a.is_empty() || b.is_empty() {
        return Err(tr("Elige las dos carpetas.").to_string());
    }
    let (a, b) = (PathBuf::from(a), PathBuf::from(b));
    if !a.is_dir() {
        return Err(trf!("«{}» no es una carpeta", a.display()));
    }
    if b.exists() && !b.is_dir() {
        return Err(trf!("«{}» no es una carpeta", b.display()));
    }
    let (ca, cb) = (crate::fsops::canonical(&a), crate::fsops::canonical(&b));
    if ca == cb || ca.starts_with(&cb) || cb.starts_with(&ca) {
        return Err(tr("Las dos carpetas no pueden ser la misma ni estar una dentro de la otra.").to_string());
    }
    Ok((a, b))
}

fn backup_path(root: &Path, stamp: &str, rel: &str) -> PathBuf {
    root.join(".sync-backup").join(stamp).join(rel)
}

#[allow(clippy::too_many_arguments)]
fn mk_op(kind: OpKind, from: &Ent, to: PathBuf, from_root: &Path, root: &Path, why: &'static str, right: bool, detail: String) -> Op {
    Op {
        kind,
        from: Some(from.path.clone()),
        to,
        from_root: from_root.to_path_buf(),
        root: root.to_path_buf(),
        rel: from.rel.clone(),
        size: from.size,
        on: !matches!(kind, OpKind::Skip | OpKind::Conflict),
        why,
        right,
        detail,
        empty_only: false,
        remove_source: false,
    }
}

fn detail_of(a: &Ent, b: Option<&Ent>) -> String {
    let side = |e: &Ent| format!("{} · {}", human_size(e.size), fsops::format_time(e.modified));
    match b {
        Some(b) => format!("{}: {}\n{}: {}", tr("Origen"), side(a), tr("Destino"), side(b)),
        None => side(a),
    }
}

fn under(k: &str, d: &str) -> bool {
    k.len() > d.len() && k.starts_with(d) && k.as_bytes()[d.len()] == b'/'
}

fn build_plan(o: &Opts, prog: &Progress, cancel: &AtomicBool) -> Result<(Plan, Vec<String>), String> {
    let (src, dst) = check_paths(o)?;
    let f = filt(o)?;
    let set = |s: &str| {
        if let Ok(mut st) = prog.stage.lock() {
            *st = s.to_string();
        }
    };
    let mut errs = vec![];
    set(tr("Recorriendo la primera carpeta"));
    let wa = walk(&src, &f, cancel, &mut errs);
    set(tr("Recorriendo la segunda carpeta"));
    let wb = walk(&dst, &f, cancel, &mut errs);
    let (ta, tb) = (&wa.tree, &wb.tree);
    let src_files = ta.values().filter(|e| !e.is_dir).count();
    let dst_files = tb.values().filter(|e| !e.is_dir).count();
    let common = ta.iter().filter(|(k, e)| !e.is_dir && tb.get(*k).map(|b| !b.is_dir).unwrap_or(false)).count();
    prog.total.store(common, Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    set(tr("Comparando archivos"));

    let mut ops: Vec<Op> = vec![];
    let mut identical = 0usize;
    let mode = o.mode;
    let compare = |a: &Ent, b: &Ent, errs: &mut Vec<String>| -> Option<bool> {
        prog.done.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut c) = prog.current.lock() {
            *c = a.rel.clone();
        }
        match differs(a, b, o, cancel) {
            Ok(d) => Some(d),
            Err(e) => {
                errs.push(format!("{}: {e}", a.rel));
                None
            }
        }
    };
    let conflict_op = |a: &Ent, b: &Ent, why: &'static str| {
        let mut op = mk_op(OpKind::Conflict, a, b.path.clone(), &src, &dst, why, true, detail_of(a, Some(b)));
        op.on = false;
        op
    };
    let empty_dir_op = |e: &Ent, to: PathBuf, from_root: &Path, root: &Path, right: bool| {
        let mut op = mk_op(OpKind::MkDir, e, to, from_root, root, "carpeta vacía", right, String::new());
        op.from = None;
        op
    };

    if mode == 3 {
        let keys: std::collections::BTreeSet<&String> = ta.keys().chain(tb.keys()).collect();
        for k in keys {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match (ta.get(k), tb.get(k)) {
                (Some(a), None) => {
                    let to = dst.join(&a.rel);
                    if a.is_dir {
                        if o.empty_dirs && is_empty_dir(&a.path) {
                            ops.push(empty_dir_op(a, to, &src, &dst, true));
                        }
                    } else {
                        ops.push(mk_op(OpKind::Copy, a, to, &src, &dst, "nuevo", true, detail_of(a, None)));
                    }
                }
                (None, Some(b)) => {
                    let to = src.join(&b.rel);
                    if b.is_dir {
                        if o.empty_dirs && is_empty_dir(&b.path) {
                            ops.push(empty_dir_op(b, to, &dst, &src, false));
                        }
                    } else {
                        ops.push(mk_op(OpKind::Copy, b, to, &dst, &src, "nuevo", false, detail_of(b, None)));
                    }
                }
                (Some(a), Some(b)) => {
                    if a.is_dir && b.is_dir {
                        continue;
                    }
                    if a.is_dir != b.is_dir {
                        ops.push(conflict_op(a, b, "uno es una carpeta y el otro un archivo"));
                        continue;
                    }
                    let Some(d) = compare(a, b, &mut errs) else { continue };
                    if !d {
                        identical += 1;
                    } else if newer(a, b, o) {
                        ops.push(mk_op(OpKind::Update, a, b.path.clone(), &src, &dst, "más reciente en la primera carpeta", true, detail_of(a, Some(b))));
                    } else if newer(b, a, o) {
                        ops.push(mk_op(OpKind::Update, b, a.path.clone(), &dst, &src, "más reciente en la segunda carpeta", false, detail_of(a, Some(b))));
                    } else {
                        ops.push(conflict_op(a, b, "distintos con la misma fecha: elige cuál conservar"));
                    }
                }
                (None, None) => {}
            }
        }
    } else {
        for (k, a) in ta {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let to = dst.join(&a.rel);
            match tb.get(k) {
                None => {
                    if a.is_dir {
                        if o.empty_dirs && is_empty_dir(&a.path) {
                            ops.push(empty_dir_op(a, to, &src, &dst, true));
                        }
                    } else {
                        let mut op = mk_op(OpKind::Copy, a, to, &src, &dst, "nuevo", true, detail_of(a, None));
                        op.remove_source = mode == 4;
                        ops.push(op);
                    }
                }
                Some(b) if a.is_dir && b.is_dir => {}
                Some(b) if a.is_dir != b.is_dir => ops.push(conflict_op(a, b, "uno es una carpeta y el otro un archivo")),
                Some(b) => {
                    let Some(d) = compare(a, b, &mut errs) else { continue };
                    if !d {
                        identical += 1;
                        if mode == 4 {
                            let mut op = mk_op(OpKind::Delete, a, a.path.clone(), &src, &src, "ya está en el destino: se quita del origen", true, detail_of(a, Some(b)));
                            op.from = None;
                            ops.push(op);
                        }
                        continue;
                    }
                    let detail = detail_of(a, Some(b));
                    let go: Result<&'static str, &'static str> = match (mode, o.overwrite) {
                        (2, _) | (_, 2) => Err("ya existe y es distinto: no se sobrescribe"),
                        (_, 0) if !newer(a, b, o) => Err("el destino es igual de reciente o más: no se sobrescribe"),
                        (_, 0) => Ok("más reciente en el origen"),
                        _ => Ok("distinto"),
                    };
                    match go {
                        Ok(why) => {
                            let mut op = mk_op(OpKind::Update, a, b.path.clone(), &src, &dst, why, true, detail);
                            op.remove_source = mode == 4;
                            ops.push(op);
                        }
                        Err(why) => ops.push(mk_op(OpKind::Skip, a, b.path.clone(), &src, &dst, why, true, detail)),
                    }
                }
            }
        }
        if mode == 0 && !cancel.load(Ordering::Relaxed) {
            // What exists only in the destination goes; a whole folder in one step when no filter hides anything in it.
            let extra: Vec<&String> = tb.keys().filter(|k| !ta.contains_key(*k)).collect();
            let extra_dirs: Vec<&String> = extra.iter().copied().filter(|k| tb[*k].is_dir).collect();
            let protected = |k: &str| wb.skipped.iter().any(|s| key(s) == k || under(&key(s), k));
            for k in &extra {
                let b = &tb[*k];
                if let Some(top) = extra_dirs.iter().find(|d| under(k, d)) {
                    if !protected(top) {
                        continue; // covered by its folder
                    }
                }
                let mut op = mk_op(OpKind::Delete, b, b.path.clone(), &dst, &dst, "ya no está en el origen", false, detail_of(b, None));
                op.from = None;
                if b.is_dir {
                    let (n, bytes) = tb.range((*k).clone()..).take_while(|(kk, _)| *kk == *k || under(kk, k)).fold((0usize, 0u64), |(n, s), (_, e)| (n + usize::from(!e.is_dir), s + e.size));
                    op.size = bytes;
                    op.empty_only = protected(k);
                    op.detail = trf!("Carpeta con {} archivo(s) · {}", n, human_size(bytes));
                }
                ops.push(op);
            }
        }
    }
    ops.sort_by_key(|a| a.rel.to_lowercase());
    let src_dirs: Vec<PathBuf> = ta.values().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
    Ok((Plan { ops, src_dirs, identical, src_files, dst_files, new_dest: !dst.exists() }, errs))
}

// ---------------- execution ----------------

enum Outcome {
    Copied,
    Updated,
    Deleted,
    Dir,
    Skipped,
}

fn move_aside(path: &Path, to: &Path) -> io::Result<()> {
    if let Some(p) = to.parent() {
        fs::create_dir_all(p)?;
    }
    fsops::move_path(path, to)
}

fn remove_by(method: u8, path: &Path, backup: PathBuf) -> Result<(), String> {
    match method {
        0 => trash::delete(path).map_err(|e| e.to_string()),
        1 => fsops::remove_permanently(path).map_err(|e| e.to_string()),
        _ => move_aside(path, &backup).map_err(|e| e.to_string()),
    }
}

fn copy_file(op: &Op, o: &Opts, stamp: &str, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    let from = op.from.as_ref().ok_or("sin origen")?;
    let to = &op.to;
    if let Some(p) = to.parent() {
        fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let name = to.file_name().unwrap_or_default().to_string_lossy().to_string();
    let tmp = to.with_file_name(format!(".{name}.sync-tmp"));
    let meta = fs::metadata(from).map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        let mut input = fs::File::open(from).map_err(|e| e.to_string())?;
        let mut out = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(tr("cancelado").to_string());
            }
            let n = input.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            prog.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        if o.preserve_mtime {
            if let Ok(m) = meta.modified() {
                let _ = out.set_modified(m);
            }
        }
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        let _ = fs::set_permissions(&tmp, meta.permissions());
        if o.verify && !same_content(from, &tmp, cancel).map_err(|e| e.to_string())? {
            return Err(tr("la copia no coincide con el original").to_string());
        }
        if to.exists() {
            if o.keep_old {
                move_aside(to, &backup_path(&op.root, stamp, &op.rel)).map_err(|e| e.to_string())?;
            } else if let Ok(m) = fs::metadata(to) {
                let mut p = m.permissions();
                if p.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    let _ = fs::set_permissions(to, p);
                }
            }
        }
        fs::rename(&tmp, to).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn run_op(op: &Op, o: &Opts, stamp: &str, prog: &Progress, cancel: &AtomicBool) -> Result<Outcome, String> {
    match op.kind {
        OpKind::MkDir => fs::create_dir_all(&op.to).map(|_| Outcome::Dir).map_err(|e| e.to_string()),
        OpKind::Copy | OpKind::Update => {
            copy_file(op, o, stamp, prog, cancel)?;
            if op.remove_source {
                if let Some(from) = &op.from {
                    remove_by(o.delete_method, from, backup_path(&op.from_root, stamp, &op.rel)).map_err(|e| format!("{} ({})", tr("copiado, pero no se pudo quitar del origen"), e))?;
                }
            }
            Ok(if op.kind == OpKind::Copy { Outcome::Copied } else { Outcome::Updated })
        }
        OpKind::Delete => {
            if (op.empty_only && !is_empty_dir(&op.to)) || !op.to.exists() {
                return Ok(Outcome::Skipped);
            }
            remove_by(o.delete_method, &op.to, backup_path(&op.root, stamp, &op.rel))?;
            Ok(Outcome::Deleted)
        }
        _ => Ok(Outcome::Skipped),
    }
}

fn execute(ops: Vec<Op>, o: &Opts, src_dirs: Vec<PathBuf>, prog: &Progress, cancel: &AtomicBool) -> Report {
    let start = Instant::now();
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let mut rep = Report::default();
    let rank = |op: &Op| match op.kind {
        OpKind::Delete => {
            if o.delete_first {
                0
            } else {
                3
            }
        }
        OpKind::MkDir => 1,
        _ => 2,
    };
    let mut order: Vec<usize> = (0..ops.len()).filter(|&i| ops[i].on && !matches!(ops[i].kind, OpKind::Skip | OpKind::Conflict)).collect();
    // Deletions go deepest first so folders are emptied before they are removed.
    order.sort_by_key(|&i| (rank(&ops[i]), if ops[i].kind == OpKind::Delete { usize::MAX - ops[i].rel.len() } else { 0 }, i));
    prog.total.store(order.len(), Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    prog.bytes.store(0, Ordering::Relaxed);
    prog.bytes_total.store(order.iter().filter(|&&i| matches!(ops[i].kind, OpKind::Copy | OpKind::Update)).map(|&i| ops[i].size).sum(), Ordering::Relaxed);
    if let Ok(mut s) = prog.stage.lock() {
        *s = tr("Sincronizando").to_string();
    }
    for i in order {
        if cancel.load(Ordering::Relaxed) {
            rep.cancelled = true;
            break;
        }
        let op = &ops[i];
        if let Ok(mut c) = prog.current.lock() {
            *c = op.rel.clone();
        }
        match run_op(op, o, &stamp, prog, cancel) {
            Ok(out) => {
                let tag = match out {
                    Outcome::Copied => {
                        rep.copied += 1;
                        rep.bytes += op.size;
                        "COPIADO"
                    }
                    Outcome::Updated => {
                        rep.updated += 1;
                        rep.bytes += op.size;
                        "ACTUALIZADO"
                    }
                    Outcome::Deleted => {
                        rep.deleted += 1;
                        "BORRADO"
                    }
                    Outcome::Dir => {
                        rep.dirs += 1;
                        "CARPETA"
                    }
                    Outcome::Skipped => "OMITIDO",
                };
                rep.log.push(format!("{tag}\t{}", op.rel));
            }
            Err(e) => {
                rep.log.push(format!("ERROR\t{}\t{e}", op.rel));
                rep.errors.push(format!("{}: {e}", op.rel));
                if o.stop_on_error {
                    break;
                }
            }
        }
        prog.done.fetch_add(1, Ordering::Relaxed);
    }
    if o.mode == 4 && !rep.cancelled {
        let mut dirs = src_dirs;
        dirs.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
        for d in dirs {
            let _ = fs::remove_dir(&d);
        }
    }
    rep.secs = start.elapsed().as_secs_f32();
    rep
}

// ---------------- the tool ----------------

#[derive(Default)]
struct Stats {
    copy: usize,
    update: usize,
    delete: usize,
    mkdir: usize,
    conflict: usize,
    bytes: u64,
    delete_bytes: u64,
}

impl Stats {
    fn total_on(&self) -> usize {
        self.copy + self.update + self.delete + self.mkdir
    }
}

fn stats(ops: &[Op]) -> Stats {
    let mut s = Stats::default();
    for op in ops {
        match op.kind {
            OpKind::Conflict => s.conflict += 1,
            OpKind::Skip => {}
            _ if !op.on => {}
            OpKind::Copy => {
                s.copy += 1;
                s.bytes += op.size;
            }
            OpKind::Update => {
                s.update += 1;
                s.bytes += op.size;
            }
            OpKind::Delete => {
                s.delete += 1;
                s.delete_bytes += op.size;
            }
            OpKind::MkDir => s.mkdir += 1,
        }
    }
    s
}

/// Turns a planned conflict (first » second) into an update in the chosen direction.
fn resolve_conflict(op: &mut Op, keep_first: bool) {
    if !keep_first {
        if let Some(old_from) = op.from.take() {
            let old_to = std::mem::replace(&mut op.to, old_from);
            op.from = Some(old_to);
            std::mem::swap(&mut op.from_root, &mut op.root);
            op.right = false;
        }
    }
    op.kind = OpKind::Update;
    op.why = "elegido por ti";
    op.on = true;
}

impl Syncer {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Syncer {
        let mut o = Opts::default();
        let dirs: Vec<&PathBuf> = targets.iter().filter(|p| p.is_dir()).collect();
        if dirs.len() == 1 {
            o.src = dirs[0].to_string_lossy().to_string();
        }
        Syncer {
            o,
            plan: None,
            plan_sig: String::new(),
            job: None,
            status: None,
            problems: vec![],
            log: vec![],
            show: [true; 5],
            filter: String::new(),
            view_cache: Default::default(),
            confirm: false,
            browser: None,
            profile_name: String::new(),
            base,
        }
    }

    /// Fills both folders and compares at once, for trying the tool (`--sync`).
    pub fn demo(&mut self, a: &Path, b: &Path) {
        self.o.src = a.to_string_lossy().to_string();
        self.o.dst = b.to_string_lossy().to_string();
        self.o.mode = 0;
        self.start_compare();
    }

    fn sig(&self) -> String {
        serde_json::to_string(&self.o).unwrap_or_default()
    }

    fn start_compare(&mut self) {
        if let Err(e) = check_paths(&self.o).and_then(|_| filt(&self.o).map(|_| ())) {
            self.status = Some((e, true));
            return;
        }
        let o = self.o.clone();
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Planned(build_plan(&o, &p2, &c2)));
        });
        self.plan = None;
        self.status = None;
        self.problems.clear();
        self.log.clear();
        self.confirm = false;
        self.plan_sig = self.sig();
        self.job = Some(Job { rx, progress, cancel, started: Instant::now(), executing: false });
    }

    fn start_sync(&mut self) {
        let Some(plan) = self.plan.take() else { return };
        let o = self.o.clone();
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        let Plan { ops, src_dirs, .. } = plan;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Done(execute(ops, &o, src_dirs, &p2, &c2)));
        });
        self.status = None;
        self.job = Some(Job { rx, progress, cancel, started: Instant::now(), executing: true });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok(msg) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        match msg {
            Msg::Planned(Ok((plan, errs))) => {
                self.problems = errs;
                if cancelled {
                    self.status = Some((tr("Comparación cancelada").to_string(), true));
                } else {
                    let pending = plan.ops.iter().filter(|o| !matches!(o.kind, OpKind::Skip)).count();
                    self.status = Some(if pending == 0 {
                        (tr("Las dos carpetas ya están sincronizadas.").to_string(), false)
                    } else {
                        (tr("Comparación terminada: revisa el plan y pulsa «Sincronizar ahora».").to_string(), false)
                    });
                    self.plan = Some(plan);
                }
            }
            Msg::Planned(Err(e)) => self.status = Some((e, true)),
            Msg::Done(rep) => {
                let head = if rep.cancelled { tr("Sincronización cancelada") } else { tr("Sincronización terminada") };
                let msg = trf!("{}: {} copiados, {} actualizados, {} borrados, {} carpetas · {} en {} s", head, rep.copied, rep.updated, rep.deleted, rep.dirs, human_size(rep.bytes), format!("{:.1}", rep.secs));
                self.status = Some((msg, !rep.errors.is_empty() || rep.cancelled));
                self.problems = rep.errors;
                self.log = rep.log;
                events.push(ToolEvent::Reload);
            }
        }
    }

    fn save_log(&mut self, events: &mut Vec<ToolEvent>) {
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
        let file = fsops::unique_name(&self.base, &format!("sincronizacion-{stamp}.log"));
        let mut text = format!("{} » {}\n", self.o.src, self.o.dst);
        text.push_str(&self.log.join("\n"));
        match fs::write(&file, text) {
            Ok(()) => {
                self.status = Some((trf!("Guardado en {}", file.display()), false));
                events.push(ToolEvent::Reload);
            }
            Err(e) => self.status = Some((trf!("No se pudo guardar: {}", e), true)),
        }
    }

    // ---- profiles

    fn profiles_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("sincronizacion"))
    }

    fn profile_list() -> Vec<String> {
        let mut v: Vec<String> = Self::profiles_dir()
            .and_then(|d| fs::read_dir(d).ok())
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_string)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    fn save_profile(&mut self) {
        let name = self.profile_name.trim().replace(['/', '\\'], "-");
        let Some(dir) = Self::profiles_dir() else { return };
        let r = fs::create_dir_all(&dir).and_then(|_| fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&self.o).unwrap_or_default()));
        self.status = Some(match r {
            Ok(()) => (trf!("Perfil «{}» guardado", name), false),
            Err(e) => (trf!("No se pudo guardar: {}", e), true),
        });
    }

    fn load_profile(&mut self, name: &str) {
        let Some(dir) = Self::profiles_dir() else { return };
        match fs::read_to_string(dir.join(format!("{name}.json"))).ok().and_then(|s| serde_json::from_str::<Opts>(&s).ok()) {
            Some(o) => {
                self.o = o;
                self.profile_name = name.to_string();
                self.plan = None;
                self.status = Some((trf!("Perfil «{}» cargado", name), false));
            }
            None => self.status = Some((tr("No se pudo leer el perfil").to_string(), true)),
        }
    }

    // ---- UI

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).filter(|p| p.is_dir()).collect());
        for p in dropped {
            if self.o.src.trim().is_empty() {
                self.o.src = p.to_string_lossy().to_string();
            } else {
                self.o.dst = p.to_string_lossy().to_string();
            }
        }
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("sync_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.paths_ui(ui, selection, cwd);
                ui.separator();
                self.mode_ui(ui);
            });
            ui.separator();
            self.actions_ui(ui, events);
            ui.add_enabled_ui(!busy, |ui| self.options_ui(ui));
            self.plan_ui(ui);
            if !self.problems.is_empty() {
                ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                    egui::ScrollArea::vertical().max_height(90.0).id_salt("sync_err").show(ui, |ui| {
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

    fn paths_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        let two_way = self.o.mode == 3;
        let mut browse: Option<u8> = None;
        let mut picked: Option<(u8, PathBuf)> = None;
        for which in 0..2u8 {
            if which == 1 {
                ui.horizontal(|ui| {
                    ui.add_space(70.0);
                    if ui.small_button(tr("⇅ Intercambiar")).on_hover_text(tr("Intercambia origen y destino")).clicked() {
                        std::mem::swap(&mut self.o.src, &mut self.o.dst);
                    }
                });
            }
            let label = match (which, two_way) {
                (0, false) => tr("Origen"),
                (1, false) => tr("Destino"),
                (0, true) => tr("Carpeta A"),
                _ => tr("Carpeta B"),
            };
            ui.horizontal(|ui| {
                ui.add_sized([70.0, 20.0], egui::Label::new(RichText::new(label).strong()));
                let text = if which == 0 { &mut self.o.src } else { &mut self.o.dst };
                ui.add(egui::TextEdit::singleline(text).desired_width((ui.available_width() - 300.0).max(160.0)).hint_text(tr("elige una carpeta o pega una ruta")));
                if ui.button(tr("📂 Examinar…")).clicked() {
                    browse = Some(which);
                }
                if let Some(p) = quick_menu(ui, cwd, selection) {
                    picked = Some((which, p));
                }
                let t = PathBuf::from(text.trim());
                if text.trim().is_empty() {
                } else if t.is_dir() {
                    ui.label(RichText::new("✔").color(theme::p().ok));
                } else if which == 1 && !t.exists() {
                    ui.label(RichText::new(tr("se creará")).small().color(theme::p().sub));
                } else {
                    ui.label(RichText::new(tr("no existe")).small().color(theme::p().danger));
                }
            });
        }
        if let Some((w, p)) = picked {
            let s = p.to_string_lossy().to_string();
            if w == 0 {
                self.o.src = s;
            } else {
                self.o.dst = s;
            }
        }
        if let Some(w) = browse {
            let cur = PathBuf::from(if w == 0 { self.o.src.trim() } else { self.o.dst.trim() });
            let start = if cur.is_dir() { cur } else { dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf()) };
            self.browser = Some(Browser::new(start, w));
        }
    }

    fn mode_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("MODO")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for (k, (name, desc)) in MODES.iter().enumerate() {
                ui.radio_value(&mut self.o.mode, k as u8, tr(name)).on_hover_text(tr(desc));
            }
        });
        ui.label(RichText::new(tr(MODES[self.o.mode as usize].1)).small().color(theme::p().sub));
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let mode = self.o.mode;
        egui::CollapsingHeader::new(tr("⚙ Comparación y cambios")).default_open(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new(tr("Considerar distintos por")).small().strong().color(theme::p().sub));
                    for (k, l) in COMPARE.iter().enumerate() {
                        ui.radio_value(&mut self.o.compare, k as u8, tr(l));
                    }
                    ui.horizontal(|ui| {
                        ui.label(tr("Tolerancia de fecha"));
                        ui.add(egui::DragValue::new(&mut self.o.tolerance).range(0..=7200).suffix(" s"))
                            .on_hover_text(tr("Dos fechas más cercanas que esto se consideran la misma. FAT y algunos NAS redondean a 2 segundos."));
                    });
                    ui.checkbox(&mut self.o.ignore_hour, tr("Ignorar diferencias de exactamente 1 hora"))
                        .on_hover_text(tr("Evita falsos cambios por el horario de verano o por discos con otra zona horaria."));
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    if matches!(mode, 0 | 1 | 4) {
                        ui.label(RichText::new(tr("Si el archivo ya existe y es distinto")).small().strong().color(theme::p().sub));
                        for (k, l) in OVERWRITE.iter().enumerate() {
                            ui.radio_value(&mut self.o.overwrite, k as u8, tr(l));
                        }
                    }
                    if matches!(mode, 0 | 1 | 3 | 4) {
                        ui.checkbox(&mut self.o.keep_old, tr("Guardar la versión anterior al sobrescribir"))
                            .on_hover_text(tr("La versión que se reemplaza se mueve a .sync-backup dentro de esa carpeta, con la fecha y hora."));
                    }
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    if matches!(mode, 0 | 4) {
                        ui.label(RichText::new(tr(if mode == 4 { "Al quitar del origen" } else { "Al borrar del destino" })).small().strong().color(theme::p().sub));
                        for (k, l) in DELETE.iter().enumerate() {
                            ui.radio_value(&mut self.o.delete_method, k as u8, tr(l));
                        }
                    }
                    if mode == 0 {
                        ui.checkbox(&mut self.o.delete_first, tr("Borrar antes de copiar")).on_hover_text(tr("Libera espacio en el destino antes de copiar. Es más arriesgado si algo falla a mitad."));
                    }
                });
            });
        });
        egui::CollapsingHeader::new(tr("🔎 Filtros")).default_open(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(400.0);
                    let hint_inc = if self.o.pat_regex { "\\.(jpe?g|png)$" } else { "*.jpg; *.png" };
                    ui.horizontal(|ui| {
                        ui.add_sized([100.0, 18.0], egui::Label::new(tr("Incluir")));
                        ui.add(egui::TextEdit::singleline(&mut self.o.include).hint_text(hint_inc).desired_width(260.0));
                    });
                    ui.horizontal(|ui| {
                        ui.add_sized([100.0, 18.0], egui::Label::new(tr("Excluir")));
                        ui.add(egui::TextEdit::singleline(&mut self.o.exclude).hint_text(if self.o.pat_regex { "\\.tmp$" } else { "*.tmp; thumbs.db" }).desired_width(260.0));
                    });
                    ui.checkbox(&mut self.o.pat_regex, tr("Los filtros son expresiones regulares"));
                    ui.horizontal(|ui| {
                        ui.add_sized([100.0, 18.0], egui::Label::new(tr("Saltar carpetas")));
                        ui.add(egui::TextEdit::singleline(&mut self.o.exclude_dirs).desired_width(260.0)).on_hover_text(tr("Nombres de carpetas que no se recorren, con comodines, separados por «;»"));
                    });
                    ui.label(RichText::new(tr("Lo que un filtro deja fuera no se copia ni se borra en ninguna de las dos carpetas.")).small().color(theme::p().muted));
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    size_field(ui, tr("Tamaño mínimo"), &mut self.o.min_text, tr("sin límite"));
                    size_field(ui, tr("Tamaño máximo"), &mut self.o.max_text, tr("sin límite"));
                    ui.horizontal(|ui| {
                        ui.add_sized([100.0, 18.0], egui::Label::new(tr("Modificados en")));
                        ui.add(egui::DragValue::new(&mut self.o.last_days).range(0..=36500).suffix(tr(" días"))).on_hover_text(tr("Solo archivos modificados en los últimos días indicados (0 = todos)"));
                    });
                    ui.checkbox(&mut self.o.skip_hidden, tr("Saltar archivos y carpetas ocultos"));
                });
            });
        });
        egui::CollapsingHeader::new(tr("🛠 Opciones avanzadas")).default_open(false).show(ui, |ui| {
            ui.checkbox(&mut self.o.preserve_mtime, tr("Conservar la fecha de modificación al copiar")).on_hover_text(tr("Recomendado: así la siguiente comparación no ve cambios donde no los hay."));
            ui.checkbox(&mut self.o.verify, tr("Verificar cada copia leyéndola de nuevo")).on_hover_text(tr("Más lento, pero detecta copias dañadas."));
            ui.checkbox(&mut self.o.empty_dirs, tr("Copiar también las carpetas vacías"));
            ui.checkbox(&mut self.o.stop_on_error, tr("Parar al primer error"));
            ui.label(RichText::new(tr("Los enlaces simbólicos se ignoran. Los archivos se copian a un temporal y se renombran al terminar, así nunca queda uno a medias.")).small().color(theme::p().muted));
        });
    }

    fn actions_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let busy = self.job.is_some();
        let sig = self.sig();
        let stale = self.plan.is_some() && sig != self.plan_sig;
        let (st, src_empty) = self.plan.as_ref().map(|p| (stats(&p.ops), p.src_files == 0)).unwrap_or_default();
        let needs_confirm = st.delete > 0 && (self.o.delete_method == 1 || src_empty);
        let can_sync = !busy && self.plan.is_some() && !stale && st.total_on() > 0 && (!needs_confirm || self.confirm);
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(tr("🔍 Comparar")).strong())).on_hover_text(tr("Revisa las dos carpetas y muestra qué se haría, sin tocar nada.")).clicked() {
                self.start_compare();
            }
            let btn = egui::Button::new(RichText::new(tr("🔄 Sincronizar ahora")).color(Color32::WHITE).strong()).fill(if st.delete > 0 { theme::p().danger } else { theme::accent() });
            if ui.add_enabled(can_sync, btn).clicked() {
                self.start_sync();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
                let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                let (bytes, btotal) = (p.bytes.load(Ordering::Relaxed), p.bytes_total.load(Ordering::Relaxed));
                let frac = if job.executing && btotal > 0 {
                    bytes as f32 / btotal as f32
                } else if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(frac.min(1.0)).desired_width(180.0).show_percentage());
                let secs = job.started.elapsed().as_secs_f64().max(0.1);
                let speed = if job.executing { format!(" · {}/s", human_size((bytes as f64 / secs) as u64)) } else { String::new() };
                ui.label(RichText::new(format!("{stage} · {done}/{total}{speed}")).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                if let Ok(c) = p.current.lock() {
                    ui.label(RichText::new(c.as_str()).small().color(theme::p().muted));
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                ui.menu_button(tr("✨ Rápidos"), |ui| {
                    ui.label(RichText::new(tr("Cambian el modo y las opciones; las carpetas se mantienen")).small().color(theme::p().muted));
                    for (name, desc, f) in PRESETS {
                        if ui.button(tr(name)).on_hover_text(tr(desc)).clicked() {
                            f(&mut self.o);
                            ui.close_menu();
                        }
                    }
                });
                ui.menu_button(tr("💾 Perfiles"), |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.profile_name).hint_text(tr("nombre del perfil")).desired_width(150.0));
                        if ui.add_enabled(!self.profile_name.trim().is_empty(), egui::Button::new(tr("Guardar"))).clicked() {
                            self.save_profile();
                        }
                    });
                    ui.label(RichText::new(tr("Guarda carpetas, modo y opciones.")).small().color(theme::p().muted));
                    let list = Self::profile_list();
                    if list.is_empty() {
                        ui.label(RichText::new(tr("Aún no hay perfiles guardados.")).small().color(theme::p().muted));
                    }
                    for name in list {
                        ui.horizontal(|ui| {
                            if ui.link(&name).on_hover_text(tr("Cargar este perfil")).clicked() {
                                self.load_profile(&name);
                                ui.close_menu();
                            }
                            if ui.small_button("🗑").on_hover_text(tr("Borrar perfil")).clicked() {
                                if let Some(d) = Self::profiles_dir() {
                                    let _ = fs::remove_file(d.join(format!("{name}.json")));
                                }
                            }
                        });
                    }
                });
            }
        });
        if stale {
            ui.label(RichText::new(tr("Has cambiado las opciones desde la última comparación: vuelve a comparar.")).small().color(theme::p().danger));
        }
        if needs_confirm && !busy && self.plan.is_some() {
            let text = if src_empty { tr("El origen está vacío: se borraría todo lo del destino. Entiendo y quiero continuar.") } else { tr("Entiendo que los borrados son definitivos.") };
            ui.checkbox(&mut self.confirm, RichText::new(text).color(theme::p().danger));
        }
        if let Some((s, err)) = &self.status {
            ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
        }
        if !self.log.is_empty() && !busy && ui.button(tr("💾 Guardar registro")).clicked() {
            self.save_log(events);
        }
    }

    fn plan_ui(&mut self, ui: &mut egui::Ui) {
        let Some(mut plan) = self.plan.take() else { return };
        let st = stats(&plan.ops);
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("{} archivos en la primera · {} en la segunda · {} iguales", plan.src_files, plan.dst_files, plan.identical)).small().color(theme::p().sub));
            if plan.new_dest {
                ui.label(RichText::new(tr("El destino no existe: se creará.")).small().color(theme::p().sub));
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("A hacer: {} nuevos · {} actualizar · {} borrar · {} carpetas · {} para copiar", st.copy, st.update, st.delete, st.mkdir, human_size(st.bytes))).strong());
            if st.delete_bytes > 0 {
                ui.label(RichText::new(trf!("({} a borrar)", human_size(st.delete_bytes))).small().color(theme::p().danger));
            }
            if st.conflict > 0 {
                ui.label(RichText::new(trf!("⚠ {} conflicto(s) sin resolver", st.conflict)).color(theme::p().danger));
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (k, g) in GROUPS.iter().enumerate() {
                ui.toggle_value(&mut self.show[k], tr(g));
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar")).desired_width(140.0));
            if ui.small_button(tr("Marcar todo")).clicked() {
                plan.ops.iter_mut().filter(|o| !matches!(o.kind, OpKind::Skip | OpKind::Conflict)).for_each(|o| o.on = true);
            }
            if ui.small_button(tr("Desmarcar todo")).clicked() {
                plan.ops.iter_mut().for_each(|o| o.on = false);
            }
            if st.delete > 0 && ui.small_button(tr("Desmarcar los borrados")).on_hover_text(tr("Sincroniza sin borrar nada")).clicked() {
                plan.ops.iter_mut().filter(|o| o.kind == OpKind::Delete).for_each(|o| o.on = false);
            }
        });
        let q = self.filter.to_lowercase();
        let mask: u64 = self.show.iter().enumerate().map(|(k, b)| (*b as u64) << k).sum();
        let salt = ((plan.ops.len() as u64) << 24) ^ ((plan.ops.as_ptr() as u64) << 8) ^ mask ^ ((st.conflict as u64) << 40);
        let view = self.view_cache.get(&self.filter, salt, || {
            plan.ops.iter().enumerate().filter(|(_, op)| self.show[op.kind.group()] && (q.is_empty() || op.rel.to_lowercase().contains(&q))).map(|(i, _)| i).collect()
        });
        let row_h = 22.0;
        let mut toggles: Vec<usize> = vec![];
        let mut resolve: Vec<(usize, bool)> = vec![];
        let two_way = self.o.mode == 3;
        egui::ScrollArea::vertical().id_salt("sync_plan").max_height(crate::tools::grow(ui, 260.0)).auto_shrink([false, false]).show_rows(ui, row_h, view.len(), |ui, range| {
            for &i in &view[range] {
                let op = &plan.ops[i];
                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                if i % 2 == 1 {
                    ui.painter().rect_filled(rect, 0.0, theme::p().stripe);
                }
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                let mut on = op.on;
                let selectable = !matches!(op.kind, OpKind::Skip | OpKind::Conflict);
                if child.add_enabled(selectable, egui::Checkbox::new(&mut on, "")).changed() {
                    toggles.push(i);
                }
                let (tag, col) = match op.kind {
                    OpKind::Copy => ("+", theme::p().ok),
                    OpKind::MkDir => ("📁", theme::p().ok),
                    OpKind::Update => ("⟳", theme::accent()),
                    OpKind::Delete => ("✖", theme::p().danger),
                    OpKind::Conflict => ("⚠", theme::p().danger),
                    OpKind::Skip => ("-", theme::p().muted),
                };
                child.add_sized([20.0, row_h], egui::Label::new(RichText::new(tag).color(col).strong()));
                let dir = match op.kind {
                    OpKind::Delete => String::new(),
                    _ if two_way => (if op.right { "A » B" } else { "B » A" }).to_string(),
                    _ => "»".to_string(),
                };
                child.add_sized([42.0, row_h], egui::Label::new(RichText::new(dir).small().color(theme::p().sub)));
                if op.kind == OpKind::Conflict {
                    if child.small_button(if two_way { "A » B" } else { "»" }).on_hover_text(tr("Conservar la de la primera carpeta")).clicked() {
                        resolve.push((i, true));
                    }
                    if child.small_button(if two_way { "B » A" } else { "«" }).on_hover_text(tr("Conservar la de la segunda carpeta")).clicked() {
                        resolve.push((i, false));
                    }
                }
                child.add_sized([70.0, row_h], egui::Label::new(RichText::new(if op.size > 0 { human_size(op.size) } else { String::new() }).small().color(theme::p().sub)));
                let r = child.add(egui::Label::new(RichText::new(&op.rel).color(if op.on || !selectable { theme::p().text } else { theme::p().muted })).truncate());
                r.on_hover_text(format!("{}\n{}", tr(op.why), op.detail));
            }
        });
        for i in toggles {
            plan.ops[i].on = !plan.ops[i].on;
        }
        for (i, keep_first) in resolve {
            resolve_conflict(&mut plan.ops[i], keep_first);
        }
        if view.is_empty() {
            ui.label(RichText::new(tr("Nada que mostrar con los filtros actuales.")).color(theme::p().muted));
        }
        self.plan = Some(plan);
    }

    fn browser_ui(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.browser.take() else { return };
        let mut open = true;
        let (mut nav, mut pick): (Option<PathBuf>, Option<PathBuf>) = (None, None);
        let mut close = false;
        let purpose = b.purpose;
        egui::Window::new(tr("Elegir carpeta")).id(egui::Id::new("sync_browser")).open(&mut open).default_width(480.0).default_height(440.0).show(ctx, |ui| {
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
            egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 270.0)).auto_shrink([false, false]).show(ui, |ui| {
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
        if let Some(p) = &pick {
            let s = p.to_string_lossy().to_string();
            if purpose == 0 {
                self.o.src = s;
            } else {
                self.o.dst = s;
            }
        }
        if open && !close && pick.is_none() {
            self.browser = Some(b);
        }
    }
}

/// The menu next to a folder box: the folder open in the main window, the selected folders, places and favourites.
pub(crate) fn quick_menu(ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) -> Option<PathBuf> {
    let mut pick = None;
    ui.menu_button(tr("➕ Añadir rápido ⏷"), |ui| {
        let label = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().to_string());
        ui.label(RichText::new(tr("Abierta ahora en la ventana principal")).small().color(theme::p().muted));
        if ui.button(format!("📁 {}", label(cwd))).on_hover_text(cwd.to_string_lossy()).clicked() {
            pick = Some(cwd.to_path_buf());
            ui.close_menu();
        }
        let sel_dirs: Vec<&PathBuf> = selection.iter().filter(|p| p.is_dir()).collect();
        if !sel_dirs.is_empty() {
            ui.separator();
            ui.label(RichText::new(tr("Seleccionadas en la ventana principal")).small().color(theme::p().muted));
            for p in sel_dirs {
                if ui.button(format!("📁 {}", label(p))).on_hover_text(p.to_string_lossy()).clicked() {
                    pick = Some(p.clone());
                    ui.close_menu();
                }
            }
        }
        ui.separator();
        ui.label(RichText::new(tr("Lugares")).small().color(theme::p().muted));
        for (name, p) in quick_places() {
            if ui.button(name).on_hover_text(p.to_string_lossy()).clicked() {
                pick = Some(p);
                ui.close_menu();
            }
        }
        let favs = favorite_folders();
        if !favs.is_empty() {
            ui.separator();
            ui.label(RichText::new(tr("Favoritos")).small().color(theme::p().muted));
            for p in favs {
                if ui.button(format!("★ {}", label(&p))).on_hover_text(p.to_string_lossy()).clicked() {
                    pick = Some(p);
                    ui.close_menu();
                }
            }
        }
    });
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(&b).unwrap();
        (dir, a, b)
    }

    fn plan(o: &Opts) -> Plan {
        build_plan(o, &Progress::default(), &AtomicBool::new(false)).unwrap().0
    }

    fn run(o: &Opts) -> Report {
        let p = plan(o);
        execute(p.ops, o, p.src_dirs, &Progress::default(), &AtomicBool::new(false))
    }

    fn opts(a: &Path, b: &Path, mode: u8) -> Opts {
        Opts { src: a.to_string_lossy().to_string(), dst: b.to_string_lossy().to_string(), mode, delete_method: 1, ..Opts::default() }
    }

    fn age(p: &Path, secs: u64) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn mirror_copies_updates_and_deletes() {
        let (dir, a, b) = setup("mirror");
        fs::write(a.join("nuevo.txt"), "hola").unwrap();
        fs::write(a.join("sub/dentro.txt"), "x").unwrap();
        fs::write(a.join("igual.txt"), "igual").unwrap();
        fs::write(b.join("igual.txt"), "igual").unwrap();
        fs::write(a.join("cambia.txt"), "version nueva").unwrap();
        fs::write(b.join("cambia.txt"), "vieja").unwrap();
        age(&b.join("cambia.txt"), 3600);
        fs::write(b.join("sobra.txt"), "sobra").unwrap();
        fs::create_dir_all(b.join("carpeta_sobra/hija")).unwrap();
        fs::write(b.join("carpeta_sobra/hija/f.txt"), "f").unwrap();
        fs::create_dir_all(a.join("vacia")).unwrap();
        let o = opts(&a, &b, 0);
        let p = plan(&o);
        let kinds = |k: OpKind| p.ops.iter().filter(|o| o.kind == k).count();
        assert_eq!(kinds(OpKind::Copy), 2);
        assert_eq!(kinds(OpKind::Update), 1);
        assert_eq!(kinds(OpKind::Delete), 2, "one file and one whole folder");
        assert_eq!(kinds(OpKind::MkDir), 1);
        let r = run(&o);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(b.join("cambia.txt")).unwrap(), "version nueva");
        assert_eq!(fs::read_to_string(b.join("sub/dentro.txt")).unwrap(), "x");
        assert!(!b.join("sobra.txt").exists() && !b.join("carpeta_sobra").exists());
        assert!(b.join("vacia").is_dir());
        // Dates are preserved, so a second comparison finds nothing to do.
        assert!(plan(&o).ops.iter().all(|o| o.kind == OpKind::Skip), "synchronised");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn modes_update_add_only_and_overwrite_rules() {
        let (dir, a, b) = setup("modes");
        fs::write(a.join("a.txt"), "origen nuevo").unwrap();
        fs::write(b.join("a.txt"), "destino").unwrap();
        age(&b.join("a.txt"), 7200);
        fs::write(b.join("solo_destino.txt"), "d").unwrap();
        fs::write(a.join("nuevo.txt"), "n").unwrap();
        let mut o = opts(&a, &b, 1);
        let p = plan(&o);
        assert!(p.ops.iter().any(|o| o.kind == OpKind::Update) && !p.ops.iter().any(|o| o.kind == OpKind::Delete));
        o.mode = 2;
        let p = plan(&o);
        assert!(p.ops.iter().any(|o| o.kind == OpKind::Skip) && !p.ops.iter().any(|o| o.kind == OpKind::Update));
        // Destination newer than source: not overwritten under "only if newer".
        o.mode = 1;
        age(&a.join("a.txt"), 20_000);
        let p = plan(&o);
        assert!(p.ops.iter().any(|x| x.kind == OpKind::Skip && x.rel == "a.txt"));
        o.overwrite = 1;
        assert!(plan(&o).ops.iter().any(|x| x.kind == OpKind::Update && x.rel == "a.txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filters_protect_the_destination_and_old_versions_are_kept() {
        let (dir, a, b) = setup("filters");
        fs::write(a.join("a.txt"), "nuevo contenido").unwrap();
        fs::write(b.join("a.txt"), "viejo").unwrap();
        age(&b.join("a.txt"), 3600);
        fs::create_dir_all(b.join("extra")).unwrap();
        fs::write(b.join("extra/importante.tmp"), "no borrar").unwrap();
        fs::write(b.join("extra/otro.txt"), "o").unwrap();
        let mut o = opts(&a, &b, 0);
        o.exclude = "*.tmp".into();
        o.keep_old = true;
        o.delete_method = 2;
        let r = run(&o);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(b.join("extra/importante.tmp").exists(), "a filtered file is never deleted");
        assert!(!b.join("extra/otro.txt").exists());
        let backups: Vec<_> = fs::read_dir(b.join(".sync-backup")).unwrap().flatten().collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path().join("a.txt")).unwrap(), "viejo");
        assert_eq!(fs::read_to_string(b.join("a.txt")).unwrap(), "nuevo contenido");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_way_newest_wins_and_conflicts_can_be_resolved() {
        let (dir, a, b) = setup("bidi");
        fs::write(a.join("solo_a.txt"), "a").unwrap();
        fs::write(b.join("solo_b.txt"), "b").unwrap();
        fs::write(a.join("c.txt"), "viejo en a").unwrap();
        fs::write(b.join("c.txt"), "nuevo en b!").unwrap();
        age(&a.join("c.txt"), 5000);
        fs::write(a.join("k.txt"), "uno").unwrap();
        fs::write(b.join("k.txt"), "dos!").unwrap();
        let t = SystemTime::now() - Duration::from_secs(900);
        for p in [a.join("k.txt"), b.join("k.txt")] {
            fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
        }
        let o = opts(&a, &b, 3);
        let mut p = plan(&o);
        let conflict = p.ops.iter().position(|x| x.kind == OpKind::Conflict).expect("same date, different content");
        resolve_conflict(&mut p.ops[conflict], false); // keep the second folder's version
        let r = execute(p.ops, &o, p.src_dirs, &Progress::default(), &AtomicBool::new(false));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(b.join("solo_a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(a.join("solo_b.txt")).unwrap(), "b");
        assert_eq!(fs::read_to_string(a.join("c.txt")).unwrap(), "nuevo en b!");
        assert_eq!(fs::read_to_string(a.join("k.txt")).unwrap(), "dos!");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn move_mode_empties_the_source_and_content_compare_works() {
        let (dir, a, b) = setup("move");
        fs::write(a.join("m.txt"), "mover").unwrap();
        fs::write(a.join("sub/n.txt"), "n").unwrap();
        fs::write(a.join("ya.txt"), "ya").unwrap();
        fs::write(b.join("ya.txt"), "ya").unwrap();
        let o = opts(&a, &b, 4);
        let r = run(&o);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(!a.join("m.txt").exists() && !a.join("ya.txt").exists() && !a.join("sub").exists());
        assert_eq!(fs::read_to_string(b.join("sub/n.txt")).unwrap(), "n");
        // Same size, different dates, same content: the smart comparison sees no change.
        let (dir2, a2, b2) = setup("content");
        fs::write(a2.join("f.bin"), "contenido").unwrap();
        fs::write(b2.join("f.bin"), "contenido").unwrap();
        age(&b2.join("f.bin"), 50_000);
        let mut o2 = opts(&a2, &b2, 1);
        let files = |o: &Opts| plan(o).ops.iter().filter(|x| matches!(x.kind, OpKind::Copy | OpKind::Update)).count();
        assert_eq!(files(&o2), 0, "smart: identical content");
        o2.compare = 1;
        assert_eq!(files(&o2), 1, "size and date only: the date differs");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&dir2).unwrap();
    }

    #[test]
    fn rejects_overlapping_folders() {
        let (dir, a, _b) = setup("overlap");
        let o = opts(&a, &a.join("sub"), 0);
        assert!(check_paths(&o).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
