//! Metadata of files: see the EXIF of photos and the tags of music, edit the common ones in
//! many files at once (author, copyright, camera, capture date; title, artist, album…), and
//! remove them (everything, or only the GPS location) before sharing. Every changed file can
//! keep a .bak copy.

use crate::checksum::cell;
use crate::fsops;
use crate::i18n::tr;
use crate::sources::Sources;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use little_exif::exif_tag::ExifTag;
use little_exif::ifd::ExifTagGroup;
use little_exif::metadata::Metadata as ExifMeta;
use lofty::prelude::*;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

const INCLUDE: &str = "*.jpg, *.jpeg, *.png, *.tif, *.tiff, *.webp, *.heic, *.avif, *.mp3, *.flac, *.m4a, *.ogg, *.opus, *.wav, *.aiff";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Jpeg,
    Png,
    OtherImage,
    Audio,
    Other,
}

fn kind_of(p: &Path) -> Kind {
    match p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default().as_str() {
        "jpg" | "jpeg" => Kind::Jpeg,
        "png" => Kind::Png,
        "tif" | "tiff" | "webp" | "heic" | "heif" | "avif" | "jxl" => Kind::OtherImage,
        "mp3" | "flac" | "m4a" | "mp4" | "ogg" | "opus" | "wav" | "aiff" | "aif" | "wma" | "ape" | "wv" => Kind::Audio,
        _ => Kind::Other,
    }
}

fn is_image(k: Kind) -> bool {
    matches!(k, Kind::Jpeg | Kind::Png | Kind::OtherImage)
}

// ---------------------------------------------------------------- reading

#[derive(Clone, Default)]
struct Summary {
    what: String,
    taken: String,
    gps: bool,
    has_meta: bool,
}

fn read_exif(path: &Path) -> Option<exif::Exif> {
    let f = fs::File::open(path).ok()?;
    exif::Reader::new().read_from_container(&mut BufReader::new(f)).ok()
}

fn summary(path: &Path) -> Summary {
    let mut s = Summary::default();
    let k = kind_of(path);
    if is_image(k) {
        if let Some(ex) = read_exif(path) {
            s.has_meta = true;
            let get = |t: exif::Tag| ex.get_field(t, exif::In::PRIMARY).map(|f| f.display_value().to_string().trim_matches('"').trim().to_string()).filter(|v| !v.is_empty());
            s.what = [get(exif::Tag::Make), get(exif::Tag::Model)].into_iter().flatten().collect::<Vec<_>>().join(" ");
            s.taken = get(exif::Tag::DateTimeOriginal).unwrap_or_default();
            s.gps = ex.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).is_some();
        }
    } else if k == Kind::Audio {
        if let Ok(tf) = lofty::read_from_path(path) {
            if let Some(t) = tf.primary_tag().or_else(|| tf.first_tag()) {
                s.has_meta = true;
                s.what = [t.artist().map(|a| a.to_string()), t.title().map(|a| a.to_string())].into_iter().flatten().collect::<Vec<_>>().join(" — ");
                s.taken = t.year().map(|y| y.to_string()).unwrap_or_default();
            }
        }
    }
    s
}

/// Every tag of the file as (name, value), for the details view.
fn read_all(path: &Path) -> Vec<(String, String)> {
    let k = kind_of(path);
    let mut out = vec![];
    if is_image(k) {
        if let Some(ex) = read_exif(path) {
            for f in ex.fields() {
                let v = f.display_value().with_unit(&ex).to_string();
                out.push((f.tag.to_string(), if v.len() > 300 { format!("{}…", v.chars().take(300).collect::<String>()) } else { v }));
            }
        }
    } else if k == Kind::Audio {
        if let Ok(tf) = lofty::read_from_path(path) {
            let p = tf.properties();
            out.push((tr("Duración").to_string(), format!("{}:{:02}", p.duration().as_secs() / 60, p.duration().as_secs() % 60)));
            if let Some(b) = p.audio_bitrate() {
                out.push((tr("Tasa de bits").to_string(), format!("{b} kbps")));
            }
            if let Some(r) = p.sample_rate() {
                out.push((tr("Frecuencia").to_string(), format!("{r} Hz")));
            }
            for tag in tf.tags() {
                for item in tag.items() {
                    let v = match item.value() {
                        lofty::tag::ItemValue::Text(t) | lofty::tag::ItemValue::Locator(t) => t.clone(),
                        lofty::tag::ItemValue::Binary(b) => format!("({} bytes)", b.len()),
                    };
                    out.push((format!("{:?}", item.key()), if v.len() > 300 { format!("{}…", v.chars().take(300).collect::<String>()) } else { v }));
                }
                for pic in tag.pictures() {
                    out.push((tr("Imagen incrustada").to_string(), format!("{} bytes", pic.data().len())));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- editing

/// Fields the person may change: (id, label).
const IMAGE_FIELDS: [(&str, &str); 7] = [
    ("desc", "Descripción"),
    ("artist", "Autor"),
    ("copyright", "Copyright"),
    ("software", "Programa"),
    ("make", "Marca de la cámara"),
    ("model", "Modelo de la cámara"),
    ("date", "Fecha de captura"),
];
const AUDIO_FIELDS: [(&str, &str); 7] = [("title", "Título"), ("artist", "Artista"), ("album", "Álbum"), ("year", "Año"), ("track", "Pista"), ("genre", "Género"), ("comment", "Comentario")];

#[derive(Clone, Default)]
struct Edits {
    /// (id, new text); an empty text clears the field.
    set: Vec<(String, String)>,
}

/// "2024-05-31 18:30:00" (or with «/» or «:») as EXIF's "2024:05:31 18:30:00".
fn exif_date(s: &str) -> Result<String, String> {
    let t = s.trim().replace(['-', '/'], ":").replace('T', " ");
    let ok = t.len() == 19 && t.chars().enumerate().all(|(i, c)| if matches!(i, 4 | 7 | 13 | 16) { c == ':' } else if i == 10 { c == ' ' } else { c.is_ascii_digit() });
    if ok {
        Ok(t)
    } else {
        Err(tr("la fecha debe ser AAAA-MM-DD HH:MM:SS").to_string())
    }
}

fn edit_image(path: &Path, e: &Edits) -> Result<(), String> {
    let mut m = ExifMeta::new_from_path(path).unwrap_or_else(|_| ExifMeta::new());
    for (id, v) in &e.set {
        let v = v.trim().to_string();
        match id.as_str() {
            "desc" => set_or_clear(&mut m, v.clone(), ExifTag::ImageDescription(v.clone()), ExifTag::ImageDescription(String::new())),
            "artist" => set_or_clear(&mut m, v.clone(), ExifTag::Artist(v.clone()), ExifTag::Artist(String::new())),
            "copyright" => set_or_clear(&mut m, v.clone(), ExifTag::Copyright(v.clone()), ExifTag::Copyright(String::new())),
            "software" => set_or_clear(&mut m, v.clone(), ExifTag::Software(v.clone()), ExifTag::Software(String::new())),
            "make" => set_or_clear(&mut m, v.clone(), ExifTag::Make(v.clone()), ExifTag::Make(String::new())),
            "model" => set_or_clear(&mut m, v.clone(), ExifTag::Model(v.clone()), ExifTag::Model(String::new())),
            "date" => {
                if v.is_empty() {
                    m.remove_tag(ExifTag::DateTimeOriginal(String::new()));
                    m.remove_tag(ExifTag::CreateDate(String::new()));
                } else {
                    let d = exif_date(&v)?;
                    m.set_tag(ExifTag::DateTimeOriginal(d.clone()));
                    m.set_tag(ExifTag::CreateDate(d));
                }
            }
            _ => {}
        }
    }
    m.write_to_file(path).map_err(|e| e.to_string())
}

fn set_or_clear(m: &mut ExifMeta, v: String, set: ExifTag, clear: ExifTag) {
    if v.is_empty() {
        m.remove_tag(clear);
    } else {
        m.set_tag(set);
    }
}

fn edit_audio(path: &Path, e: &Edits) -> Result<(), String> {
    use lofty::config::WriteOptions;
    let mut tf = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    if tf.primary_tag().is_none() && tf.first_tag().is_none() {
        let t = lofty::tag::Tag::new(tf.primary_tag_type());
        tf.insert_tag(t);
    }
    let tag = tf.primary_tag_mut().or_else(|| None);
    let tag = match tag {
        Some(t) => t,
        None => return Err(tr("este formato no admite etiquetas").to_string()),
    };
    for (id, v) in &e.set {
        let v = v.trim();
        let num = v.parse::<u32>().ok();
        match id.as_str() {
            "title" => if v.is_empty() { tag.remove_title() } else { tag.set_title(v.to_string()) },
            "artist" => if v.is_empty() { tag.remove_artist() } else { tag.set_artist(v.to_string()) },
            "album" => if v.is_empty() { tag.remove_album() } else { tag.set_album(v.to_string()) },
            "genre" => if v.is_empty() { tag.remove_genre() } else { tag.set_genre(v.to_string()) },
            "comment" => if v.is_empty() { tag.remove_comment() } else { tag.set_comment(v.to_string()) },
            "year" => match (v.is_empty(), num) {
                (true, _) => tag.remove_year(),
                (_, Some(n)) => tag.set_year(n),
                _ => return Err(tr("el año debe ser un número").to_string()),
            },
            "track" => match (v.is_empty(), num) {
                (true, _) => tag.remove_track(),
                (_, Some(n)) => tag.set_track(n),
                _ => return Err(tr("la pista debe ser un número").to_string()),
            },
            _ => {}
        }
    }
    tf.save_to_path(path, WriteOptions::default()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- removing

/// JPEG without its metadata segments: EXIF and XMP (APP1), Photoshop/IPTC (APP13) and comments.
/// Colour profiles and everything that draws the picture are kept untouched.
fn strip_jpeg(b: &[u8]) -> Result<Vec<u8>, String> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return Err(tr("no es un JPEG válido").to_string());
    }
    let mut out = Vec::with_capacity(b.len());
    out.extend_from_slice(&b[..2]);
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return Err(tr("JPEG dañado").to_string());
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            out.extend_from_slice(&b[i..]);
            return Ok(out);
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if len < 2 || i + 2 + len > b.len() {
            return Err(tr("JPEG dañado").to_string());
        }
        let seg = &b[i..i + 2 + len];
        let body = &seg[4..];
        let drop = match marker {
            0xE1 => body.starts_with(b"Exif\0\0") || body.starts_with(b"http://ns.adobe.com/xap/1.0/") || body.starts_with(b"http://ns.adobe.com/xmp/extension/"),
            0xED | 0xFE => true,
            _ => false,
        };
        if !drop {
            out.extend_from_slice(seg);
        }
        i += 2 + len;
    }
    Err(tr("JPEG dañado").to_string())
}

/// PNG without text and EXIF chunks (tEXt, zTXt, iTXt, eXIf, tIME).
fn strip_png(b: &[u8]) -> Result<Vec<u8>, String> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if b.len() < 8 || b[..8] != SIG {
        return Err(tr("no es un PNG válido").to_string());
    }
    let mut out = b[..8].to_vec();
    let mut i = 8;
    while i + 12 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let end = i + 12 + len;
        if end > b.len() {
            return Err(tr("PNG dañado").to_string());
        }
        let name = &b[i + 4..i + 8];
        if !matches!(name, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME") {
            out.extend_from_slice(&b[i..end]);
        }
        i = end;
    }
    Ok(out)
}

fn orientation_of(path: &Path) -> u16 {
    read_exif(path).and_then(|ex| ex.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0))).unwrap_or(1) as u16
}

#[derive(Clone, Copy)]
struct CleanOpts {
    gps_only: bool,
    keep_orientation: bool,
}

fn clean_file(path: &Path, o: CleanOpts) -> Result<(), String> {
    let k = kind_of(path);
    if is_image(k) && o.gps_only {
        // A picture with no EXIF has no location to remove.
        let Ok(mut m) = ExifMeta::new_from_path(path) else { return Ok(()) };
        for id in 0..=0x1f {
            m.remove_tag_by_hex_group(id, ExifTagGroup::GPS);
        }
        return m.write_to_file(path).map_err(|e| e.to_string());
    }
    if o.gps_only {
        return Err(tr("solo las fotos tienen ubicación GPS").to_string());
    }
    let orientation = if o.keep_orientation && is_image(k) { orientation_of(path) } else { 1 };
    match k {
        Kind::Jpeg | Kind::Png => {
            let bytes = fs::read(path).map_err(|e| e.to_string())?;
            let new = if k == Kind::Jpeg { strip_jpeg(&bytes)? } else { strip_png(&bytes)? };
            if new.len() != bytes.len() {
                write_atomic(path, &new)?;
            }
        }
        Kind::OtherImage => {
            ExifMeta::new().write_to_file(path).map_err(|e| e.to_string())?;
        }
        Kind::Audio => {
            let tf = lofty::read_from_path(path).map_err(|e| e.to_string())?;
            let types: Vec<lofty::tag::TagType> = tf.tags().iter().map(|t| t.tag_type()).collect();
            for t in types {
                t.remove_from_path(path).map_err(|e| e.to_string())?;
            }
        }
        Kind::Other => return Err(tr("tipo de archivo no admitido").to_string()),
    }
    if orientation > 1 {
        let mut m = ExifMeta::new_from_path(path).unwrap_or_else(|_| ExifMeta::new());
        m.set_tag(ExifTag::Orientation(vec![orientation]));
        m.write_to_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let tmp = path.with_file_name(format!(".{name}.part"));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if let Ok(m) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, m.permissions());
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

fn backup(path: &Path) -> Result<(), String> {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    fs::copy(path, fsops::unique_name(&dir, &format!("{name}.bak"))).map(|_| ()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- the tool

#[derive(Clone)]
struct Row {
    path: PathBuf,
    kind: Kind,
    sum: Summary,
    on: bool,
    result: Option<Result<(), String>>,
}

enum Msg {
    Rows(Vec<Row>),
    Done(Vec<(PathBuf, Result<(), String>)>),
}

pub struct MetaTool {
    src: Sources,
    tab: u8,
    rows: Vec<Row>,
    filter: String,
    view_cache: crate::tools::ViewCache,
    selected: Option<usize>,
    details: Vec<(String, String)>,
    image_fields: Vec<(bool, String)>,
    audio_fields: Vec<(bool, String)>,
    gps_only: bool,
    keep_orientation: bool,
    backup: bool,
    rx: Option<Receiver<Msg>>,
    progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    status: Option<(String, bool)>,
}

impl MetaTool {
    pub fn new(targets: Vec<PathBuf>) -> MetaTool {
        let mut src = Sources::new("meta_src", targets);
        src.include = INCLUDE.into();
        src.exclude_dirs.clear();
        MetaTool {
            src,
            tab: 0,
            rows: vec![],
            filter: String::new(),
            view_cache: Default::default(),
            selected: None,
            details: vec![],
            image_fields: vec![(false, String::new()); IMAGE_FIELDS.len()],
            audio_fields: vec![(false, String::new()); AUDIO_FIELDS.len()],
            gps_only: false,
            keep_orientation: true,
            backup: true,
            rx: None,
            progress: Arc::new(AtomicUsize::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            status: None,
        }
    }

    /// Reads the files of a folder at once, for trying the tool (`--meta`).
    pub fn demo(&mut self, dir: &Path) {
        self.src.items = vec![dir.to_path_buf()];
        self.scan();
    }

    fn scan(&mut self) {
        if self.src.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas.").to_string(), true));
            return;
        }
        let Ok(filters) = self.src.filters() else {
            self.status = Some((tr("Algún patrón no es válido.").to_string(), true));
            return;
        };
        let (items, recursive, hidden) = (self.src.items.clone(), self.src.recursive, self.src.hidden);
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let progress = self.progress.clone();
        progress.store(0, Ordering::Relaxed);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (files, _) = crate::sources::collect_files(&items, &filters, recursive, hidden, &cancel);
            let rows: Vec<Row> = files
                .into_iter()
                .filter(|f| kind_of(f) != Kind::Other)
                .take_while(|_| !cancel.load(Ordering::Relaxed))
                .map(|f| {
                    progress.fetch_add(1, Ordering::Relaxed);
                    Row { kind: kind_of(&f), sum: summary(&f), path: f, on: true, result: None }
                })
                .collect();
            let _ = tx.send(Msg::Rows(rows));
        });
        self.rows.clear();
        self.selected = None;
        self.details.clear();
        self.status = None;
        self.rx = Some(rx);
    }

    fn run_edit(&mut self) {
        let mk = |fields: &[(bool, String)], ids: &[(&str, &str)]| Edits { set: ids.iter().zip(fields).filter(|(_, f)| f.0).map(|((id, _), f)| (id.to_string(), f.1.clone())).collect() };
        let (img, aud) = (mk(&self.image_fields, &IMAGE_FIELDS), mk(&self.audio_fields, &AUDIO_FIELDS));
        if img.set.is_empty() && aud.set.is_empty() {
            self.status = Some((tr("Marca al menos un campo para cambiar.").to_string(), true));
            return;
        }
        let jobs: Vec<(PathBuf, Kind)> = self.rows.iter().filter(|r| r.on).map(|r| (r.path.clone(), r.kind)).collect();
        let bak = self.backup;
        self.spawn_job(jobs, move |p, k| {
            if bak {
                backup(p)?;
            }
            if is_image(k) {
                if img.set.is_empty() {
                    return Err(tr("sin campos de imagen marcados").to_string());
                }
                edit_image(p, &img)
            } else {
                if aud.set.is_empty() {
                    return Err(tr("sin campos de audio marcados").to_string());
                }
                edit_audio(p, &aud)
            }
        });
    }

    fn run_clean(&mut self) {
        let o = CleanOpts { gps_only: self.gps_only, keep_orientation: self.keep_orientation };
        let jobs: Vec<(PathBuf, Kind)> = self.rows.iter().filter(|r| r.on).map(|r| (r.path.clone(), r.kind)).collect();
        let bak = self.backup;
        self.spawn_job(jobs, move |p, _| {
            if bak {
                backup(p)?;
            }
            clean_file(p, o)
        });
    }

    fn spawn_job(&mut self, jobs: Vec<(PathBuf, Kind)>, f: impl Fn(&Path, Kind) -> Result<(), String> + Send + 'static) {
        if jobs.is_empty() {
            self.status = Some((tr("No hay archivos marcados.").to_string(), true));
            return;
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let progress = self.progress.clone();
        progress.store(0, Ordering::Relaxed);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(jobs.len());
            for (p, k) in jobs {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = f(&p, k);
                progress.fetch_add(1, Ordering::Relaxed);
                out.push((p, r));
            }
            let _ = tx.send(Msg::Done(out));
        });
        self.status = None;
        self.rx = Some(rx);
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(rx) = &self.rx else { return };
        let Ok(msg) = rx.try_recv() else { return };
        self.rx = None;
        match msg {
            Msg::Rows(rows) => {
                self.status = Some((trf!("{} archivo(s) con metadatos posibles", rows.len()), false));
                self.rows = rows;
            }
            Msg::Done(res) => {
                let (ok, bad) = (res.iter().filter(|r| r.1.is_ok()).count(), res.iter().filter(|r| r.1.is_err()).count());
                for (p, r) in res {
                    if let Some(row) = self.rows.iter_mut().find(|row| row.path == p) {
                        if r.is_ok() {
                            row.sum = summary(&p);
                        }
                        row.result = Some(r);
                    }
                }
                if let Some(k) = self.selected {
                    if let Some(r) = self.rows.get(k) {
                        self.details = read_all(&r.path);
                    }
                }
                self.status = Some((trf!("Listo: {} archivo(s) cambiados · {} con error", ok, bad), bad > 0));
                events.push(ToolEvent::Reload);
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let busy = self.rx.is_some();
        ui.add_enabled_ui(!busy, |ui| self.src.ui(ui, selection, cwd, tr("FOTOS Y MÚSICA")));
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🔍 Leer metadatos")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy, btn).clicked() {
                self.scan();
            }
            if busy {
                ui.spinner();
                ui.label(RichText::new(format!("{}", self.progress.load(Ordering::Relaxed))).small());
                if ui.button(tr("Cancelar")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if self.rows.is_empty() {
            return;
        }
        ui.separator();
        self.table_ui(ui);
        if let Some(k) = self.selected {
            if !self.details.is_empty() {
                let name = self.rows.get(k).map(|r| r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or_default();
                egui::CollapsingHeader::new(RichText::new(trf!("🗒 Todos los metadatos de «{}» ({})", name, self.details.len()))).default_open(true).show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("meta_details").max_height(crate::tools::grow(ui, 170.0)).auto_shrink([false, true]).show_rows(ui, 20.0, self.details.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            let (k, v) = &self.details[i];
                            ui.horizontal(|ui| {
                                cell(ui, 220.0, 20.0, |ui| ui.label(RichText::new(k).small().color(theme::p().sub)));
                                ui.add(egui::Label::new(RichText::new(v).small().monospace()).truncate());
                            });
                        }
                    });
                });
            } else {
                ui.label(RichText::new(tr("Este archivo no tiene metadatos.")).small().color(theme::p().muted));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, 0, tr("✏ Editar"));
            ui.selectable_value(&mut self.tab, 1, tr("🧹 Quitar metadatos"));
        });
        ui.add_enabled_ui(!busy, |ui| match self.tab {
            0 => self.edit_ui(ui),
            _ => self.clean_ui(ui),
        });
    }

    fn table_ui(&mut self, ui: &mut egui::Ui) {
        let with_gps = self.rows.iter().filter(|r| r.sum.gps).count();
        let marked = self.rows.iter().filter(|r| r.on).count();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(trf!("{} archivo(s) · {} marcado(s)", self.rows.len(), marked)).strong());
            if with_gps > 0 {
                ui.label(RichText::new(trf!("📍 {} con ubicación GPS", with_gps)).color(theme::p().danger));
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Filtrar")).desired_width(150.0));
            if ui.small_button(tr("Marcar todos")).clicked() {
                self.rows.iter_mut().for_each(|r| r.on = true);
            }
            if ui.small_button(tr("Desmarcar todos")).clicked() {
                self.rows.iter_mut().for_each(|r| r.on = false);
            }
            if with_gps > 0 && ui.small_button(tr("Marcar solo los que tienen GPS")).clicked() {
                self.rows.iter_mut().for_each(|r| r.on = r.sum.gps);
            }
        });
        let q = self.filter.to_lowercase();
        let salt = self.rows.len() as u64;
        let view = self.view_cache.get(&self.filter, salt, || self.rows.iter().enumerate().filter(|(_, r)| q.is_empty() || r.path.to_string_lossy().to_lowercase().contains(&q) || r.sum.what.to_lowercase().contains(&q)).map(|(i, _)| i).collect());
        let mut click = None;
        egui::ScrollArea::vertical().id_salt("meta_rows").max_height(crate::tools::grow(ui, 200.0)).auto_shrink([false, false]).show_rows(ui, 22.0, view.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &i in &view[range] {
                let selected = self.selected == Some(i);
                let r = &mut self.rows[i];
                ui.horizontal(|ui| {
                    ui.set_height(22.0);
                    if selected {
                        ui.painter().rect_filled(ui.max_rect(), 4.0, theme::accent().gamma_multiply(0.15));
                    }
                    ui.checkbox(&mut r.on, "");
                    let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let l = cell(ui, 230.0, 22.0, |ui| ui.add(egui::Label::new(RichText::new(name)).truncate().sense(egui::Sense::click()))).on_hover_text(r.path.to_string_lossy());
                    if l.clicked() {
                        click = Some(i);
                    }
                    cell(ui, 18.0, 22.0, |ui| ui.label(if is_image(r.kind) { "🖼" } else { "🎵" }));
                    cell(ui, 260.0, 22.0, |ui| ui.add(egui::Label::new(RichText::new(if r.sum.has_meta { r.sum.what.clone() } else { tr("sin metadatos").to_string() }).small().color(if r.sum.has_meta { theme::p().text } else { theme::p().muted })).truncate()));
                    cell(ui, 140.0, 22.0, |ui| ui.label(RichText::new(&r.sum.taken).small().color(theme::p().sub)));
                    if r.sum.gps {
                        ui.label(RichText::new("📍").color(theme::p().danger)).on_hover_text(tr("Contiene la ubicación GPS donde se hizo"));
                    }
                    match &r.result {
                        Some(Ok(())) => {
                            ui.label(RichText::new(tr("✔ hecho")).small().color(theme::p().ok));
                        }
                        Some(Err(e)) => {
                            ui.add(egui::Label::new(RichText::new(format!("✖ {e}")).small().color(theme::p().danger)).truncate());
                        }
                        None => {}
                    }
                });
            }
        });
        if let Some(i) = click {
            self.selected = Some(i);
            self.details = read_all(&self.rows[i].path);
            // Start the edit fields from this file's values.
            if let Some(ex) = read_exif(&self.rows[i].path) {
                let get = |t: exif::Tag| ex.get_field(t, exif::In::PRIMARY).map(|f| f.display_value().to_string().trim_matches('"').trim().to_string()).unwrap_or_default();
                let vals = [get(exif::Tag::ImageDescription), get(exif::Tag::Artist), get(exif::Tag::Copyright), get(exif::Tag::Software), get(exif::Tag::Make), get(exif::Tag::Model), String::new()];
                for (f, v) in self.image_fields.iter_mut().zip(vals) {
                    if !f.0 {
                        f.1 = v;
                    }
                }
                // "2024-05-31 18:30:00": only the date part uses dashes.
                if !self.image_fields[6].0 {
                    let d = get(exif::Tag::DateTimeOriginal);
                    if d.len() == 19 {
                        self.image_fields[6].1 = format!("{} {}", d[..10].replace(':', "-"), &d[11..]);
                    }
                }
            }
            if self.rows[i].kind == Kind::Audio {
                if let Ok(tf) = lofty::read_from_path(&self.rows[i].path) {
                    if let Some(t) = tf.primary_tag().or_else(|| tf.first_tag()) {
                        let vals = [t.title().map(|s| s.to_string()), t.artist().map(|s| s.to_string()), t.album().map(|s| s.to_string()), t.year().map(|y| y.to_string()), t.track().map(|y| y.to_string()), t.genre().map(|s| s.to_string()), t.comment().map(|s| s.to_string())];
                        for (f, v) in self.audio_fields.iter_mut().zip(vals) {
                            if !f.0 {
                                f.1 = v.unwrap_or_default();
                            }
                        }
                    }
                }
            }
        }
    }

    fn edit_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("Marca los campos que quieres cambiar en los archivos marcados. Un campo marcado y vacío se borra. Haz clic en un archivo para partir de sus valores.")).small().color(theme::p().muted));
        let cols = ui.available_width() > 900.0;
        let show = |ui: &mut egui::Ui, title: &str, ids: &[(&str, &str)], fields: &mut Vec<(bool, String)>| {
            ui.label(RichText::new(title).small().strong().color(theme::p().sub));
            egui::Grid::new(title).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                for ((id, label), f) in ids.iter().zip(fields.iter_mut()) {
                    ui.checkbox(&mut f.0, tr(label));
                    let hint = if *id == "date" { "2024-05-31 18:30:00" } else { "" };
                    ui.add_enabled(f.0, egui::TextEdit::singleline(&mut f.1).hint_text(hint).desired_width(if cols { 320.0 } else { 260.0 }));
                    ui.end_row();
                }
            });
        };
        if cols {
            ui.columns(2, |c| {
                show(&mut c[0], tr("FOTOS"), &IMAGE_FIELDS, &mut self.image_fields);
                show(&mut c[1], tr("MÚSICA"), &AUDIO_FIELDS, &mut self.audio_fields);
            });
        } else {
            show(ui, tr("FOTOS"), &IMAGE_FIELDS, &mut self.image_fields);
            show(ui, tr("MÚSICA"), &AUDIO_FIELDS, &mut self.audio_fields);
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.backup, tr("Guardar una copia .bak de cada archivo"));
            let n = self.rows.iter().filter(|r| r.on).count();
            let btn = egui::Button::new(RichText::new(trf!("✏ Aplicar a {} archivo(s)", n)).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(n > 0, btn).clicked() {
                self.run_edit();
            }
        });
    }

    fn clean_ui(&mut self, ui: &mut egui::Ui) {
        ui.selectable_value(&mut self.gps_only, false, tr("Quitar todo: EXIF, XMP, IPTC, comentarios (y las etiquetas de la música)"));
        ui.selectable_value(&mut self.gps_only, true, tr("Quitar solo la ubicación GPS de las fotos"));
        ui.add_enabled(!self.gps_only, egui::Checkbox::new(&mut self.keep_orientation, tr("Conservar la orientación de la foto"))).on_hover_text(tr("Sin ella, algunas fotos de móvil se verían giradas."));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.backup, tr("Guardar una copia .bak de cada archivo"));
            let n = self.rows.iter().filter(|r| r.on).count();
            let btn = egui::Button::new(RichText::new(trf!("🧹 Limpiar {} archivo(s)", n)).color(Color32::WHITE).strong()).fill(theme::p().danger);
            if ui.add_enabled(n > 0, btn).clicked() {
                self.run_clean();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("meta-{n}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn photo(path: &Path) {
        let img = image::RgbImage::from_fn(16, 8, |x, y| image::Rgb([(x * 10) as u8, (y * 20) as u8, 90]));
        img.save(path).unwrap();
    }

    #[test]
    fn exif_dates_are_checked_and_reformatted() {
        assert_eq!(exif_date("2024-05-31 18:30:00").unwrap(), "2024:05:31 18:30:00");
        assert_eq!(exif_date("2024/05/31T18:30:00").unwrap(), "2024:05:31 18:30:00");
        assert!(exif_date("31/05/2024").is_err() && exif_date("").is_err());
    }

    #[test]
    fn edits_jpeg_exif_then_strips_it_keeping_the_picture_and_orientation() {
        let d = dir("jpg");
        let p = d.join("f.jpg");
        photo(&p);
        let e = Edits { set: vec![("artist".into(), "Ana".into()), ("copyright".into(), "© Ana".into()), ("make".into(), "Canon".into()), ("date".into(), "2024-05-31 18:30:00".into())] };
        edit_image(&p, &e).unwrap();
        let s = summary(&p);
        assert!(s.has_meta && s.what.contains("Canon") && s.taken.contains("2024"), "{:?} {}", s.what, s.taken);
        let all = read_all(&p);
        assert!(all.iter().any(|(k, v)| k == "Artist" && v.contains("Ana")), "{all:?}");
        // Clearing one field removes just it.
        edit_image(&p, &Edits { set: vec![("make".into(), String::new())] }).unwrap();
        assert!(!summary(&p).what.contains("Canon"));
        assert!(edit_image(&p, &Edits { set: vec![("date".into(), "mal".into())] }).is_err());
        // Orientation survives a full strip when asked.
        let mut m = ExifMeta::new_from_path(&p).unwrap_or_else(|_| ExifMeta::new());
        m.set_tag(ExifTag::Orientation(vec![6]));
        m.write_to_file(&p).unwrap();
        clean_file(&p, CleanOpts { gps_only: false, keep_orientation: true }).unwrap();
        let all = read_all(&p);
        assert!(!all.iter().any(|(k, _)| k == "Artist" || k == "Copyright"), "{all:?}");
        assert_eq!(orientation_of(&p), 6);
        assert_eq!(image::open(&p).unwrap().width(), 16, "still a valid picture");
        clean_file(&p, CleanOpts { gps_only: false, keep_orientation: false }).unwrap();
        assert!(read_all(&p).is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn strips_png_text_and_refuses_bad_files() {
        let d = dir("png");
        let p = d.join("f.png");
        photo(&p);
        let mut bytes = fs::read(&p).unwrap();
        // Add a tEXt chunk after the signature+IHDR (8 + 25 bytes).
        let mut chunk = vec![];
        let body = b"Author\0Ana";
        chunk.extend((body.len() as u32).to_be_bytes());
        chunk.extend(b"tEXt");
        chunk.extend(body);
        chunk.extend(crc32fast::hash(&[b"tEXt".as_slice(), body].concat()).to_be_bytes());
        bytes.splice(33..33, chunk);
        let stripped = strip_png(&bytes).unwrap();
        assert!(stripped.len() < bytes.len() && !stripped.windows(4).any(|w| w == b"tEXt"));
        fs::write(&p, &stripped).unwrap();
        assert!(image::open(&p).is_ok());
        assert!(strip_png(b"no").is_err() && strip_jpeg(b"no").is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn removes_only_gps_and_edits_and_clears_audio_tags() {
        let d = dir("gps");
        let p = d.join("f.jpg");
        photo(&p);
        let mut m = ExifMeta::new_from_path(&p).unwrap_or_else(|_| ExifMeta::new());
        m.set_tag(ExifTag::Artist("Ana".into()));
        m.set_tag(ExifTag::GPSLatitudeRef("N".into()));
        m.set_tag(ExifTag::GPSLatitude(vec![little_exif::rational::uR64 { nominator: 40, denominator: 1 }, little_exif::rational::uR64 { nominator: 25, denominator: 1 }, little_exif::rational::uR64 { nominator: 0, denominator: 1 }]));
        m.write_to_file(&p).unwrap();
        assert!(summary(&p).gps);
        clean_file(&p, CleanOpts { gps_only: true, keep_orientation: false }).unwrap();
        let s = summary(&p);
        assert!(!s.gps && read_all(&p).iter().any(|(k, _)| k == "Artist"), "GPS gone, the rest kept");
        // Audio: a tiny WAV (RIFF INFO tags) written, read, cleared.
        let w = d.join("a.wav");
        let mut data = vec![];
        let pcm = vec![0u8; 800];
        data.extend(b"RIFF");
        data.extend((36 + pcm.len() as u32).to_le_bytes());
        data.extend(b"WAVEfmt ");
        data.extend(16u32.to_le_bytes());
        data.extend([1, 0, 1, 0]);
        data.extend(8000u32.to_le_bytes());
        data.extend(8000u32.to_le_bytes());
        data.extend([1, 0, 8, 0]);
        data.extend(b"data");
        data.extend((pcm.len() as u32).to_le_bytes());
        data.extend(&pcm);
        fs::write(&w, data).unwrap();
        edit_audio(&w, &Edits { set: vec![("title".into(), "Canción".into()), ("artist".into(), "Ana".into()), ("year".into(), "2020".into())] }).unwrap();
        let s = summary(&w);
        assert!(s.has_meta && s.what == "Ana — Canción", "{:?}", s.what);
        assert!(edit_audio(&w, &Edits { set: vec![("year".into(), "x".into())] }).is_err());
        clean_file(&w, CleanOpts { gps_only: false, keep_orientation: true }).unwrap();
        assert!(!summary(&w).has_meta);
        assert!(clean_file(&w, CleanOpts { gps_only: true, keep_orientation: false }).is_err());
        fs::remove_dir_all(&d).unwrap();
    }
}
