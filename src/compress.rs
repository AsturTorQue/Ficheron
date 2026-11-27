//! Compress: ZIP, 7z, TAR (plain, .gz, .bz2, .xz, .zst) and single-file GZ/BZ2/XZ/ZST, all in pure
//! Rust or bundled code. One package for everything, one per item, or one per file; levels,
//! filters, passwords (AES-256), splitting into parts, verification and optional removal of the
//! originals once the package has been checked.

use crate::dupes::{parse_size, size_field, Browser};
use crate::fsops::human_size;
use crate::i18n::tr;
use crate::sync::quick_menu;
use crate::theme;
use crate::tools::ToolEvent;
use chrono::{Datelike, Timelike};
use eframe::egui::{self, Color32, RichText};
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fmt {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Gz,
    Bz2,
    Xz,
    Zst,
}

const FORMATS: [(Fmt, &str, &str, &str); 11] = [
    (Fmt::Zip, "zip", "ZIP", "Se abre en cualquier sistema sin instalar nada. Admite contraseña."),
    (Fmt::SevenZ, "7z", "7z", "La mejor compresión (LZMA2). Se abre con 7-Zip, Keka, p7zip… Admite contraseña."),
    (Fmt::Tar, "tar", "TAR", "Solo empaqueta, sin comprimir. Conserva permisos y enlaces."),
    (Fmt::TarGz, "tar.gz", "TAR.GZ", "El estándar de Linux y Mac: rápido y muy compatible."),
    (Fmt::TarBz2, "tar.bz2", "TAR.BZ2", "Más compresión que gzip, algo más lento."),
    (Fmt::TarXz, "tar.xz", "TAR.XZ", "Muy buena compresión; usado en distribuciones de Linux."),
    (Fmt::TarZst, "tar.zst", "TAR.ZST", "Zstandard: compresión muy buena y muy rápida; ideal para copias de seguridad."),
    (Fmt::Gz, "gz", "GZ", "Comprime un archivo suelto (no carpetas)."),
    (Fmt::Bz2, "bz2", "BZ2", "Comprime un archivo suelto (no carpetas)."),
    (Fmt::Xz, "xz", "XZ", "Comprime un archivo suelto (no carpetas)."),
    (Fmt::Zst, "zst", "ZST", "Comprime un archivo suelto (no carpetas)."),
];

impl Fmt {
    fn ext(self) -> &'static str {
        FORMATS.iter().find(|f| f.0 == self).map(|f| f.1).unwrap_or("zip")
    }
    fn single(self) -> bool {
        matches!(self, Fmt::Gz | Fmt::Bz2 | Fmt::Xz | Fmt::Zst)
    }
    fn tar_based(self) -> bool {
        matches!(self, Fmt::Tar | Fmt::TarGz | Fmt::TarBz2 | Fmt::TarXz | Fmt::TarZst)
    }
    fn password(self) -> bool {
        matches!(self, Fmt::Zip | Fmt::SevenZ)
    }
    fn default_level(self) -> u32 {
        match self {
            Fmt::Bz2 | Fmt::TarBz2 => 9,
            Fmt::Zst | Fmt::TarZst => 5,
            _ => 6,
        }
    }
    fn has_level(self) -> bool {
        self != Fmt::Tar
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    None,
    Gz,
    Bz2,
    Xz,
    Zst,
}

fn codec(f: Fmt) -> Codec {
    match f {
        Fmt::TarGz | Fmt::Gz => Codec::Gz,
        Fmt::TarBz2 | Fmt::Bz2 => Codec::Bz2,
        Fmt::TarXz | Fmt::Xz => Codec::Xz,
        Fmt::TarZst | Fmt::Zst => Codec::Zst,
        _ => Codec::None,
    }
}

const MODES: [(&str, &str); 3] = [
    ("Todo en un solo paquete", "Un único archivo comprimido con todo lo que has añadido."),
    ("Un paquete por cada elemento", "Cada archivo o carpeta añadidos se comprime en su propio paquete."),
    ("Un paquete por cada archivo", "Cada archivo, incluidos los que hay dentro de las carpetas, va en su propio paquete."),
];
const ZIP_METHODS: [&str; 4] = ["Deflate (compatible con todo)", "Bzip2", "Zstandard (requiere un descompresor reciente)", "Sin comprimir (solo guardar)"];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

type Preset = (&'static str, &'static str, fn(&mut Opts));

const PRESETS: [Preset; 8] = [
    ("📧 Para enviar por correo", "ZIP dividido en partes de 20 MB.", |o| {
        o.fmt = Fmt::Zip;
        o.level = 6;
        o.split_text = "20 MB".into();
    }),
    ("📦 Máxima compresión", "7z con el nivel más alto: tarda más pero ocupa menos.", |o| {
        o.fmt = Fmt::SevenZ;
        o.level = 9;
    }),
    ("⚡ Rápido", "ZIP con el nivel más bajo.", |o| {
        o.fmt = Fmt::Zip;
        o.level = 1;
    }),
    ("🐧 Estándar de Linux y Mac", "TAR.GZ, nivel equilibrado.", |o| {
        o.fmt = Fmt::TarGz;
        o.level = 6;
    }),
    ("💾 Copia de seguridad", "TAR.ZST: muy rápido y con buena compresión.", |o| {
        o.fmt = Fmt::TarZst;
        o.level = 5;
        o.verify = true;
    }),
    ("📦 Un ZIP por cada archivo", "Cada archivo, también los de dentro de carpetas, en su propio ZIP.", |o| {
        o.fmt = Fmt::Zip;
        o.mode = 2;
    }),
    ("📁 Un paquete por cada elemento", "Cada archivo o carpeta en su propio paquete.", |o| o.mode = 1),
    ("🔒 Con contraseña", "7z con cifrado AES-256 (también cifra los nombres de los archivos).", |o| {
        o.fmt = Fmt::SevenZ;
        o.level = 6;
    }),
];

#[derive(Clone)]
struct Opts {
    fmt: Fmt,
    mode: u8,
    level: u32,
    zip_method: u8,
    store_compressed: bool,
    name: String,
    keep_ext: bool,
    dest_mode: u8,
    dest: String,
    conflict: u8,
    include: String,
    exclude: String,
    pat_regex: bool,
    hidden: bool,
    follow_links: bool,
    root_folder: bool,
    password: String,
    password2: String,
    show_pw: bool,
    comment: String,
    verify: bool,
    delete_orig: bool,
    threads: usize,
    split_text: String,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            fmt: Fmt::Zip,
            mode: 0,
            level: 6,
            zip_method: 0,
            store_compressed: true,
            name: "{nombre}".into(),
            keep_ext: false,
            dest_mode: 0,
            dest: String::new(),
            conflict: 0,
            include: String::new(),
            exclude: ".DS_Store, Thumbs.db, desktop.ini".into(),
            pat_regex: false,
            hidden: true,
            follow_links: false,
            root_folder: true,
            password: String::new(),
            password2: String::new(),
            show_pw: false,
            comment: String::new(),
            verify: true,
            delete_orig: false,
            threads: std::thread::available_parallelism().map(|n| n.get().min(4)).unwrap_or(2),
            split_text: String::new(),
        }
    }
}

// ---------------- entries and packages ----------------

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
    mode: u32,
    link: Option<PathBuf>,
}

struct Pack {
    out: PathBuf,
    label: String,
    entries: Vec<Entry>,
    sources: Vec<PathBuf>,
}

struct Filter {
    include: Option<Regex>,
    exclude: Option<Regex>,
    hidden: bool,
    follow: bool,
    tar: bool,
    skip: HashSet<PathBuf>,
}

struct Walker<'a> {
    f: &'a Filter,
    out: Vec<Entry>,
    errs: Vec<String>,
    links_skipped: usize,
    cancel: &'a AtomicBool,
}

fn unix_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if meta.is_dir() {
            0o755
        } else {
            0o644
        }
    }
}

fn entry_of(path: &Path, name: String, meta: &fs::Metadata, link: Option<PathBuf>) -> Entry {
    Entry { path: path.to_path_buf(), name, is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, mtime: meta.modified().ok(), mode: unix_mode(meta), link }
}

impl Walker<'_> {
    /// Adds `item` under the archive name `name`; for a folder, its contents too.
    fn item(&mut self, item: &Path, name: &str, include_root: bool, files_only: bool) {
        let Ok(meta) = fs::symlink_metadata(item) else {
            self.errs.push(format!("{}: {}", item.display(), tr("no se puede leer")));
            return;
        };
        let meta = if meta.file_type().is_symlink() {
            match fs::metadata(item) {
                Ok(m) => m,
                Err(_) => meta,
            }
        } else {
            meta
        };
        if meta.is_dir() {
            if include_root && !files_only {
                self.out.push(entry_of(item, name.to_string(), &meta, None));
            }
            let prefix = if include_root { name.to_string() } else { String::new() };
            self.dir(item, &prefix, files_only, 0);
        } else if meta.is_file() {
            self.out.push(entry_of(item, name.to_string(), &meta, None));
        }
    }

    fn dir(&mut self, dir: &Path, prefix: &str, files_only: bool, depth: u32) {
        if depth > 60 || self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.errs.push(format!("{}: {e}", dir.display()));
                return;
            }
        };
        let mut children: Vec<_> = rd.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().to_string();
            let path = child.path();
            if self.f.skip.contains(&path) || (!self.f.hidden && name.starts_with('.')) || self.f.exclude.as_ref().map(|re| re.is_match(&name)).unwrap_or(false) {
                continue;
            }
            let Ok(lmeta) = fs::symlink_metadata(&path) else { continue };
            let arc = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if lmeta.file_type().is_symlink() {
                if self.f.follow {
                    match fs::metadata(&path) {
                        Ok(m) if m.is_dir() => {
                            if !files_only {
                                self.out.push(entry_of(&path, arc.clone(), &m, None));
                            }
                            self.dir(&path, &arc, files_only, depth + 1);
                        }
                        Ok(m) if m.is_file()
                            && self.f.include.as_ref().map(|re| re.is_match(&name)).unwrap_or(true) => {
                                self.out.push(entry_of(&path, arc, &m, None));
                            }
                        _ => {}
                    }
                } else if self.f.tar && !files_only {
                    if let Ok(t) = fs::read_link(&path) {
                        self.out.push(entry_of(&path, arc, &lmeta, Some(t)));
                    }
                } else {
                    self.links_skipped += 1;
                }
            } else if lmeta.is_dir() {
                if !files_only {
                    self.out.push(entry_of(&path, arc.clone(), &lmeta, None));
                }
                self.dir(&path, &arc, files_only, depth + 1);
            } else if lmeta.is_file() && self.f.include.as_ref().map(|re| re.is_match(&name)).unwrap_or(true) {
                self.out.push(entry_of(&path, arc, &lmeta, None));
            }
        }
    }
}

#[derive(Default)]
struct Progress {
    stage: Mutex<String>,
    current: Mutex<String>,
    bytes: AtomicU64,
    total: AtomicU64,
    packs_done: AtomicUsize,
    packs_total: AtomicUsize,
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

/// Opens only when first read and lets go at the end, so thousands of files don't hold thousands of handles.
struct Lazy {
    path: PathBuf,
    file: Option<fs::File>,
    done: bool,
}

impl Read for Lazy {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.file.is_none() {
            self.file = Some(fs::File::open(&self.path)?);
        }
        let n = self.file.as_mut().unwrap().read(buf)?;
        if n == 0 {
            self.file = None;
            self.done = true;
        }
        Ok(n)
    }
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn counting<'a>(path: &Path, prog: &'a Progress, cancel: &'a AtomicBool) -> Result<Counting<'a, fs::File>, String> {
    Ok(Counting { inner: fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?, prog, cancel })
}

fn zstd_level(l: u32) -> i32 {
    [1, 2, 3, 5, 7, 9, 12, 15, 19, 22][l.min(9) as usize]
}

fn already_compressed(name: &str) -> bool {
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        "zip" | "rar" | "7z" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "avif" | "mp3" | "aac" | "m4a" | "ogg" | "opus" | "flac" | "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4v" | "wmv" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub" | "apk" | "jar" | "pdf"
    )
}

fn zip_time(t: SystemTime) -> Option<zip::DateTime> {
    let d: chrono::DateTime<chrono::Local> = t.into();
    zip::DateTime::from_date_and_time(d.year().try_into().ok()?, d.month() as u8, d.day() as u8, d.hour() as u8, d.minute() as u8, d.second() as u8).ok()
}

// ---------------- writers ----------------

fn write_zip(p: &Pack, o: &Opts, tmp: &Path, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod as M;
    let file = fs::File::create(tmp).map_err(es)?;
    let mut zw = zip::ZipWriter::new(io::BufWriter::new(file));
    if !o.comment.trim().is_empty() {
        zw.set_comment(o.comment.trim().to_string()).map_err(es)?;
    }
    let pw = (!o.password.is_empty()).then(|| o.password.clone());
    for e in &p.entries {
        if cancel.load(Ordering::Relaxed) {
            return Err(tr("cancelado").to_string());
        }
        let mut base = SimpleFileOptions::default();
        #[cfg(unix)]
        {
            base = base.unix_permissions(e.mode);
        }
        if let Some(dt) = e.mtime.and_then(zip_time) {
            base = base.last_modified_time(dt);
        }
        if e.is_dir {
            zw.add_directory(format!("{}/", e.name), base).map_err(es)?;
            continue;
        }
        if e.link.is_some() {
            continue;
        }
        let stored = o.zip_method == 3 || (o.level == 0 && o.zip_method == 0) || (o.store_compressed && already_compressed(&e.name));
        let (method, level) = if stored {
            (M::Stored, None)
        } else {
            match o.zip_method {
                1 => (M::Bzip2, Some(o.level.clamp(1, 9) as i64)),
                2 => (M::Zstd, Some(zstd_level(o.level) as i64)),
                _ => (M::Deflated, Some(o.level.min(9) as i64)),
            }
        };
        let mut opt = base.compression_method(method).compression_level(level);
        if e.size >= u32::MAX as u64 {
            opt = opt.large_file(true);
        }
        match &pw {
            Some(pw) => zw.start_file(&e.name, opt.with_aes_encryption(zip::AesMode::Aes256, pw)).map_err(es)?,
            None => zw.start_file(&e.name, opt).map_err(es)?,
        }
        let mut r = counting(&e.path, prog, cancel)?;
        io::copy(&mut r, &mut zw).map_err(es)?;
    }
    zw.finish().map_err(es)?.flush().map_err(es)
}

fn write_7z(p: &Pack, o: &Opts, tmp: &Path, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, Password};
    let file = fs::File::create(tmp).map_err(es)?;
    let mut w = ArchiveWriter::new(file).map_err(es)?;
    let mut methods: Vec<EncoderConfiguration> = vec![];
    if !o.password.is_empty() {
        methods.push(AesEncoderOptions::new(Password::new(&o.password)).into());
    }
    methods.push(Lzma2Options::from_level(o.level.min(9)).into());
    w.set_content_methods(methods);
    for e in &p.entries {
        if cancel.load(Ordering::Relaxed) {
            return Err(tr("cancelado").to_string());
        }
        if e.link.is_some() {
            continue;
        }
        let entry = ArchiveEntry::from_path(&e.path, e.name.clone());
        if e.is_dir {
            w.push_archive_entry::<&[u8]>(entry, None).map_err(es)?;
        } else {
            let reader = Counting { inner: Lazy { path: e.path.clone(), file: None, done: false }, prog, cancel };
            w.push_archive_entry(entry, Some(reader)).map_err(es)?;
        }
    }
    w.finish().map_err(es)?.flush().map_err(es)
}

fn write_tar(w: &mut dyn Write, p: &Pack, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    let mut b = tar::Builder::new(w);
    b.follow_symlinks(false);
    for e in &p.entries {
        if cancel.load(Ordering::Relaxed) {
            return Err(tr("cancelado").to_string());
        }
        let mut h = tar::Header::new_gnu();
        h.set_mode(e.mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(e.mtime.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
        if e.is_dir {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            b.append_data(&mut h, format!("{}/", e.name), io::empty()).map_err(es)?;
        } else if let Some(t) = &e.link {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_link_name(t).map_err(es)?;
            b.append_data(&mut h, &e.name, io::empty()).map_err(es)?;
        } else {
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(e.size);
            b.append_data(&mut h, &e.name, counting(&e.path, prog, cancel)?).map_err(es)?;
        }
    }
    b.finish().map_err(es)
}

/// Wraps `file` in the chosen compressor, runs `body` on it and closes everything properly.
fn with_encoder(c: Codec, level: u32, file: fs::File, body: &mut dyn FnMut(&mut dyn Write) -> Result<(), String>) -> Result<(), String> {
    let out = io::BufWriter::with_capacity(1 << 20, file);
    match c {
        Codec::None => {
            let mut w = out;
            body(&mut w)?;
            w.flush().map_err(es)
        }
        Codec::Gz => {
            let mut e = flate2::write::GzEncoder::new(out, flate2::Compression::new(level.min(9)));
            body(&mut e)?;
            e.finish().map_err(es)?.flush().map_err(es)
        }
        Codec::Bz2 => {
            let mut e = bzip2::write::BzEncoder::new(out, bzip2::Compression::new(level.clamp(1, 9)));
            body(&mut e)?;
            e.finish().map_err(es)?.flush().map_err(es)
        }
        Codec::Xz => {
            let mut e = lzma_rust2::XzWriter::new(out, lzma_rust2::XzOptions::with_preset(level.min(9))).map_err(es)?;
            body(&mut e)?;
            e.finish().map_err(es)?.flush().map_err(es)
        }
        Codec::Zst => {
            let mut e = zstd::stream::write::Encoder::new(out, zstd_level(level)).map_err(es)?;
            body(&mut e)?;
            e.finish().map_err(es)?.flush().map_err(es)
        }
    }
}

fn write_pack(p: &Pack, o: &Opts, tmp: &Path, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    match o.fmt {
        Fmt::Zip => write_zip(p, o, tmp, prog, cancel),
        Fmt::SevenZ => write_7z(p, o, tmp, prog, cancel),
        f if f.tar_based() => {
            let file = fs::File::create(tmp).map_err(es)?;
            with_encoder(codec(f), o.level, file, &mut |w| write_tar(w, p, prog, cancel))
        }
        f => {
            // A single file straight through the compressor.
            let e = p.entries.iter().find(|e| !e.is_dir).ok_or_else(|| tr("no hay ningún archivo que comprimir").to_string())?;
            let file = fs::File::create(tmp).map_err(es)?;
            with_encoder(codec(f), o.level, file, &mut |w| {
                let mut r = counting(&e.path, prog, cancel)?;
                io::copy(&mut r, w).map(|_| ()).map_err(es)
            })
        }
    }
}

/// Reads the finished package back completely; returns how many entries it holds.
fn verify(path: &Path, o: &Opts) -> Result<usize, String> {
    let sink = &mut io::sink();
    let open = || fs::File::open(path).map_err(es);
    let count_tar = |r: &mut dyn Read| -> Result<usize, String> {
        let mut a = tar::Archive::new(r);
        let mut n = 0;
        for e in a.entries().map_err(es)? {
            let mut e = e.map_err(es)?;
            io::copy(&mut e, &mut io::sink()).map_err(es)?;
            n += 1;
        }
        Ok(n)
    };
    match o.fmt {
        Fmt::Zip => {
            let mut z = zip::ZipArchive::new(io::BufReader::new(open()?)).map_err(es)?;
            for i in 0..z.len() {
                let mut f = if o.password.is_empty() { z.by_index(i).map_err(es)? } else { z.by_index_decrypt(i, o.password.as_bytes()).map_err(es)? };
                io::copy(&mut f, sink).map_err(es)?;
            }
            Ok(z.len())
        }
        Fmt::SevenZ => {
            let mut r = sevenz_rust2::ArchiveReader::new(open()?, sevenz_rust2::Password::new(&o.password)).map_err(es)?;
            let mut n = 0;
            r.for_each_entries(|_, rd| {
                io::copy(rd, &mut io::sink())?;
                n += 1;
                Ok(true)
            })
            .map_err(es)?;
            Ok(n)
        }
        Fmt::Tar => count_tar(&mut io::BufReader::new(open()?)),
        Fmt::TarGz => count_tar(&mut flate2::read::GzDecoder::new(io::BufReader::new(open()?))),
        Fmt::TarBz2 => count_tar(&mut bzip2::read::BzDecoder::new(io::BufReader::new(open()?))),
        Fmt::TarXz => count_tar(&mut lzma_rust2::XzReader::new(io::BufReader::new(open()?), true)),
        Fmt::TarZst => count_tar(&mut zstd::stream::read::Decoder::new(open()?).map_err(es)?),
        Fmt::Gz => io::copy(&mut flate2::read::GzDecoder::new(io::BufReader::new(open()?)), sink).map(|_| 1).map_err(es),
        Fmt::Bz2 => io::copy(&mut bzip2::read::BzDecoder::new(io::BufReader::new(open()?)), sink).map(|_| 1).map_err(es),
        Fmt::Xz => io::copy(&mut lzma_rust2::XzReader::new(io::BufReader::new(open()?), true), sink).map(|_| 1).map_err(es),
        Fmt::Zst => io::copy(&mut zstd::stream::read::Decoder::new(open()?).map_err(es)?, sink).map(|_| 1).map_err(es),
    }
}

fn split_file(path: &Path, part: u64) -> io::Result<Vec<PathBuf>> {
    let mut f = fs::File::open(path)?;
    let size = f.metadata()?.len();
    let n = size.div_ceil(part).max(1);
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let mut parts = vec![];
    let mut buf = vec![0u8; 1 << 20];
    for k in 1..=n {
        let p = path.with_file_name(format!("{name}.{k:03}"));
        let mut out = io::BufWriter::new(fs::File::create(&p)?);
        let mut left = part.min(size - (k - 1) * part);
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let got = f.read(&mut buf[..want])?;
            if got == 0 {
                break;
            }
            out.write_all(&buf[..got])?;
            left -= got as u64;
        }
        out.flush()?;
        parts.push(p);
    }
    fs::remove_file(path)?;
    Ok(parts)
}

#[derive(Clone, Default)]
struct PackResult {
    label: String,
    out: PathBuf,
    parts: usize,
    files: usize,
    in_bytes: u64,
    out_bytes: u64,
    error: Option<String>,
    skipped: bool,
    verified: bool,
    deleted: usize,
}

fn run_pack(p: &Pack, o: &Opts, split: Option<u64>, prog: &Progress, cancel: &AtomicBool) -> PackResult {
    let mut res = PackResult {
        label: p.label.clone(),
        out: p.out.clone(),
        files: p.entries.iter().filter(|e| !e.is_dir).count(),
        in_bytes: p.entries.iter().map(|e| e.size).sum(),
        ..Default::default()
    };
    if let Ok(mut c) = prog.current.lock() {
        *c = p.label.clone();
    }
    let name = p.out.file_name().unwrap_or_default().to_string_lossy().to_string();
    let tmp = p.out.with_file_name(format!("{name}.part"));
    if let Some(dir) = p.out.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            res.error = Some(e.to_string());
            return res;
        }
    }
    if let Err(e) = write_pack(p, o, &tmp, prog, cancel) {
        let _ = fs::remove_file(&tmp);
        res.error = Some(e);
        return res;
    }
    if p.out.exists() && fs::remove_file(&p.out).is_err() {
        let _ = fs::remove_file(&tmp);
        res.error = Some(tr("no se pudo sobrescribir el paquete existente").to_string());
        return res;
    }
    if let Err(e) = fs::rename(&tmp, &p.out) {
        let _ = fs::remove_file(&tmp);
        res.error = Some(e.to_string());
        return res;
    }
    if o.verify {
        match verify(&p.out, o) {
            Ok(n) if n >= p.entries.iter().filter(|e| !(o.fmt == Fmt::Zip || o.fmt == Fmt::SevenZ) || e.link.is_none()).count() => res.verified = true,
            Ok(n) => {
                let _ = fs::remove_file(&p.out);
                res.error = Some(trf!("la verificación encontró {} elementos y se esperaban más; el paquete se ha descartado", n));
                return res;
            }
            Err(e) => {
                let _ = fs::remove_file(&p.out);
                res.error = Some(format!("{}: {e}", tr("falló la verificación; el paquete se ha descartado")));
                return res;
            }
        }
    }
    res.out_bytes = fs::metadata(&p.out).map(|m| m.len()).unwrap_or(0);
    if let Some(part) = split {
        if res.out_bytes > part {
            match split_file(&p.out, part) {
                Ok(parts) => res.parts = parts.len(),
                Err(e) => res.error = Some(format!("{}: {e}", tr("no se pudo dividir"))),
            }
        }
    }
    if o.delete_orig && res.error.is_none() && (res.verified || !o.verify) {
        for s in &p.sources {
            if trash::delete(s).is_ok() {
                res.deleted += 1;
            }
        }
    }
    res
}

// ---------------- planning ----------------

fn expand_name(tpl: &str, stem: &str, ext: &str, folder: &str, n: usize, now: chrono::DateTime<chrono::Local>) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let token = &after[..close];
        let (name, arg) = token.split_once(':').map(|(a, b)| (a, Some(b))).unwrap_or((token, None));
        match name.trim().to_lowercase().as_str() {
            "nombre" | "name" => out.push_str(stem),
            "ext" => out.push_str(ext),
            "carpeta" | "folder" => out.push_str(folder),
            "n" => out.push_str(&n.to_string()),
            "fecha" | "date" => {
                let fmt = arg.filter(|a| !a.is_empty()).unwrap_or("%Y-%m-%d");
                let mut s = String::new();
                let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{}", now.format(fmt)));
                out.push_str(&s);
            }
            _ => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect::<String>().trim().to_string()
}

fn stem_ext(name: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "archivo".to_string())
}

fn parse_split(text: &str) -> Result<Option<u64>, String> {
    parse_size(text).map(|v| v.filter(|n| *n > 0))
}

// ---------------- messages and the tool ----------------

enum Msg {
    Done(Vec<PackResult>, Vec<String>, f32),
}

struct Job {
    rx: Receiver<Msg>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

struct Picker {
    path: PathBuf,
    input: String,
    entries: Vec<(String, PathBuf, bool)>,
    checked: BTreeSet<PathBuf>,
    hidden: bool,
}

impl Picker {
    fn new(path: PathBuf) -> Picker {
        let mut p = Picker { path: PathBuf::new(), input: String::new(), entries: vec![], checked: BTreeSet::new(), hidden: false };
        p.go(path);
        p
    }

    fn go(&mut self, p: PathBuf) {
        let p = crate::fsops::canonical(&p);
        let mut entries: Vec<(String, PathBuf, bool)> = fs::read_dir(&p)
            .map(|rd| rd.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), e.path(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))).collect())
            .unwrap_or_default();
        entries.retain(|(n, _, _)| self.hidden || !n.starts_with('.'));
        entries.sort_by_key(|(n, _, d)| (!*d, n.to_lowercase()));
        self.input = p.to_string_lossy().to_string();
        self.path = p;
        self.entries = entries;
    }
}

pub struct Compressor {
    o: Opts,
    items: Vec<PathBuf>,
    job: Option<Job>,
    results: Vec<PackResult>,
    notes: Vec<String>,
    status: Option<(String, bool)>,
    picker: Option<Picker>,
    browser: Option<Browser>,
    confirm_delete: bool,
    base: PathBuf,
}

impl Compressor {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Compressor {
        Compressor { o: Opts::default(), items: targets, job: None, results: vec![], notes: vec![], status: None, picker: None, browser: None, confirm_delete: false, base }
    }

    /// Fills the list with the folder's contents and compresses at once, for trying the tool (`--compress`).
    pub fn demo(&mut self, dir: &Path) {
        let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| !file_name(p).starts_with('.')).collect()).unwrap_or_default();
        v.sort();
        self.items = v;
        self.o.mode = 1;
        self.o.dest_mode = 1;
        self.o.dest = dir.join("_paquetes").to_string_lossy().to_string();
        self.start();
    }

    fn add_items(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.exists() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    /// What each package will be called (modes 1 and 2 need the real files, so this covers modes 0 and 1).
    fn preview(&self) -> Vec<String> {
        let o = &self.o;
        let now = chrono::Local::now();
        let ext = o.fmt.ext();
        let mut used = HashSet::new();
        let mut unique = |mut n: String| {
            let base = n.clone();
            let mut k = 2;
            while !used.insert(n.clone()) {
                n = format!("{base} ({k})");
                k += 1;
            }
            n
        };
        if self.items.is_empty() {
            return vec![];
        }
        if o.mode == 0 {
            let first = &self.items[0];
            let stem = if self.items.len() == 1 {
                let (s, e) = stem_ext(&file_name(first), first.is_dir());
                if o.keep_ext || o.fmt.single() && !e.is_empty() {
                    format!("{s}.{e}")
                } else {
                    s
                }
            } else {
                first.parent().map(file_name).unwrap_or_else(|| "Archivo".into())
            };
            let folder = first.parent().map(file_name).unwrap_or_default();
            return vec![unique(format!("{}.{ext}", expand_name(&o.name, &stem, "", &folder, 1, now)))];
        }
        self.items
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let (s, e) = stem_ext(&file_name(p), p.is_dir());
                let stem = if (o.keep_ext || o.fmt.single()) && !e.is_empty() { format!("{s}.{e}") } else { s };
                let folder = p.parent().map(file_name).unwrap_or_default();
                unique(format!("{}.{ext}", expand_name(&o.name, &stem, &e, &folder, k + 1, now)))
            })
            .collect()
    }

    fn validate(&self) -> Result<(), String> {
        let o = &self.o;
        if self.items.is_empty() {
            return Err(tr("Añade archivos o carpetas para comprimir.").to_string());
        }
        if o.fmt.single() && o.mode != 2 && self.items.iter().any(|p| p.is_dir()) {
            return Err(tr("GZ, BZ2, XZ y ZST solo comprimen archivos sueltos: quita las carpetas o usa «Un paquete por cada archivo».").to_string());
        }
        if o.fmt.single() && o.mode == 0 && self.items.len() > 1 {
            return Err(tr("GZ, BZ2, XZ y ZST comprimen un archivo cada uno: elige «Un paquete por cada elemento» o añade solo uno.").to_string());
        }
        if !o.password.is_empty() && o.password != o.password2 {
            return Err(tr("Las dos contraseñas no coinciden.").to_string());
        }
        if o.dest_mode == 1 && o.dest.trim().is_empty() {
            return Err(tr("Elige la carpeta de destino.").to_string());
        }
        parse_split(&o.split_text).map_err(|e| format!("{}: {e}", tr("Dividir en partes")))?;
        crate::rename::name_matcher(&o.include, o.pat_regex).map_err(|e| trf!("Expresión no válida: {}", e))?;
        crate::rename::name_matcher(&o.exclude, o.pat_regex).map_err(|e| trf!("Expresión no válida: {}", e))?;
        if o.delete_orig && !self.confirm_delete {
            return Err(tr("Marca la casilla para confirmar que se envíen los originales a la papelera.").to_string());
        }
        Ok(())
    }

    fn start(&mut self) {
        if let Err(e) = self.validate() {
            self.status = Some((e, true));
            return;
        }
        let o = self.o.clone();
        let items = self.items.clone();
        let split = parse_split(&o.split_text).ok().flatten();
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let (results, notes) = run_all(&items, &o, split, &p2, &c2);
            let _ = tx.send(Msg::Done(results, notes, started.elapsed().as_secs_f32()));
        });
        self.results.clear();
        self.notes.clear();
        self.status = None;
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok(Msg::Done(results, notes, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        let ok = results.iter().filter(|r| r.error.is_none() && !r.skipped).count();
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        let (i, out): (u64, u64) = results.iter().filter(|r| r.error.is_none() && !r.skipped).fold((0, 0), |(a, b), r| (a + r.in_bytes, b + r.out_bytes));
        let ratio = if i > 0 { format!("{:.0}%", out as f64 * 100.0 / i as f64) } else { "—".into() };
        let head = if cancelled { tr("Compresión cancelada") } else { tr("Compresión terminada") };
        self.status = Some((trf!("{}: {} paquete(s) · {} » {} ({}) · {} error(es) · {} s", head, ok, human_size(i), human_size(out), ratio, bad, format!("{secs:.1}")), bad > 0 || cancelled));
        self.results = results;
        self.notes = notes;
        self.confirm_delete = false;
        events.push(ToolEvent::Reload);
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add_items(dropped);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("comp_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.items_ui(ui, selection, cwd);
                ui.separator();
                self.format_ui(ui);
                self.mode_ui(ui);
                self.name_ui(ui, cwd, selection);
            });
            ui.add_enabled_ui(!busy, |ui| self.advanced_ui(ui));
            ui.separator();
            self.action_ui(ui);
            self.results_ui(ui, events);
        });
        let ctx = ui.ctx().clone();
        self.picker_ui(&ctx);
        self.browser_ui(&ctx);
    }

    fn items_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("QUÉ COMPRIMIR")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("comp_items").max_height(100.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, p) in self.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(if p.is_dir() { "📁" } else { "🗋" });
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
        ui.horizontal_wrapped(|ui| {
            let sel: Vec<PathBuf> = selection.to_vec();
            if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).on_hover_text(tr("Selecciona archivos en la ventana principal (puedes cambiar de carpeta) y pulsa aquí.")).clicked() {
                self.add_items(sel);
            }
            if ui.button(tr("📂 Examinar…")).on_hover_text(tr("Navega y marca archivos y carpetas")).clicked() {
                let start = self.items.last().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| cwd.to_path_buf());
                self.picker = Some(Picker::new(start));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
        });
    }

    fn format_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("FORMATO")).small().strong().color(theme::p().sub));
            ui.menu_button(tr("✨ Rápidos"), |ui| {
                ui.label(RichText::new(tr("Cambian formato y opciones; lo que has añadido se mantiene")).small().color(theme::p().muted));
                for (name, desc, f) in PRESETS {
                    if ui.button(tr(name)).on_hover_text(tr(desc)).clicked() {
                        f(&mut self.o);
                        ui.close_menu();
                    }
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            for (f, _, label, _) in FORMATS {
                if ui.selectable_label(self.o.fmt == f, label).clicked() && self.o.fmt != f {
                    self.o.fmt = f;
                    self.o.level = f.default_level();
                }
            }
        });
        let desc = FORMATS.iter().find(|f| f.0 == self.o.fmt).map(|f| f.3).unwrap_or("");
        ui.label(RichText::new(tr(desc)).small().color(theme::p().sub));
    }

    fn mode_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("CUÁNTOS PAQUETES")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for (k, (name, desc)) in MODES.iter().enumerate() {
                ui.selectable_value(&mut self.o.mode, k as u8, tr(name)).on_hover_text(tr(desc));
            }
        });
        ui.label(RichText::new(tr(MODES[self.o.mode as usize].1)).small().color(theme::p().sub));
    }

    fn name_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.label(RichText::new(tr("NOMBRE Y DESTINO")).small().strong().color(theme::p().sub));
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Nombre"));
            ui.add(egui::TextEdit::singleline(&mut self.o.name).desired_width(260.0).font(egui::TextStyle::Monospace)).on_hover_text(tr("Admite {nombre}, {fecha}, {fecha:%d-%m-%Y}, {n}, {carpeta} y {ext}"));
            for code in ["{nombre}", "{fecha}", "{n}"] {
                if ui.small_button(code).clicked() {
                    self.o.name.push_str(code);
                }
            }
            if !self.o.fmt.single() {
                ui.checkbox(&mut self.o.keep_ext, tr("Conservar la extensión original")).on_hover_text(tr("foto.jpg » foto.jpg.zip en vez de foto.zip"));
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Guardar en"));
            ui.selectable_value(&mut self.o.dest_mode, 0, tr("Junto al original"));
            ui.selectable_value(&mut self.o.dest_mode, 1, tr("Otra carpeta"));
            if self.o.dest_mode == 1 {
                ui.add(egui::TextEdit::singleline(&mut self.o.dest).hint_text(tr("carpeta destino")).desired_width(280.0));
                if ui.button(tr("📂 Examinar…")).clicked() {
                    let cur = PathBuf::from(self.o.dest.trim());
                    self.browser = Some(Browser::new(if cur.is_dir() { cur } else { dirs::home_dir().unwrap_or_else(|| self.base.clone()) }, 0));
                }
                if let Some(p) = quick_menu(ui, cwd, selection) {
                    self.o.dest = p.to_string_lossy().to_string();
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Si ya existe"));
            for (k, l) in CONFLICTS.iter().enumerate() {
                ui.selectable_value(&mut self.o.conflict, k as u8, tr(l));
            }
        });
        let names = self.preview();
        if self.o.mode == 2 {
            ui.label(RichText::new(tr("Se creará un paquete por cada archivo, con su mismo nombre.")).small().color(theme::p().sub));
        } else if !names.is_empty() {
            let shown: Vec<&str> = names.iter().take(4).map(String::as_str).collect();
            let more = if names.len() > 4 { trf!(" y {} más", names.len() - 4) } else { String::new() };
            ui.label(RichText::new(trf!("Se creará: {}{}", shown.join(", "), more)).small().color(theme::p().sub));
        }
    }

    fn advanced_ui(&mut self, ui: &mut egui::Ui) {
        let fmt = self.o.fmt;
        egui::CollapsingHeader::new(tr("⚙ Compresión")).default_open(false).show(ui, |ui| {
            if fmt.has_level() {
                ui.horizontal(|ui| {
                    crate::ui_kit::label(ui, tr("Nivel de compresión"));
                    ui.add(egui::Slider::new(&mut self.o.level, 0..=9).show_value(true));
                    let hint = match self.o.level {
                        0 => "sin comprimir",
                        1..=3 => "rápido, ocupa más",
                        4..=6 => "equilibrado",
                        _ => "máxima compresión, tarda más",
                    };
                    ui.label(RichText::new(tr(hint)).small().color(theme::p().sub));
                });
            }
            if fmt == Fmt::Zip {
                ui.label(RichText::new(tr("Método")).small().strong().color(theme::p().sub));
                for (k, l) in ZIP_METHODS.iter().enumerate() {
                    ui.selectable_value(&mut self.o.zip_method, k as u8, tr(l));
                }
                ui.checkbox(&mut self.o.store_compressed, tr("No recomprimir lo que ya está comprimido (jpg, mp4, zip, pdf…)"))
                    .on_hover_text(tr("Esos archivos se guardan tal cual: va más rápido y casi no cambia el tamaño."));
            }
            if self.o.mode != 0 {
                ui.horizontal(|ui| {
                    crate::ui_kit::label(ui, tr("Paquetes a la vez"));
                    ui.add(egui::DragValue::new(&mut self.o.threads).range(1..=16));
                    ui.label(RichText::new(tr("(en paralelo)")).small().color(theme::p().sub));
                });
            }
            size_field(ui, tr("Dividir en partes"), &mut self.o.split_text, tr("no dividir"));
            ui.label(RichText::new(tr("Con partes (.001, .002…) puedes enviar paquetes grandes por correo; se vuelven a unir con la herramienta «Unir archivos».")).small().color(theme::p().muted));
        });
        egui::CollapsingHeader::new(tr("🔎 Qué incluir")).default_open(false).show(ui, |ui| {
            let hint_inc = if self.o.pat_regex { "\\.(jpe?g|png)$" } else { "*.jpg, *.png" };
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Solo"));
                ui.add(egui::TextEdit::singleline(&mut self.o.include).hint_text(hint_inc).desired_width(300.0)).on_hover_text(tr("Dentro de las carpetas, incluye solo los archivos que coincidan (vacío = todos)"));
            });
            ui.horizontal(|ui| {
                crate::ui_kit::label(ui, tr("Excluir"));
                ui.add(egui::TextEdit::singleline(&mut self.o.exclude).hint_text(if self.o.pat_regex { "^\\.git$" } else { "*.tmp, node_modules, .git" }).desired_width(300.0)).on_hover_text(tr("Archivos y carpetas (con todo lo que contienen) que no se incluyen"));
            });
            ui.checkbox(&mut self.o.pat_regex, tr("Los filtros son expresiones regulares"));
            ui.checkbox(&mut self.o.hidden, tr("Incluir archivos y carpetas ocultos"));
            ui.checkbox(&mut self.o.root_folder, tr("Incluir la carpeta raíz dentro del paquete")).on_hover_text(tr("Con ella: fotos/a.jpg. Sin ella: a.jpg directamente."));
            ui.checkbox(&mut self.o.follow_links, tr("Seguir los enlaces simbólicos")).on_hover_text(tr("Si no, en TAR se guardan como enlaces y en ZIP y 7z se omiten."));
        });
        egui::CollapsingHeader::new(tr("🔒 Seguridad y extras")).default_open(false).show(ui, |ui| {
            if fmt.password() {
                ui.horizontal(|ui| {
                    crate::ui_kit::label(ui, tr("Contraseña"));
                    ui.add(egui::TextEdit::singleline(&mut self.o.password).password(!self.o.show_pw).desired_width(200.0));
                    ui.add(egui::TextEdit::singleline(&mut self.o.password2).password(!self.o.show_pw).hint_text(tr("repetir")).desired_width(200.0));
                    ui.checkbox(&mut self.o.show_pw, tr("mostrar"));
                });
                ui.label(RichText::new(tr(if fmt == Fmt::SevenZ { "Cifrado AES-256; en 7z también se ocultan los nombres de los archivos. Si la olvidas no hay forma de recuperarlo." } else { "Cifrado AES-256 (lo abren 7-Zip, Keka y la mayoría de programas actuales; el ZIP clásico del sistema puede no abrirlo). Si la olvidas no hay forma de recuperarlo." })).small().color(theme::p().muted));
            } else {
                self.o.password.clear();
                self.o.password2.clear();
                ui.label(RichText::new(tr("La contraseña solo está disponible en ZIP y 7z.")).small().color(theme::p().muted));
            }
            if fmt == Fmt::Zip {
                ui.horizontal(|ui| {
                    crate::ui_kit::label(ui, tr("Comentario"));
                    ui.add(egui::TextEdit::singleline(&mut self.o.comment).desired_width(400.0));
                });
            }
            ui.checkbox(&mut self.o.verify, tr("Verificar el paquete al terminar")).on_hover_text(tr("Lo lee entero para comprobar que no está dañado. Si falla, se descarta. Recomendado."));
            ui.checkbox(&mut self.o.delete_orig, tr("Enviar los originales a la papelera al terminar"))
                .on_hover_text(tr("Solo si el paquete se creó (y se verificó) sin errores."));
            if self.o.delete_orig {
                ui.checkbox(&mut self.confirm_delete, RichText::new(tr("Confirmo que quiero que los originales vayan a la papelera.")).color(theme::p().danger));
            }
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("📦 Comprimir")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.items.is_empty(), btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let (b, t) = (p.bytes.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                let frac = if t > 0 { b as f32 / t as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac.min(1.0)).desired_width(200.0).show_percentage());
                let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
                let secs = job.started.elapsed().as_secs_f64().max(0.1);
                ui.label(RichText::new(format!("{stage} · {}/{} · {}/s", p.packs_done.load(Ordering::Relaxed), p.packs_total.load(Ordering::Relaxed), human_size((b as f64 / secs) as u64))).small());
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
        if self.results.is_empty() && self.notes.is_empty() {
            return;
        }
        ui.separator();
        egui::ScrollArea::vertical().id_salt("comp_res").max_height(crate::tools::grow(ui, 240.0)).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("comp_grid").striped(true).num_columns(5).spacing([14.0, 4.0]).show(ui, |ui| {
                for r in &self.results {
                    let name = r.out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| r.label.clone());
                    if ui.link(name).on_hover_text(tr("Ir a la carpeta")).clicked() {
                        if let Some(p) = r.out.parent() {
                            events.push(ToolEvent::Navigate(p.to_path_buf()));
                        }
                    }
                    ui.label(RichText::new(trf!("{} archivos", r.files)).small().color(theme::p().sub));
                    if r.error.is_some() || r.skipped {
                        ui.label("");
                    } else {
                        let ratio = if r.in_bytes > 0 { format!("{:.0}%", r.out_bytes as f64 * 100.0 / r.in_bytes as f64) } else { "—".into() };
                        ui.label(RichText::new(format!("{} » {} ({ratio})", human_size(r.in_bytes), human_size(r.out_bytes))).small());
                    }
                    let (txt, col) = match (&r.error, r.skipped) {
                        (Some(e), _) => (format!("✖ {e}"), theme::p().danger),
                        (_, true) => (tr("omitido: ya existe").to_string(), theme::p().muted),
                        _ => {
                            let mut t = String::from("✔");
                            if r.verified {
                                t.push(' ');
                                t.push_str(tr("verificado"));
                            }
                            if r.parts > 0 {
                                t.push_str(&format!(" · {} {}", r.parts, tr("partes")));
                            }
                            if r.deleted > 0 {
                                t.push_str(&format!(" · {}", tr("original a la papelera")));
                            }
                            (t, theme::p().ok)
                        }
                    };
                    ui.label(RichText::new(txt).small().color(col));
                    ui.end_row();
                }
            });
            for n in &self.notes {
                ui.label(RichText::new(n).small().color(theme::p().muted));
            }
        });
    }

    fn picker_ui(&mut self, ctx: &egui::Context) {
        let Some(mut pk) = self.picker.take() else { return };
        let mut open = true;
        let mut nav: Option<PathBuf> = None;
        let mut add = false;
        let mut close = false;
        egui::Window::new(tr("Elegir archivos y carpetas")).id(egui::Id::new("comp_picker")).open(&mut open).default_width(520.0).default_height(480.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(pk.path.parent().is_some(), egui::Button::new("⬆")).on_hover_text(tr("Carpeta superior")).clicked() {
                    nav = pk.path.parent().map(Path::to_path_buf);
                }
                if ui.button("🏠").on_hover_text(tr("Carpeta personal")).clicked() {
                    nav = dirs::home_dir();
                }
                let r = ui.add(egui::TextEdit::singleline(&mut pk.input).desired_width(ui.available_width() - 90.0));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    nav = Some(PathBuf::from(pk.input.trim()));
                }
                if ui.checkbox(&mut pk.hidden, tr("ocultos")).changed() {
                    nav = Some(pk.path.clone());
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 330.0)).auto_shrink([false, false]).show(ui, |ui| {
                let all: Vec<PathBuf> = pk.entries.iter().map(|e| e.1.clone()).collect();
                for (name, path, is_dir) in &pk.entries {
                    ui.horizontal(|ui| {
                        let mut on = pk.checked.contains(path);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                pk.checked.insert(path.clone());
                            } else {
                                pk.checked.remove(path);
                            }
                        }
                        if *is_dir {
                            if ui.link(format!("📁 {name}")).on_hover_text(tr("Abrir")).clicked() {
                                nav = Some(path.clone());
                            }
                        } else {
                            ui.label(format!("🗋 {name}"));
                        }
                    });
                }
                let _ = all;
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.small_button(tr("Marcar todo")).clicked() {
                    let all: Vec<PathBuf> = pk.entries.iter().map(|e| e.1.clone()).collect();
                    pk.checked.extend(all);
                }
                if ui.small_button(tr("Desmarcar todo")).clicked() {
                    pk.checked.clear();
                }
                if ui.add_enabled(!pk.checked.is_empty(), egui::Button::new(trf!("➕ Añadir {} marcado(s)", pk.checked.len()))).clicked() {
                    add = true;
                }
                if ui.button(tr("Cerrar")).clicked() {
                    close = true;
                }
            });
        });
        if let Some(p) = nav.filter(|p| p.is_dir()) {
            pk.go(p);
        }
        if add {
            let v: Vec<PathBuf> = pk.checked.iter().cloned().collect();
            self.add_items(v);
            pk.checked.clear();
        }
        if open && !close {
            self.picker = Some(pk);
        }
    }

    fn browser_ui(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.browser.take() else { return };
        let mut open = true;
        let (mut nav, mut pick): (Option<PathBuf>, Option<PathBuf>) = (None, None);
        let mut close = false;
        egui::Window::new(tr("Elegir carpeta")).id(egui::Id::new("comp_browser")).open(&mut open).default_width(480.0).default_height(440.0).show(ctx, |ui| {
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
        if let Some(p) = pick {
            self.o.dest = p.to_string_lossy().to_string();
            return;
        }
        if open && !close {
            self.browser = Some(b);
        }
    }
}

// ---------------- the run ----------------

/// Builds the packages from the items and runs them (several at once when there are many).
fn run_all(items: &[PathBuf], o: &Opts, split: Option<u64>, prog: &Progress, cancel: &AtomicBool) -> (Vec<PackResult>, Vec<String>) {
    let set = |s: &str| {
        if let Ok(mut st) = prog.stage.lock() {
            *st = s.to_string();
        }
    };
    set(tr("Preparando"));
    let now = chrono::Local::now();
    let mut notes = vec![];
    let mut early: Vec<PackResult> = vec![];
    let include = crate::rename::name_matcher(&o.include, o.pat_regex).ok().flatten();
    let exclude = crate::rename::name_matcher(&o.exclude, o.pat_regex).ok().flatten();
    let mut filter = Filter { include, exclude, hidden: o.hidden, follow: o.follow_links, tar: o.fmt.tar_based(), skip: HashSet::new() };

    // Where each package goes, and what it will be called.
    let ext = o.fmt.ext();
    let dest = (o.dest_mode == 1).then(|| PathBuf::from(o.dest.trim()));
    let mut used: HashSet<PathBuf> = HashSet::new();
    let mut resolve = |dir: PathBuf, base: String, early: &mut Vec<PackResult>, label: &str| -> Option<PathBuf> {
        let mut out = dir.join(format!("{base}.{ext}"));
        let mut k = 2;
        loop {
            let taken = used.contains(&out) || (o.conflict == 0 && out.exists());
            if !taken {
                break;
            }
            out = dir.join(format!("{base} ({k}).{ext}"));
            k += 1;
        }
        if o.conflict == 2 && out.exists() {
            early.push(PackResult { label: label.to_string(), out: out.clone(), skipped: true, ..Default::default() });
            return None;
        }
        used.insert(out.clone());
        Some(out)
    };

    struct Plan {
        out: PathBuf,
        label: String,
        items: Vec<(PathBuf, String)>,
        files_only: bool,
        single_file: Option<(PathBuf, String)>,
    }
    let mut plans: Vec<Plan> = vec![];
    let first_dir = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let name_of = |tpl_stem: &str, ext_: &str, folder: &str, n: usize| expand_name(&o.name, tpl_stem, ext_, folder, n, now);
    let mut top_used: HashSet<String> = HashSet::new();
    let mut top_name = |name: String| -> String {
        let mut n = name.clone();
        let mut k = 2;
        while !top_used.insert(n.clone()) {
            let (s, e) = stem_ext(&name, false);
            n = if e.is_empty() { format!("{s} ({k})") } else { format!("{s} ({k}).{e}") };
            k += 1;
        }
        n
    };
    match o.mode {
        0 => {
            let first = &items[0];
            let stem = if items.len() == 1 {
                let (s, e) = stem_ext(&file_name(first), first.is_dir());
                if (o.keep_ext || o.fmt.single()) && !e.is_empty() {
                    format!("{s}.{e}")
                } else {
                    s
                }
            } else {
                first.parent().map(file_name).unwrap_or_else(|| "Archivo".into())
            };
            let folder = first.parent().map(file_name).unwrap_or_default();
            let dir = dest.clone().unwrap_or_else(|| first_dir(first));
            if let Some(out) = resolve(dir, name_of(&stem, "", &folder, 1), &mut early, &stem) {
                plans.push(Plan { out, label: stem, items: items.iter().map(|p| (p.clone(), top_name(file_name(p)))).collect(), files_only: false, single_file: None });
            }
        }
        1 => {
            for (k, p) in items.iter().enumerate() {
                let (s, e) = stem_ext(&file_name(p), p.is_dir());
                let stem = if (o.keep_ext || o.fmt.single()) && !e.is_empty() { format!("{s}.{e}") } else { s };
                let folder = p.parent().map(file_name).unwrap_or_default();
                let dir = dest.clone().unwrap_or_else(|| first_dir(p));
                if let Some(out) = resolve(dir, name_of(&stem, &e, &folder, k + 1), &mut early, &stem) {
                    plans.push(Plan { out, label: stem, items: vec![(p.clone(), file_name(p))], files_only: false, single_file: None });
                }
            }
        }
        _ => {
            let mut n = 0;
            for p in items {
                if p.is_dir() {
                    let mut w = Walker { f: &filter, out: vec![], errs: vec![], links_skipped: 0, cancel };
                    w.item(p, &file_name(p), false, true);
                    notes.extend(w.errs);
                    for e in w.out {
                        n += 1;
                        let rel_parent = Path::new(&e.name).parent().map(Path::to_path_buf).unwrap_or_default();
                        let dir = match &dest {
                            Some(d) => d.join(file_name(p)).join(&rel_parent),
                            None => e.path.parent().map(Path::to_path_buf).unwrap_or_default(),
                        };
                        let fname = file_name(&e.path);
                        let (s, ex) = stem_ext(&fname, false);
                        let stem = if (o.keep_ext || o.fmt.single()) && !ex.is_empty() { format!("{s}.{ex}") } else { s };
                        let folder = e.path.parent().map(file_name).unwrap_or_default();
                        if let Some(out) = resolve(dir, name_of(&stem, &ex, &folder, n), &mut early, &stem) {
                            plans.push(Plan { out, label: fname.clone(), items: vec![], files_only: true, single_file: Some((e.path.clone(), fname)) });
                        }
                    }
                } else {
                    n += 1;
                    let fname = file_name(p);
                    let (s, ex) = stem_ext(&fname, false);
                    let stem = if (o.keep_ext || o.fmt.single()) && !ex.is_empty() { format!("{s}.{ex}") } else { s };
                    let folder = p.parent().map(file_name).unwrap_or_default();
                    let dir = dest.clone().unwrap_or_else(|| first_dir(p));
                    if let Some(out) = resolve(dir, name_of(&stem, &ex, &folder, n), &mut early, &stem) {
                        plans.push(Plan { out, label: fname.clone(), items: vec![], files_only: true, single_file: Some((p.clone(), fname)) });
                    }
                }
            }
        }
    }

    // Never put a package inside itself.
    for pl in &plans {
        let name = file_name(&pl.out);
        filter.skip.insert(pl.out.clone());
        filter.skip.insert(pl.out.with_file_name(format!("{name}.part")));
    }
    set(tr("Leyendo carpetas"));
    let mut packs: Vec<Pack> = vec![];
    let mut links = 0;
    for pl in plans {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut w = Walker { f: &filter, out: vec![], errs: vec![], links_skipped: 0, cancel };
        let mut sources = vec![];
        if let Some((path, name)) = &pl.single_file {
            w.item(path, name, true, true);
            sources.push(path.clone());
        } else {
            for (path, top) in &pl.items {
                let include_root = o.root_folder || !path.is_dir();
                w.item(path, top, include_root, false);
                sources.push(path.clone());
            }
        }
        let _ = pl.files_only;
        links += w.links_skipped;
        notes.extend(w.errs);
        if o.fmt.single() && (w.out.len() != 1 || w.out[0].is_dir) {
            early.push(PackResult { label: pl.label.clone(), out: pl.out.clone(), error: Some(tr("este formato solo comprime un archivo suelto").to_string()), ..Default::default() });
            continue;
        }
        if w.out.is_empty() {
            early.push(PackResult { label: pl.label.clone(), out: pl.out.clone(), error: Some(tr("no hay nada que comprimir (todo quedó excluido por los filtros)").to_string()), ..Default::default() });
            continue;
        }
        packs.push(Pack { out: pl.out, label: pl.label, entries: w.out, sources });
    }
    if links > 0 {
        notes.push(trf!("{} enlace(s) simbólico(s) omitido(s)", links));
    }
    prog.total.store(packs.iter().flat_map(|p| p.entries.iter()).map(|e| e.size).sum(), Ordering::Relaxed);
    prog.packs_total.store(packs.len(), Ordering::Relaxed);
    set(tr("Comprimiendo"));

    // Several packages at once.
    let results: Mutex<Vec<(usize, PackResult)>> = Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    let workers = if packs.len() > 1 { o.threads.clamp(1, 16).min(packs.len()) } else { 1 };
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= packs.len() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = run_pack(&packs[i], o, split, prog, cancel);
                prog.packs_done.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut v) = results.lock() {
                    v.push((i, r));
                }
            });
        }
    });
    let mut done = results.into_inner().unwrap_or_default();
    done.sort_by_key(|(i, _)| *i);
    let mut all = early;
    all.extend(done.into_iter().map(|(_, r)| r));
    (all, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("datos/sub")).unwrap();
        fs::write(dir.join("datos/a.txt"), "hola ".repeat(2000)).unwrap();
        fs::write(dir.join("datos/sub/b.txt"), "adios ".repeat(1000)).unwrap();
        fs::write(dir.join("datos/foto.jpg"), vec![7u8; 3000]).unwrap();
        fs::write(dir.join("datos/.oculto"), "x").unwrap();
        fs::write(dir.join("suelto.txt"), "suelto ".repeat(500)).unwrap();
        dir
    }

    fn run(dir: &Path, o: &Opts, items: &[&str]) -> Vec<PackResult> {
        let items: Vec<PathBuf> = items.iter().map(|i| dir.join(i)).collect();
        run_all(&items, o, parse_split(&o.split_text).unwrap(), &Progress::default(), &AtomicBool::new(false)).0
    }

    #[test]
    fn every_format_round_trips_and_verifies() {
        let dir = setup("formats");
        for (f, ext, _, _) in FORMATS {
            let mut o = Opts { fmt: f, level: f.default_level(), threads: 2, ..Opts::default() };
            o.dest_mode = 1;
            o.dest = dir.join(format!("out-{ext}")).to_string_lossy().to_string();
            let items: Vec<&str> = if f.single() { vec!["suelto.txt"] } else { vec!["datos", "suelto.txt"] };
            if f.single() {
                o.mode = 1;
            }
            let r = run(&dir, &o, &items);
            assert_eq!(r.len(), 1, "{ext}: {:?}", r.iter().map(|r| r.error.clone()).collect::<Vec<_>>());
            assert!(r[0].error.is_none(), "{ext}: {:?}", r[0].error);
            assert!(r[0].verified && r[0].out.exists(), "{ext}");
            assert!(r[0].out.to_string_lossy().ends_with(&format!(".{ext}")));
            if !f.single() {
                assert!(r[0].files >= 4, "{ext}: hidden files included by default ({})", r[0].files);
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_package_per_item_and_per_file_with_filters() {
        let dir = setup("modes");
        let mut o = Opts { mode: 1, ..Opts::default() };
        let r = run(&dir, &o, &["datos", "suelto.txt"]);
        assert_eq!(r.len(), 2);
        assert!(dir.join("datos.zip").exists() && dir.join("suelto.zip").exists());
        // Running again: the names are taken, so they get a number.
        let r = run(&dir, &o, &["datos"]);
        assert!(r[0].out.ends_with("datos (2).zip"));
        o.conflict = 2;
        assert!(run(&dir, &o, &["datos"])[0].skipped);
        // One package per file, hidden files and excluded names left out, next to each original.
        let mut o = Opts { mode: 2, hidden: false, exclude: "*.jpg".into(), fmt: Fmt::TarGz, ..Opts::default() };
        o.conflict = 0;
        let r = run(&dir, &o, &["datos"]);
        let names: BTreeSet<String> = r.iter().map(|r| file_name(&r.out)).collect();
        assert_eq!(names, BTreeSet::from(["a.txt.tar.gz".to_string(), "b.txt.tar.gz".to_string()]).iter().map(|n| n.replace(".txt.tar.gz", ".tar.gz")).collect::<BTreeSet<_>>());
        assert!(dir.join("datos/sub/b.tar.gz").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn password_split_and_delete_originals() {
        let dir = setup("extras");
        let mut o = Opts { fmt: Fmt::Zip, level: 0, password: "secreto".into(), password2: "secreto".into(), split_text: "4 KB".into(), mode: 1, delete_orig: true, ..Opts::default() };
        o.store_compressed = false;
        let r = run(&dir, &o, &["datos"]);
        assert!(r[0].error.is_none(), "{:?}", r[0].error);
        assert!(r[0].parts >= 2, "split into parts: {}", r[0].parts);
        assert!(dir.join("datos.zip.001").exists() && !dir.join("datos.zip").exists());
        assert!(!dir.join("datos").exists(), "the original went to the trash after verifying");
        // 7z with a password verifies by reading it back with that password.
        let dir2 = setup("seven");
        let o = Opts { fmt: Fmt::SevenZ, password: "clave".into(), password2: "clave".into(), ..Opts::default() };
        let r = run(&dir2, &o, &["datos"]);
        assert!(r[0].error.is_none() && r[0].verified, "{:?}", r[0].error);
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&dir2).unwrap();
    }

    /// Writes one package of each format to /tmp/comp-check so other programs can test them.
    #[test]
    #[ignore]
    fn write_samples_for_external_tools() {
        let dir = setup("samples");
        let out = PathBuf::from("/tmp/comp-check");
        let _ = fs::remove_dir_all(&out);
        for (f, _, _, _) in FORMATS {
            let mut o = Opts { fmt: f, level: f.default_level(), dest_mode: 1, dest: out.to_string_lossy().to_string(), ..Opts::default() };
            if f.single() {
                o.mode = 1;
            }
            if f == Fmt::Zip {
                o.name = "{nombre}-pw".into();
                o.password = "pw".into();
                o.password2 = "pw".into();
                run(&dir, &o, &["datos", "suelto.txt"]);
                o.password.clear();
                o.password2.clear();
                o.name = "{nombre}".into();
            }
            run(&dir, &o, if f.single() { &["suelto.txt"] } else { &["datos", "suelto.txt"] });
        }
    }

    #[test]
    fn names_sizes_and_validation() {
        let now = chrono::Local::now();
        assert_eq!(expand_name("{nombre}-{n}", "foto", "jpg", "carpeta", 3, now), "foto-3");
        assert_eq!(expand_name("a/b:{nombre}", "x", "", "", 1, now), "a-b-x");
        assert_eq!(parse_size("20 MB").unwrap(), Some(20 * 1024 * 1024));
        let tmp = std::env::temp_dir();
        let mut c = Compressor::new(vec![tmp.clone()], tmp);
        c.o.fmt = Fmt::Gz;
        assert!(c.validate().is_err(), "a folder can't go into a single-file format");
        c.o.fmt = Fmt::Zip;
        c.o.password = "a".into();
        assert!(c.validate().is_err(), "passwords must match");
        assert!(already_compressed("a.JPG") && !already_compressed("a.txt"));
    }
}
