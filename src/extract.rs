//! Extract: ZIP (also encrypted), 7z (also encrypted), TAR (plain, .gz, .bz2, .xz, .zst) and
//! single-file GZ/BZ2/XZ/ZST. Paths that try to escape the destination are refused; a folder
//! named after the archive is made when it helps; existing files are renamed, overwritten or kept.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::{picker_window, DestField, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Codec {
    None,
    Gz,
    Bz2,
    Xz,
    Zst,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum AKind {
    Zip,
    SevenZ,
    Tar(Codec),
    Single(Codec),
}

const SUFFIXES: [(&str, AKind); 17] = [
    (".tar.gz", AKind::Tar(Codec::Gz)),
    (".tgz", AKind::Tar(Codec::Gz)),
    (".tar.bz2", AKind::Tar(Codec::Bz2)),
    (".tbz2", AKind::Tar(Codec::Bz2)),
    (".tar.xz", AKind::Tar(Codec::Xz)),
    (".txz", AKind::Tar(Codec::Xz)),
    (".tar.zst", AKind::Tar(Codec::Zst)),
    (".tzst", AKind::Tar(Codec::Zst)),
    (".tar", AKind::Tar(Codec::None)),
    (".zip", AKind::Zip),
    (".jar", AKind::Zip),
    (".apk", AKind::Zip),
    (".7z", AKind::SevenZ),
    (".gz", AKind::Single(Codec::Gz)),
    (".bz2", AKind::Single(Codec::Bz2)),
    (".xz", AKind::Single(Codec::Xz)),
    (".zst", AKind::Single(Codec::Zst)),
];

/// Which kind of archive a file is, from its name; also how long the extension is.
fn detect(p: &Path) -> Option<(AKind, usize)> {
    let name = p.file_name()?.to_string_lossy().to_lowercase();
    SUFFIXES.iter().find(|(s, _)| name.ends_with(s) && name.len() > s.len()).map(|(s, k)| (*k, s.len()))
}

fn archive_stem(p: &Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "archivo".into());
    match detect(p) {
        Some((_, n)) => name[..name.len() - n].to_string(),
        None => name,
    }
}

/// A decompressing reader over `file`.
fn decoder(c: Codec, file: fs::File) -> io::Result<Box<dyn Read>> {
    let buffered = io::BufReader::with_capacity(1 << 20, file);
    Ok(match c {
        Codec::None => Box::new(buffered),
        Codec::Gz => Box::new(flate2::read::MultiGzDecoder::new(buffered)),
        Codec::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(buffered)),
        Codec::Xz => Box::new(lzma_rust2::XzReader::new(buffered, true)),
        Codec::Zst => Box::new(zstd::stream::read::Decoder::new(buffered)?),
    })
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// `name` below `base`, or None if it would leave it (absolute paths, «..», drive letters).
fn safe_join(base: &Path, name: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    let mut any = false;
    if name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            p if p.ends_with(':') || p.contains('\0') => return None,
            p => {
                out.push(p);
                any = true;
            }
        }
    }
    any.then_some(out)
}

#[derive(Clone)]
pub struct LItem {
    name: String,
    size: u64,
    is_dir: bool,
}

fn list_entries(path: &Path, kind: AKind, pw: &str) -> Result<Vec<LItem>, String> {
    match kind {
        AKind::Zip => {
            let mut z = zip::ZipArchive::new(io::BufReader::new(fs::File::open(path).map_err(es)?)).map_err(es)?;
            let mut v = vec![];
            for i in 0..z.len() {
                let f = z.by_index_raw(i).map_err(es)?;
                let name = f.name().map(|n| n.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(f.name_raw()).into_owned());
                v.push(LItem { size: f.size(), is_dir: f.is_dir(), name });
            }
            Ok(v)
        }
        AKind::SevenZ => {
            let a = sevenz_rust2::Archive::open_with_password(path, &sevenz_rust2::Password::new(pw)).map_err(es)?;
            Ok(a.files.iter().map(|e| LItem { name: e.name().to_string(), size: e.size(), is_dir: e.is_directory() }).collect())
        }
        AKind::Tar(c) => {
            let r = decoder(c, fs::File::open(path).map_err(es)?).map_err(es)?;
            let mut a = tar::Archive::new(r);
            let mut v = vec![];
            for e in a.entries().map_err(es)? {
                let e = e.map_err(es)?;
                v.push(LItem { name: e.path().map_err(es)?.to_string_lossy().to_string(), size: e.size(), is_dir: e.header().entry_type().is_dir() });
            }
            Ok(v)
        }
        AKind::Single(_) => Ok(vec![LItem { name: archive_stem(path), size: 0, is_dir: false }]),
    }
}

// ---------------- writing ----------------

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    current: Mutex<String>,
    bytes: AtomicU64,
    done: AtomicUsize,
    total: AtomicUsize,
}

struct Counting<'a, R> {
    inner: R,
    prog: &'a Progress,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(tr("cancelado")));
        }
        let n = self.inner.read(buf)?;
        self.prog.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[derive(Clone)]
struct Opts {
    dest_other: bool,
    folder_mode: u8,
    conflict: u8,
    password: String,
    filter: Option<Regex>,
    verify_mtime: bool,
}

#[derive(Default)]
struct Tally {
    files: usize,
    bytes: u64,
    skipped: usize,
    unsafe_paths: usize,
}

/// Writes one file safely: to a temporary name first, then renamed; honours the conflict choice.
fn write_file(target: &Path, reader: &mut dyn Read, mtime: Option<SystemTime>, mode: Option<u32>, o: &Opts, prog: &Progress, cancel: &AtomicBool, t: &mut Tally) -> Result<(), String> {
    let mut target = target.to_path_buf();
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    if target.exists() {
        match o.conflict {
            2 => {
                t.skipped += 1;
                return Ok(());
            }
            0 => {
                let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
                target = fsops::unique_name(&dir, &target.file_name().unwrap_or_default().to_string_lossy());
            }
            _ => {}
        }
    }
    let name = target.file_name().unwrap_or_default().to_string_lossy().to_string();
    let tmp = target.with_file_name(format!(".{name}.part"));
    let result = (|| -> Result<u64, String> {
        let mut out = io::BufWriter::new(fs::File::create(&tmp).map_err(es)?);
        let mut counting = Counting { inner: reader, prog, cancel };
        let n = io::copy(&mut counting, &mut out).map_err(es)?;
        out.flush().map_err(es)?;
        let file = out.into_inner().map_err(|e| e.to_string())?;
        if let Some(m) = mtime.filter(|_| o.verify_mtime) {
            let _ = file.set_modified(m);
        }
        drop(file);
        #[cfg(unix)]
        if let Some(m) = mode.filter(|m| m & 0o777 != 0) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(m & 0o7777));
        }
        #[cfg(not(unix))]
        let _ = mode;
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        fs::rename(&tmp, &target).map_err(es)?;
        Ok(n)
    })();
    match result {
        Ok(n) => {
            t.files += 1;
            t.bytes += n;
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("{}: {e}", target.display()))
        }
    }
}

fn zip_time(d: zip::DateTime) -> Option<SystemTime> {
    chrono::NaiveDate::from_ymd_opt(d.year() as i32, d.month() as u32, d.day() as u32)
        .and_then(|day| day.and_hms_opt(d.hour() as u32, d.minute() as u32, d.second() as u32))
        .and_then(|n| n.and_local_timezone(chrono::Local).single())
        .map(SystemTime::from)
}

fn wanted(o: &Opts, name: &str) -> bool {
    o.filter.as_ref().map(|re| re.is_match(name.rsplit(['/', '\\']).next().unwrap_or(name))).unwrap_or(true)
}

fn extract_one(path: &Path, kind: AKind, out: &Path, o: &Opts, prog: &Progress, cancel: &AtomicBool, t: &mut Tally) -> Result<(), String> {
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    match kind {
        AKind::Zip => {
            let mut z = zip::ZipArchive::new(io::BufReader::new(fs::File::open(path).map_err(es)?)).map_err(es)?;
            for i in 0..z.len() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(tr("cancelado").to_string());
                }
                let mut f = if o.password.is_empty() { z.by_index(i).map_err(es)? } else { z.by_index_decrypt(i, o.password.as_bytes()).map_err(|e| format!("{} ({e})", tr("contraseña incorrecta")))? };
                let name = f.name().map(|n| n.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(f.name_raw()).into_owned());
                let Some(target) = safe_join(out, &name) else {
                    t.unsafe_paths += 1;
                    continue;
                };
                if f.is_dir() {
                    fs::create_dir_all(&target).map_err(es)?;
                } else if f.is_symlink() {
                    t.skipped += 1;
                } else if wanted(o, &name) {
                    let (m, mode) = (f.last_modified().and_then(zip_time), f.unix_mode());
                    write_file(&target, &mut f, m, mode, o, prog, cancel, t)?;
                }
            }
            Ok(())
        }
        AKind::SevenZ => {
            let file = fs::File::open(path).map_err(es)?;
            let mut r = sevenz_rust2::ArchiveReader::new(file, sevenz_rust2::Password::new(&o.password)).map_err(|e| format!("{e}"))?;
            r.for_each_entries(|entry, rd| {
                let name = entry.name().to_string();
                let Some(target) = safe_join(out, &name) else {
                    t.unsafe_paths += 1;
                    return Ok(true);
                };
                if entry.is_directory() {
                    fs::create_dir_all(&target)?;
                } else if wanted(o, &name) {
                    write_file(&target, rd, None, None, o, prog, cancel, t).map_err(io::Error::other)?;
                }
                Ok(true)
            })
            .map_err(es)
        }
        AKind::Tar(c) => {
            let reader = Counting { inner: decoder(c, fs::File::open(path).map_err(es)?).map_err(es)?, prog, cancel };
            let mut a = tar::Archive::new(reader);
            for e in a.entries().map_err(es)? {
                let mut e = e.map_err(es)?;
                let name = e.path().map_err(es)?.to_string_lossy().to_string();
                let Some(target) = safe_join(out, &name) else {
                    t.unsafe_paths += 1;
                    continue;
                };
                let ty = e.header().entry_type();
                if ty.is_dir() {
                    fs::create_dir_all(&target).map_err(es)?;
                } else if ty.is_file() {
                    if wanted(o, &name) {
                        let m = e.header().mtime().ok().map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
                        let mode = e.header().mode().ok();
                        // The bytes were already counted by the outer reader.
                        let dummy = Progress::default();
                        write_file(&target, &mut e, m, mode, o, &dummy, cancel, t)?;
                    }
                } else if ty.is_symlink() {
                    #[cfg(unix)]
                    {
                        let link = e.link_name().ok().flatten().map(|l| l.to_path_buf());
                        match link {
                            Some(l) if !l.is_absolute() && !l.components().any(|c| matches!(c, Component::ParentDir)) && !target.exists() => {
                                if let Some(p) = target.parent() {
                                    fs::create_dir_all(p).map_err(es)?;
                                }
                                std::os::unix::fs::symlink(&l, &target).map_err(es)?;
                            }
                            _ => t.skipped += 1,
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        t.skipped += 1;
                    }
                } else {
                    t.skipped += 1;
                }
            }
            Ok(())
        }
        AKind::Single(c) => {
            let mut rd = decoder(c, fs::File::open(path).map_err(es)?).map_err(es)?;
            let target = out.join(archive_stem(path));
            let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
            write_file(&target, &mut rd, mtime, None, o, prog, cancel, t)
        }
    }
}

#[derive(Clone, Default)]
struct ExResult {
    archive: PathBuf,
    out: PathBuf,
    files: usize,
    bytes: u64,
    skipped: usize,
    unsafe_paths: usize,
    error: Option<String>,
    deleted: bool,
}

/// Where an archive's contents go.
fn target_dir(path: &Path, kind: AKind, base: &Path, folder_mode: u8, conflict: u8, pw: &str) -> PathBuf {
    let stem = archive_stem(path);
    let own = || {
        let mut p = base.join(&stem);
        if conflict == 0 && p.exists() {
            p = fsops::unique_name(base, &stem);
        }
        p
    };
    if matches!(kind, AKind::Single(_)) {
        return base.to_path_buf();
    }
    match folder_mode {
        0 => own(),
        1 => base.to_path_buf(),
        _ => {
            // Smart: no extra folder when everything is already inside a single one.
            let roots: BTreeSet<String> = list_entries(path, kind, pw).map(|v| v.iter().filter_map(|e| e.name.split(['/', '\\']).find(|p| !p.is_empty() && *p != ".").map(str::to_string)).collect()).unwrap_or_default();
            let all_inside = list_entries(path, kind, pw).map(|v| v.iter().all(|e| e.is_dir || e.name.contains('/') || e.name.contains('\\'))).unwrap_or(false);
            if roots.len() == 1 && all_inside {
                base.to_path_buf()
            } else {
                own()
            }
        }
    }
}

// ---------------- the tool ----------------

struct Job {
    rx: Receiver<(Vec<ExResult>, f32)>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct Extractor {
    items: Vec<PathBuf>,
    picker: Option<Picker>,
    dest: DestField,
    dest_other: bool,
    folder_mode: u8,
    conflict: u8,
    password: String,
    show_pw: bool,
    filter: String,
    filter_regex: bool,
    keep_dates: bool,
    delete_orig: bool,
    confirm_delete: bool,
    job: Option<Job>,
    results: Vec<ExResult>,
    status: Option<(String, bool)>,
    listing: Option<(PathBuf, Result<Vec<LItem>, String>)>,
    list_rx: Option<Receiver<(PathBuf, Result<Vec<LItem>, String>)>>,
}

const FOLDER_MODES: [&str; 3] = ["En una carpeta con el nombre del paquete", "Directamente aquí", "Inteligente: carpeta solo si hace falta"];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

impl Extractor {
    pub fn new(targets: Vec<PathBuf>) -> Extractor {
        let items = targets.into_iter().filter(|p| detect(p).is_some()).collect();
        Extractor {
            items,
            picker: None,
            dest: DestField::new("extract_dest"),
            dest_other: false,
            folder_mode: 2,
            conflict: 0,
            password: String::new(),
            show_pw: false,
            filter: String::new(),
            filter_regex: false,
            keep_dates: true,
            delete_orig: false,
            confirm_delete: false,
            job: None,
            results: vec![],
            status: None,
            listing: None,
            list_rx: None,
        }
    }

    /// Extracts every archive in `dir` next to itself, for trying the tool (`--extract`).
    pub fn demo(&mut self, dir: &Path) {
        let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| detect(p).is_some()).collect()).unwrap_or_default();
        v.sort();
        self.items = v;
        self.dest_other = true;
        self.dest.text = dir.join("_extraido").to_string_lossy().to_string();
        self.start();
    }

    fn add_items(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.is_file() && detect(&p).is_some() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.items.is_empty() {
            return Err(tr("Añade al menos un archivo comprimido (zip, 7z, tar.gz…).").to_string());
        }
        if self.dest_other && self.dest.text.trim().is_empty() {
            return Err(tr("Elige la carpeta de destino.").to_string());
        }
        crate::rename::name_matcher(&self.filter, self.filter_regex).map_err(|e| trf!("Expresión no válida: {}", e))?;
        if self.delete_orig && !self.confirm_delete {
            return Err(tr("Marca la casilla para confirmar que se envíen los paquetes a la papelera.").to_string());
        }
        Ok(())
    }

    fn start(&mut self) {
        if let Err(e) = self.validate() {
            self.status = Some((e, true));
            return;
        }
        let o = Opts {
            dest_other: self.dest_other,
            folder_mode: self.folder_mode,
            conflict: self.conflict,
            password: self.password.clone(),
            filter: crate::rename::name_matcher(&self.filter, self.filter_regex).ok().flatten(),
            verify_mtime: self.keep_dates,
        };
        let items = self.items.clone();
        let dest = self.dest.path();
        let delete = self.delete_orig;
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        progress.total.store(items.len(), Ordering::Relaxed);
        if let Ok(mut s) = progress.stage.lock() {
            *s = tr("Descomprimiendo").to_string();
        }
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut results = vec![];
            for path in items {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(mut c) = p2.current.lock() {
                    *c = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                }
                let mut r = ExResult { archive: path.clone(), ..Default::default() };
                match detect(&path) {
                    None => r.error = Some(tr("formato no reconocido").to_string()),
                    Some((kind, _)) => {
                        let base = if o.dest_other { dest.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
                        let out = target_dir(&path, kind, &base, o.folder_mode, o.conflict, &o.password);
                        r.out = out.clone();
                        let mut t = Tally::default();
                        match extract_one(&path, kind, &out, &o, &p2, &c2, &mut t) {
                            Ok(()) => {
                                if delete && trash::delete(&path).is_ok() {
                                    r.deleted = true;
                                }
                            }
                            Err(e) => r.error = Some(e),
                        }
                        r.files = t.files;
                        r.bytes = t.bytes;
                        r.skipped = t.skipped;
                        r.unsafe_paths = t.unsafe_paths;
                    }
                }
                p2.done.fetch_add(1, Ordering::Relaxed);
                results.push(r);
            }
            let _ = tx.send((results, started.elapsed().as_secs_f32()));
        });
        self.results.clear();
        self.status = None;
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        if let Some(rx) = &self.list_rx {
            if let Ok(l) = rx.try_recv() {
                self.listing = Some(l);
                self.list_rx = None;
            }
        }
        let Some(job) = &self.job else { return };
        let Ok((results, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        let ok = results.iter().filter(|r| r.error.is_none()).count();
        let bad = results.len() - ok;
        let (files, bytes): (usize, u64) = results.iter().fold((0, 0), |(f, b), r| (f + r.files, b + r.bytes));
        let head = if cancelled { tr("Extracción cancelada") } else { tr("Extracción terminada") };
        self.status = Some((trf!("{}: {} paquete(s) · {} archivos · {} · {} error(es) · {} s", head, ok, files, human_size(bytes), bad, format!("{secs:.1}")), bad > 0 || cancelled));
        self.results = results;
        self.confirm_delete = false;
        if self.delete_orig {
            let gone: Vec<PathBuf> = self.results.iter().filter(|r| r.deleted).map(|r| r.archive.clone()).collect();
            self.items.retain(|p| !gone.contains(p));
        }
        events.push(ToolEvent::Reload);
    }

    fn show_listing(&mut self, path: &Path) {
        let Some((kind, _)) = detect(path) else { return };
        let (tx, rx) = channel();
        let (p, pw) = (path.to_path_buf(), self.password.clone());
        std::thread::spawn(move || {
            let l = list_entries(&p, kind, &pw);
            let _ = tx.send((p, l));
        });
        self.listing = None;
        self.list_rx = Some(rx);
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add_items(dropped);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("extract_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.items_ui(ui, selection, cwd);
                ui.separator();
                self.options_ui(ui, cwd, selection);
            });
            ui.separator();
            self.action_ui(ui);
            self.listing_ui(ui);
            self.results_ui(ui, events);
        });
        let ctx = ui.ctx().clone();
        let added = picker_window(&ctx, "extract_picker", &mut self.picker);
        self.add_items(added);
    }

    fn items_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("PAQUETES A DESCOMPRIMIR")).small().strong().color(theme::p().sub));
        let mut remove = None;
        let mut view = None;
        egui::ScrollArea::vertical().id_salt("extract_items").max_height(110.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, p) in self.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    if ui.small_button("👁").on_hover_text(tr("Ver qué contiene")).clicked() {
                        view = Some(p.clone());
                    }
                    let size = fs::metadata(p).map(|m| human_size(m.len())).unwrap_or_default();
                    ui.label(RichText::new(size).small().color(theme::p().sub));
                    ui.add(egui::Label::new(p.to_string_lossy()).truncate());
                });
            }
            if self.items.is_empty() {
                ui.label(RichText::new(tr("Todavía no hay nada. Añade los seleccionados, elige archivos o arrástralos aquí.")).color(theme::p().muted));
            }
        });
        if let Some(k) = remove {
            self.items.remove(k);
        }
        if let Some(p) = view {
            self.show_listing(&p);
        }
        ui.horizontal_wrapped(|ui| {
            let sel: Vec<PathBuf> = selection.iter().filter(|p| detect(p).is_some()).cloned().collect();
            if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).on_hover_text(tr("Solo se añaden los archivos comprimidos que reconoce.")).clicked() {
                self.add_items(sel);
            }
            if ui.button(tr("📂 Examinar…")).clicked() {
                self.picker = Some(Picker::new(self.items.last().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| cwd.to_path_buf())));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
            ui.label(RichText::new(tr("Reconoce zip, 7z, tar, tar.gz, tar.bz2, tar.xz, tar.zst, gz, bz2, xz y zst.")).small().color(theme::p().muted));
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.label(RichText::new(tr("DÓNDE Y CÓMO")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Extraer en"));
            ui.selectable_value(&mut self.dest_other, false, tr("Junto a cada paquete"));
            ui.selectable_value(&mut self.dest_other, true, tr("Otra carpeta"));
            if self.dest_other {
                self.dest.ui(ui, cwd, selection);
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Carpeta"));
            for (k, l) in FOLDER_MODES.iter().enumerate() {
                ui.selectable_value(&mut self.folder_mode, k as u8, tr(l));
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Si ya existe"));
            for (k, l) in CONFLICTS.iter().enumerate() {
                ui.selectable_value(&mut self.conflict, k as u8, tr(l));
            }
        });
        egui::CollapsingHeader::new(tr("⚙ Más opciones")).default_open(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Contraseña"));
                ui.add(egui::TextEdit::singleline(&mut self.password).password(!self.show_pw).hint_text(tr("solo si el paquete la pide")).desired_width(220.0));
                ui.checkbox(&mut self.show_pw, tr("mostrar"));
            });
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Solo extraer"));
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(if self.filter_regex { "\\.(jpg|png)$" } else { "*.jpg, *.png" }).desired_width(260.0)).on_hover_text(tr("Solo los archivos cuyo nombre coincida (vacío = todo)"));
                ui.checkbox(&mut self.filter_regex, tr("Expresión regular"));
            });
            ui.checkbox(&mut self.keep_dates, tr("Conservar las fechas de modificación"));
            ui.checkbox(&mut self.delete_orig, tr("Enviar los paquetes a la papelera al terminar")).on_hover_text(tr("Solo los que se extrajeron sin errores."));
            if self.delete_orig {
                ui.checkbox(&mut self.confirm_delete, RichText::new(tr("Confirmo que los paquetes vayan a la papelera.")).color(theme::p().danger));
            }
            ui.label(RichText::new(tr("Por seguridad, las rutas que intentan salir de la carpeta de destino (como «../») se ignoran.")).small().color(theme::p().muted));
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("📂 Descomprimir")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.items.is_empty(), btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let (d, t) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(180.0).show_percentage());
                let secs = job.started.elapsed().as_secs_f64().max(0.1);
                ui.label(RichText::new(format!("{d}/{t} · {}/s", human_size((p.bytes.load(Ordering::Relaxed) as f64 / secs) as u64))).small());
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

    fn listing_ui(&mut self, ui: &mut egui::Ui) {
        if self.list_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("Leyendo el contenido…"));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut close = false;
        if let Some((path, res)) = &self.listing {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(trf!("Contenido de {}", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())).strong());
                if ui.small_button(tr("Cerrar")).clicked() {
                    close = true;
                }
            });
            match res {
                Err(e) => {
                    ui.label(RichText::new(e).color(theme::p().danger));
                }
                Ok(items) => {
                    let total: u64 = items.iter().map(|i| i.size).sum();
                    ui.label(RichText::new(trf!("{} elementos · {} descomprimidos", items.len(), human_size(total))).small().color(theme::p().sub));
                    egui::ScrollArea::vertical().id_salt("extract_listing").max_height(crate::tools::grow(ui, 200.0)).auto_shrink([false, true]).show_rows(ui, 18.0, items.len(), |ui, range| {
                        for it in &items[range] {
                            ui.horizontal(|ui| {
                                ui.label(if it.is_dir { "📁" } else { "🗋" });
                                ui.add_sized([70.0, 16.0], egui::Label::new(RichText::new(if it.is_dir { String::new() } else { human_size(it.size) }).small().color(theme::p().sub)));
                                ui.add(egui::Label::new(&it.name).truncate());
                            });
                        }
                    });
                }
            }
        }
        if close {
            self.listing = None;
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.results.is_empty() {
            return;
        }
        ui.separator();
        egui::ScrollArea::vertical().id_salt("extract_res").max_height(crate::tools::grow(ui, 220.0)).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("extract_grid").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for r in &self.results {
                    ui.label(r.archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                    ui.label(RichText::new(trf!("{} archivos · {}", r.files, human_size(r.bytes))).small().color(theme::p().sub));
                    if r.out.as_os_str().is_empty() {
                        ui.label("");
                    } else if ui.link(RichText::new(r.out.to_string_lossy()).small()).on_hover_text(tr("Ir a la carpeta")).clicked() {
                        events.push(ToolEvent::Navigate(r.out.clone()));
                    }
                    let (txt, col) = match &r.error {
                        Some(e) => (format!("✖ {e}"), theme::p().danger),
                        None => {
                            let mut t = String::from("✔");
                            if r.skipped > 0 {
                                t.push_str(&format!(" · {} {}", r.skipped, tr("omitidos")));
                            }
                            if r.unsafe_paths > 0 {
                                t.push_str(&format!(" · {} {}", r.unsafe_paths, tr("rutas peligrosas ignoradas")));
                            }
                            if r.deleted {
                                t.push_str(&format!(" · {}", tr("paquete a la papelera")));
                            }
                            (t, theme::p().ok)
                        }
                    };
                    ui.label(RichText::new(txt).small().color(col));
                    ui.end_row();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src/sub")).unwrap();
        fs::write(d.join("src/a.txt"), "hola ".repeat(500)).unwrap();
        fs::write(d.join("src/sub/b.txt"), "adios").unwrap();
        d
    }

    fn opts() -> Opts {
        Opts { dest_other: false, folder_mode: 1, conflict: 0, password: String::new(), filter: None, verify_mtime: true }
    }

    fn run(path: &Path, out: &Path, o: &Opts) -> Result<Tally, String> {
        let mut t = Tally::default();
        let (kind, _) = detect(path).unwrap();
        extract_one(path, kind, out, o, &Progress::default(), &AtomicBool::new(false), &mut t).map(|_| t)
    }

    #[test]
    fn safe_paths_and_detection() {
        let base = Path::new("/x/y");
        assert_eq!(safe_join(base, "a/b.txt"), Some(PathBuf::from("/x/y/a/b.txt")));
        assert_eq!(safe_join(base, "./a//b"), Some(PathBuf::from("/x/y/a/b")));
        assert!(safe_join(base, "../evil").is_none() && safe_join(base, "a/../../evil").is_none());
        assert!(safe_join(base, "/etc/passwd").is_none() && safe_join(base, "C:\\Windows").is_none() && safe_join(base, "").is_none());
        assert_eq!(detect(Path::new("x.TAR.GZ")).map(|d| d.0), Some(AKind::Tar(Codec::Gz)));
        assert_eq!(detect(Path::new("x.tgz")).map(|d| d.0), Some(AKind::Tar(Codec::Gz)));
        assert_eq!(detect(Path::new("foto.jpg")), None);
        assert_eq!(archive_stem(Path::new("/a/datos.tar.zst")), "datos");
    }

    #[test]
    fn zip_with_password_conflicts_and_zip_slip() {
        let d = dir("zip");
        let path = d.join("p.zip");
        {
            use zip::write::SimpleFileOptions;
            let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let opt = SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "pw");
            z.start_file("carpeta/a.txt", opt).unwrap();
            z.write_all(b"secreto").unwrap();
            z.start_file("../escape.txt", SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "pw")).unwrap();
            z.write_all(b"mal").unwrap();
            z.finish().unwrap();
        }
        let out = d.join("out");
        let mut o = opts();
        assert!(run(&path, &out, &o).is_err(), "no password");
        o.password = "pw".into();
        let t = run(&path, &out, &o).unwrap();
        assert_eq!(fs::read_to_string(out.join("carpeta/a.txt")).unwrap(), "secreto");
        assert_eq!(t.unsafe_paths, 1);
        assert!(!d.join("escape.txt").exists());
        // Second run: the existing file is renamed, kept, or overwritten as asked.
        run(&path, &out, &o).unwrap();
        assert!(out.join("carpeta/a (copia).txt").exists());
        o.conflict = 2;
        assert_eq!(run(&path, &out, &o).unwrap().skipped, 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn tar_variants_seven_z_and_single_streams() {
        let d = dir("many");
        let tgz = d.join("p.tar.gz");
        {
            let enc = flate2::write::GzEncoder::new(fs::File::create(&tgz).unwrap(), flate2::Compression::default());
            let mut b = tar::Builder::new(enc);
            b.append_dir_all("raiz", d.join("src")).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let o = opts();
        let out = d.join("o1");
        let t = run(&tgz, &out, &o).unwrap();
        assert_eq!(t.files, 2);
        assert_eq!(fs::read_to_string(out.join("raiz/sub/b.txt")).unwrap(), "adios");
        // 7z written with the library's own helper.
        let sz = d.join("p.7z");
        sevenz_rust2::compress_to_path(d.join("src"), &sz).unwrap();
        let out = d.join("o2");
        run(&sz, &out, &o).unwrap();
        assert_eq!(fs::read_to_string(out.join("sub/b.txt")).unwrap(), "adios");
        assert_eq!(list_entries(&sz, AKind::SevenZ, "").unwrap().iter().filter(|e| !e.is_dir).count(), 2);
        // A single compressed file, with a filter on the way.
        let gz = d.join("nota.txt.gz");
        {
            let mut e = flate2::write::GzEncoder::new(fs::File::create(&gz).unwrap(), flate2::Compression::default());
            e.write_all(b"una nota").unwrap();
            e.finish().unwrap();
        }
        let out = d.join("o3");
        run(&gz, &out, &o).unwrap();
        assert_eq!(fs::read_to_string(out.join("nota.txt")).unwrap(), "una nota");
        let mut o = opts();
        o.filter = crate::rename::name_matcher("b.*", false).unwrap();
        let out = d.join("o4");
        let t = run(&tgz, &out, &o).unwrap();
        assert_eq!(t.files, 1);
        // Smart folder choice: a single root folder is used as it is, loose files get a folder.
        assert_eq!(target_dir(&tgz, AKind::Tar(Codec::Gz), &d, 2, 0, ""), d);
        assert_eq!(target_dir(&sz, AKind::SevenZ, &d, 2, 0, ""), d.join("p"));
        fs::remove_dir_all(&d).unwrap();
    }
}
