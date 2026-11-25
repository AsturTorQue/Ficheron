//! Images: convert between PNG, JPEG, WebP, BMP, GIF and TIFF; resize (fit, width, height,
//! percent, exact, fill and crop), rotate and flip, turn the camera's orientation into real
//! pixels, grayscale; naming template; several images at once.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::{picker_window, DestField, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use image::imageops::FilterType;
use image::DynamicImage;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const EXTS: [&str; 9] = ["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "ico"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fmt {
    Keep,
    Png,
    Jpeg,
    WebP,
    Bmp,
    Gif,
    Tiff,
    Avif,
}

const FORMATS: [(Fmt, &str, &str); 8] = [
    (Fmt::Keep, "Mismo formato", ""),
    (Fmt::Png, "PNG", "png"),
    (Fmt::Jpeg, "JPEG", "jpg"),
    (Fmt::WebP, "WebP (sin pérdida)", "webp"),
    (Fmt::Bmp, "BMP", "bmp"),
    (Fmt::Gif, "GIF", "gif"),
    (Fmt::Tiff, "TIFF", "tif"),
    (Fmt::Avif, "AVIF", "avif"),
];

#[derive(Clone, Copy, PartialEq)]
enum Resize {
    None,
    Fit,
    Width,
    Height,
    Percent,
    Exact,
    Fill,
}

const RESIZES: [(Resize, &str, &str); 7] = [
    (Resize::None, "Sin cambiar el tamaño", ""),
    (Resize::Fit, "Ajustar a una caja", "Cabe dentro del ancho y alto máximos, sin deformar."),
    (Resize::Width, "Por ancho", "El ancho que indiques; el alto sale solo."),
    (Resize::Height, "Por alto", "El alto que indiques; el ancho sale solo."),
    (Resize::Percent, "Porcentaje", "Por ejemplo 50 % para la mitad."),
    (Resize::Exact, "Tamaño exacto", "Ancho y alto exactos; la imagen se deforma si no coinciden las proporciones."),
    (Resize::Fill, "Rellenar y recortar", "Ancho y alto exactos: se escala y se recorta lo que sobra por el centro."),
];

const FILTERS: [(&str, &str); 4] = [("Lanczos (mejor calidad)", "lanczos"), ("Bicúbico", "cubic"), ("Bilineal (rápido)", "linear"), ("Vecino más cercano (píxeles nítidos)", "nearest")];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

type Preset = (&'static str, &'static str, fn(&mut Opts));

const PRESETS: [Preset; 7] = [
    ("🌐 Para web", "Cabe en 1920 × 1920, JPEG con calidad 80.", |o| {
        o.fmt = Fmt::Jpeg;
        o.resize = Resize::Fit;
        o.w = 1920;
        o.h = 1920;
        o.quality = 80;
        o.no_enlarge = true;
    }),
    ("🖼 Miniaturas", "256 × 256 como máximo, JPEG.", |o| {
        o.fmt = Fmt::Jpeg;
        o.resize = Resize::Fit;
        o.w = 256;
        o.h = 256;
        o.quality = 80;
        o.name = "{nombre}-mini".into();
    }),
    ("📱 Redes sociales", "Cuadradas de 1080 × 1080, recortadas por el centro.", |o| {
        o.fmt = Fmt::Jpeg;
        o.resize = Resize::Fill;
        o.w = 1080;
        o.h = 1080;
        o.quality = 85;
    }),
    ("➗ Reducir a la mitad", "Al 50 % del tamaño original.", |o| {
        o.resize = Resize::Percent;
        o.percent = 50;
    }),
    ("🔄 Convertir a PNG", "Sin cambiar el tamaño.", |o| {
        o.fmt = Fmt::Png;
        o.resize = Resize::None;
    }),
    ("🔄 Convertir a JPEG", "Calidad 90; lo transparente se vuelve blanco.", |o| {
        o.fmt = Fmt::Jpeg;
        o.resize = Resize::None;
        o.quality = 90;
    }),
    ("⚫ Blanco y negro", "Pasa a escala de grises.", |o| o.gray = true),
];

#[derive(Clone)]
struct Opts {
    fmt: Fmt,
    resize: Resize,
    w: u32,
    h: u32,
    percent: u32,
    no_enlarge: bool,
    filter: usize,
    quality: u8,
    png_best: bool,
    auto_orient: bool,
    rotate: u8,
    flip_h: bool,
    flip_v: bool,
    gray: bool,
    name: String,
    dest_other: bool,
    dest: PathBuf,
    conflict: u8,
    recursive: bool,
    threads: usize,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            fmt: Fmt::Keep,
            resize: Resize::Fit,
            w: 1920,
            h: 1080,
            percent: 50,
            no_enlarge: true,
            filter: 0,
            quality: 85,
            png_best: false,
            auto_orient: true,
            rotate: 0,
            flip_h: false,
            flip_v: false,
            gray: false,
            name: "{nombre}-{ancho}x{alto}".into(),
            dest_other: false,
            dest: PathBuf::new(),
            conflict: 0,
            recursive: false,
            threads: std::thread::available_parallelism().map(|n| n.get().min(4)).unwrap_or(2),
        }
    }
}

fn is_image(p: &Path) -> bool {
    p.extension().map(|e| EXTS.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

/// The size an image will have.
fn target_dims(ow: u32, oh: u32, o: &Opts) -> (u32, u32) {
    let clamp = |v: f64| (v.round() as u32).max(1);
    let (ow_f, oh_f) = (ow as f64, oh as f64);
    let (w, h) = (o.w.max(1) as f64, o.h.max(1) as f64);
    let mut scale = match o.resize {
        Resize::None => return (ow, oh),
        Resize::Fit => (w / ow_f).min(h / oh_f),
        Resize::Width => w / ow_f,
        Resize::Height => h / oh_f,
        Resize::Percent => o.percent.max(1) as f64 / 100.0,
        Resize::Exact | Resize::Fill => return if o.no_enlarge && (o.w > ow || o.h > oh) && o.resize == Resize::Fill { (ow.min(o.w.max(1)), oh.min(o.h.max(1))) } else { (o.w.max(1), o.h.max(1)) },
    };
    if o.no_enlarge && scale > 1.0 {
        scale = 1.0;
    }
    (clamp(ow_f * scale), clamp(oh_f * scale))
}

fn filter_of(i: usize) -> FilterType {
    match i {
        1 => FilterType::CatmullRom,
        2 => FilterType::Triangle,
        3 => FilterType::Nearest,
        _ => FilterType::Lanczos3,
    }
}

fn orientation(path: &Path) -> u32 {
    let Ok(f) = fs::File::open(path) else { return 1 };
    exif::Reader::new()
        .read_from_container(&mut BufReader::new(f))
        .ok()
        .and_then(|ex| ex.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)))
        .unwrap_or(1)
}

fn apply_orientation(img: DynamicImage, o: u32) -> DynamicImage {
    match o {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Puts transparent areas over white, for formats that can't keep them.
fn flatten(img: &DynamicImage) -> DynamicImage {
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        let a = p[3] as u32;
        for c in 0..3 {
            p[c] = ((p[c] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).to_rgb8())
}

fn process(img: DynamicImage, path: &Path, o: &Opts) -> DynamicImage {
    let mut img = if o.auto_orient { apply_orientation(img, orientation(path)) } else { img };
    img = match o.rotate {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    };
    if o.flip_h {
        img = img.fliph();
    }
    if o.flip_v {
        img = img.flipv();
    }
    let (tw, th) = target_dims(img.width(), img.height(), o);
    if (tw, th) != (img.width(), img.height()) {
        let f = filter_of(o.filter);
        img = if o.resize == Resize::Fill { img.resize_to_fill(tw, th, f) } else { img.resize_exact(tw, th, f) };
    }
    if o.gray {
        img = img.grayscale();
    }
    img
}

fn ext_for(fmt: Fmt, src: &Path) -> String {
    FORMATS.iter().find(|f| f.0 == fmt && fmt != Fmt::Keep).map(|f| f.2.to_string()).unwrap_or_else(|| src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "png".into()))
}

fn encode(img: &DynamicImage, fmt: Fmt, src: &Path, o: &Opts, out: &mut BufWriter<fs::File>) -> Result<(), String> {
    let fmt = if fmt == Fmt::Keep {
        match src.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
            Some("jpg" | "jpeg") => Fmt::Jpeg,
            Some("webp") => Fmt::WebP,
            Some("bmp") => Fmt::Bmp,
            Some("gif") => Fmt::Gif,
            Some("tif" | "tiff") => Fmt::Tiff,
            _ => Fmt::Png,
        }
    } else {
        fmt
    };
    let e = |x: image::ImageError| x.to_string();
    match fmt {
        Fmt::Jpeg => {
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut *out, o.quality.clamp(1, 100));
            flatten(img).write_with_encoder(enc).map_err(e)
        }
        Fmt::Png => {
            let level = if o.png_best { image::codecs::png::CompressionType::Best } else { image::codecs::png::CompressionType::Default };
            let enc = image::codecs::png::PngEncoder::new_with_quality(&mut *out, level, image::codecs::png::FilterType::Adaptive);
            img.write_with_encoder(enc).map_err(e)
        }
        Fmt::WebP => img.write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut *out)).map_err(e),
        Fmt::Bmp => img.write_to(out, image::ImageFormat::Bmp).map_err(e),
        Fmt::Gif => img.write_to(out, image::ImageFormat::Gif).map_err(e),
        Fmt::Tiff => img.write_to(out, image::ImageFormat::Tiff).map_err(e),
        Fmt::Avif => {
            let enc = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut *out, 6, o.quality.clamp(1, 100));
            img.write_with_encoder(enc).map_err(e)
        }
        Fmt::Keep => unreachable!(),
    }
}

fn expand_name(tpl: &str, stem: &str, w: u32, h: u32, n: usize, ext: &str) -> String {
    let s = tpl.replace("{nombre}", stem).replace("{ancho}", &w.to_string()).replace("{alto}", &h.to_string()).replace("{n}", &n.to_string()).replace("{formato}", &ext.to_uppercase());
    s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect::<String>().trim().to_string()
}

#[derive(Clone, Default)]
struct Res {
    src: PathBuf,
    out: PathBuf,
    from: (u32, u32),
    to: (u32, u32),
    in_bytes: u64,
    out_bytes: u64,
    skipped: bool,
    error: Option<String>,
}

fn run_one(src: &Path, n: usize, o: &Opts, taken: &Mutex<std::collections::HashSet<PathBuf>>) -> Res {
    let mut r = Res { src: src.to_path_buf(), in_bytes: fs::metadata(src).map(|m| m.len()).unwrap_or(0), ..Default::default() };
    let result = (|| -> Result<(), String> {
        let img = image::ImageReader::open(src).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?.decode().map_err(|e| e.to_string())?;
        r.from = (img.width(), img.height());
        let img = process(img, src, o);
        r.to = (img.width(), img.height());
        let ext = ext_for(o.fmt, src);
        let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let name = expand_name(&o.name, &stem, r.to.0, r.to.1, n, &ext);
        let dir = if o.dest_other { o.dest.clone() } else { src.parent().map(Path::to_path_buf).unwrap_or_default() };
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut out = dir.join(format!("{name}.{ext}"));
        {
            let mut t = taken.lock().map_err(|e| e.to_string())?;
            let same = out == src;
            if (out.exists() && o.conflict != 1) || t.contains(&out) || (same && o.conflict != 1) {
                if o.conflict == 2 && !t.contains(&out) {
                    r.skipped = true;
                    return Ok(());
                }
                out = fsops::unique_name(&dir, &format!("{name}.{ext}"));
                let mut k = 2;
                while t.contains(&out) {
                    out = dir.join(format!("{name} ({k}).{ext}"));
                    k += 1;
                }
            }
            t.insert(out.clone());
        }
        let tmp = out.with_file_name(format!(".{}.part", out.file_name().unwrap_or_default().to_string_lossy()));
        let write = || -> Result<(), String> {
            let mut w = BufWriter::new(fs::File::create(&tmp).map_err(|e| e.to_string())?);
            encode(&img, o.fmt, src, o, &mut w)?;
            w.flush().map_err(|e| e.to_string())
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
        r.out_bytes = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        r.out = out;
        Ok(())
    })();
    if let Err(e) = result {
        r.error = Some(e);
    }
    r
}

fn collect(items: &[PathBuf], recursive: bool) -> Vec<PathBuf> {
    let mut out = vec![];
    for p in items {
        if p.is_file() {
            if is_image(p) {
                out.push(p.clone());
            }
        } else if p.is_dir() {
            let mut stack = vec![p.clone()];
            while let Some(d) = stack.pop() {
                let mut kids: Vec<PathBuf> = fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
                kids.sort();
                for k in kids {
                    if k.is_dir() {
                        if recursive && !k.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) {
                            stack.push(k);
                        }
                    } else if is_image(&k) {
                        out.push(k);
                    }
                }
            }
        }
    }
    out.dedup();
    out
}

struct Job {
    rx: Receiver<(Vec<Res>, f32)>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct Imager {
    o: Opts,
    items: Vec<PathBuf>,
    picker: Option<Picker>,
    dest: DestField,
    job: Option<Job>,
    results: Vec<Res>,
    status: Option<(String, bool)>,
}

impl Imager {
    pub fn new(targets: Vec<PathBuf>) -> Imager {
        let items = targets.into_iter().filter(|p| p.is_dir() || is_image(p)).collect();
        Imager { o: Opts::default(), items, picker: None, dest: DestField::new("images_dest"), job: None, results: vec![], status: None }
    }

    /// Converts the folder's images to small JPEGs in a subfolder, for trying the tool (`--images`).
    pub fn demo(&mut self, dir: &Path) {
        self.items = vec![dir.to_path_buf()];
        self.o.fmt = Fmt::Jpeg;
        self.o.resize = Resize::Fit;
        self.o.w = 64;
        self.o.h = 64;
        self.o.dest_other = true;
        self.dest.text = dir.join("_pequenas").to_string_lossy().to_string();
        self.start();
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if (p.is_dir() || is_image(&p)) && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn start(&mut self) {
        if self.items.is_empty() {
            self.status = Some((tr("Añade imágenes o carpetas con imágenes.").to_string(), true));
            return;
        }
        if self.o.dest_other && self.dest.text.trim().is_empty() {
            self.status = Some((tr("Elige la carpeta de destino.").to_string(), true));
            return;
        }
        let mut o = self.o.clone();
        o.dest = self.dest.path();
        let files = collect(&self.items, o.recursive);
        if files.is_empty() {
            self.status = Some((tr("No hay imágenes (png, jpg, gif, bmp, tiff, webp, ico) en lo que has añadido.").to_string(), true));
            return;
        }
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(files.len())));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (d2, c2) = (done.clone(), cancel.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let taken = Mutex::new(std::collections::HashSet::new());
            let results: Mutex<Vec<(usize, Res)>> = Mutex::new(vec![]);
            let next = AtomicUsize::new(0);
            std::thread::scope(|sc| {
                for _ in 0..o.threads.clamp(1, 16).min(files.len()) {
                    sc.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() || c2.load(Ordering::Relaxed) {
                            break;
                        }
                        let r = run_one(&files[i], i + 1, &o, &taken);
                        d2.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut v) = results.lock() {
                            v.push((i, r));
                        }
                    });
                }
            });
            let mut v = results.into_inner().unwrap_or_default();
            v.sort_by_key(|(i, _)| *i);
            let _ = tx.send((v.into_iter().map(|(_, r)| r).collect(), started.elapsed().as_secs_f32()));
        });
        self.results.clear();
        self.status = None;
        self.job = Some(Job { rx, done, total, cancel, started: Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok((results, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        let _ = job.started;
        self.job = None;
        let ok = results.iter().filter(|r| r.error.is_none() && !r.skipped).count();
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        let (i, o): (u64, u64) = results.iter().filter(|r| r.error.is_none() && !r.skipped).fold((0, 0), |(a, b), r| (a + r.in_bytes, b + r.out_bytes));
        let head = if cancelled { tr("Cancelado") } else { tr("Terminado") };
        self.status = Some((trf!("{}: {} imagen(es) · {} » {} · {} error(es) · {} s", head, ok, human_size(i), human_size(o), bad, format!("{secs:.1}")), bad > 0 || cancelled));
        self.results = results;
        events.push(ToolEvent::Reload);
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("images_all").auto_shrink([false, false]).max_height(840.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.items_ui(ui, selection, cwd);
                ui.separator();
                self.format_ui(ui);
                self.resize_ui(ui);
                self.output_ui(ui, cwd, selection);
                self.advanced_ui(ui);
            });
            ui.separator();
            self.action_ui(ui);
            self.results_ui(ui, events);
        });
        let ctx = ui.ctx().clone();
        let added = picker_window(&ctx, "images_picker", &mut self.picker);
        self.add(added);
    }

    fn items_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("IMÁGENES")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("images_items").max_height(100.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, p) in self.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(if p.is_dir() { "📁" } else { "🖼" });
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
            let sel: Vec<PathBuf> = selection.iter().filter(|p| p.is_dir() || is_image(p)).cloned().collect();
            if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).on_hover_text(tr("Se añaden las imágenes y las carpetas; de las carpetas se toman sus imágenes.")).clicked() {
                self.add(sel);
            }
            if ui.button(tr("📂 Elegir archivos…")).clicked() {
                self.picker = Some(Picker::new(self.items.last().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| cwd.to_path_buf())));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
            ui.checkbox(&mut self.o.recursive, tr("Incluir subcarpetas"));
            ui.menu_button(tr("✨ Rápidos"), |ui| {
                ui.label(RichText::new(tr("Cambian formato y tamaño; lo que has añadido se mantiene")).small().color(theme::p().muted));
                for (name, desc, f) in PRESETS {
                    if ui.button(tr(name)).on_hover_text(tr(desc)).clicked() {
                        f(&mut self.o);
                        ui.close_menu();
                    }
                }
            });
        });
    }

    fn format_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("FORMATO")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for (f, label, _) in FORMATS {
                ui.selectable_value(&mut self.o.fmt, f, tr(label));
            }
        });
        if matches!(self.o.fmt, Fmt::Jpeg | Fmt::Avif | Fmt::Keep) {
            ui.horizontal(|ui| {
                ui.add_sized([120.0, 18.0], egui::Label::new(tr(if self.o.fmt == Fmt::Avif { "Calidad AVIF" } else { "Calidad JPEG" })));
                ui.add(egui::Slider::new(&mut self.o.quality, 1..=100));
                ui.label(RichText::new(tr(if self.o.quality >= 90 { "casi sin pérdida, pesa más" } else if self.o.quality >= 70 { "equilibrada" } else { "pequeña, se nota la compresión" })).small().color(theme::p().sub));
            });
        }
        if matches!(self.o.fmt, Fmt::Png | Fmt::Keep) {
            ui.checkbox(&mut self.o.png_best, tr("PNG con la máxima compresión (más lento)"));
        }
        if matches!(self.o.fmt, Fmt::Jpeg | Fmt::Bmp | Fmt::Avif) {
            ui.label(RichText::new(tr("Lo transparente se rellena de blanco.")).small().color(theme::p().muted));
        }
        if self.o.fmt == Fmt::Gif {
            ui.label(RichText::new(tr("Solo se guarda un fotograma: los GIF animados pierden la animación.")).small().color(theme::p().muted));
        }
    }

    fn resize_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("TAMAÑO")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for (r, label, desc) in RESIZES {
                ui.radio_value(&mut self.o.resize, r, tr(label)).on_hover_text(tr(desc));
            }
        });
        ui.horizontal_wrapped(|ui| match self.o.resize {
            Resize::None => {}
            Resize::Percent => {
                ui.label(tr("Porcentaje"));
                ui.add(egui::DragValue::new(&mut self.o.percent).range(1..=1000).suffix(" %"));
            }
            Resize::Width => {
                ui.label(tr("Ancho"));
                ui.add(egui::DragValue::new(&mut self.o.w).range(1..=20000).suffix(" px"));
            }
            Resize::Height => {
                ui.label(tr("Alto"));
                ui.add(egui::DragValue::new(&mut self.o.h).range(1..=20000).suffix(" px"));
            }
            _ => {
                ui.label(tr("Ancho"));
                ui.add(egui::DragValue::new(&mut self.o.w).range(1..=20000).suffix(" px"));
                ui.label("×");
                ui.label(tr("Alto"));
                ui.add(egui::DragValue::new(&mut self.o.h).range(1..=20000).suffix(" px"));
            }
        });
        if self.o.resize != Resize::None {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.o.no_enlarge, tr("No ampliar las que ya son más pequeñas"));
                egui::ComboBox::from_id_salt("images_filter").width(240.0).selected_text(tr(FILTERS[self.o.filter].0)).show_ui(ui, |ui| {
                    for (k, (l, _)) in FILTERS.iter().enumerate() {
                        ui.selectable_value(&mut self.o.filter, k, tr(l));
                    }
                });
            });
        }
    }

    fn output_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.label(RichText::new(tr("NOMBRE Y DESTINO")).small().strong().color(theme::p().sub));
        ui.horizontal(|ui| {
            ui.add_sized([90.0, 18.0], egui::Label::new(tr("Nombre")));
            ui.add(egui::TextEdit::singleline(&mut self.o.name).desired_width(260.0).font(egui::TextStyle::Monospace)).on_hover_text(tr("Admite {nombre}, {ancho}, {alto}, {n} y {formato}"));
            for c in ["{nombre}", "{ancho}", "{alto}", "{n}"] {
                if ui.small_button(c).clicked() {
                    self.o.name.push_str(c);
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([90.0, 18.0], egui::Label::new(tr("Guardar en")));
            ui.radio_value(&mut self.o.dest_other, false, tr("Junto al original"));
            ui.radio_value(&mut self.o.dest_other, true, tr("Otra carpeta"));
            if self.o.dest_other {
                self.dest.ui(ui, cwd, selection);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([90.0, 18.0], egui::Label::new(tr("Si ya existe")));
            for (k, l) in CONFLICTS.iter().enumerate() {
                ui.radio_value(&mut self.o.conflict, k as u8, tr(l));
            }
        });
        if self.o.conflict == 1 && !self.o.dest_other && !self.o.name.contains('{') {
            ui.label(RichText::new(tr("⚠ Con este nombre y «Sobrescribir» se reemplazarían las imágenes originales.")).small().color(theme::p().danger));
        }
    }

    fn advanced_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(tr("⚙ Girar, voltear y más")).default_open(false).show(ui, |ui| {
            ui.checkbox(&mut self.o.auto_orient, tr("Enderezar según la orientación de la cámara (EXIF)")).on_hover_text(tr("Las fotos del móvil a veces guardan «girada» una foto en vertical; así queda realmente derecha."));
            ui.horizontal(|ui| {
                ui.label(tr("Girar"));
                for (k, l) in ["0°", "90° →", "180°", "270° ←"].iter().enumerate() {
                    ui.radio_value(&mut self.o.rotate, k as u8, *l);
                }
                ui.checkbox(&mut self.o.flip_h, tr("Voltear horizontal"));
                ui.checkbox(&mut self.o.flip_v, tr("Voltear vertical"));
            });
            ui.checkbox(&mut self.o.gray, tr("Pasar a blanco y negro"));
            ui.horizontal(|ui| {
                ui.label(tr("Imágenes a la vez"));
                ui.add(egui::DragValue::new(&mut self.o.threads).range(1..=16));
            });
            ui.label(RichText::new(tr("Las imágenes nuevas no llevan los datos EXIF (cámara, fecha, ubicación): también sirve para limpiarlos antes de compartirlas.")).small().color(theme::p().muted));
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🖼 Convertir")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.items.is_empty(), btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let (d, t) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(200.0).show_percentage());
                ui.label(RichText::new(format!("{d}/{t}")).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.results.is_empty() {
            return;
        }
        ui.separator();
        egui::ScrollArea::vertical().id_salt("images_res").max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("images_grid").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for r in &self.results {
                    let name = if r.out.as_os_str().is_empty() { r.src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { r.out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() };
                    if ui.link(name).on_hover_text(tr("Ir a la carpeta")).clicked() {
                        if let Some(p) = r.out.parent().or_else(|| r.src.parent()) {
                            events.push(ToolEvent::Navigate(p.to_path_buf()));
                        }
                    }
                    match (&r.error, r.skipped) {
                        (Some(e), _) => {
                            ui.label("");
                            ui.label("");
                            ui.label(RichText::new(format!("✖ {e}")).small().color(theme::p().danger));
                        }
                        (_, true) => {
                            ui.label("");
                            ui.label("");
                            ui.label(RichText::new(tr("omitida: ya existe")).small().color(theme::p().muted));
                        }
                        _ => {
                            ui.label(RichText::new(format!("{}×{} » {}×{}", r.from.0, r.from.1, r.to.0, r.to.1)).small().color(theme::p().sub));
                            let ratio = if r.in_bytes > 0 { format!("{:.0}%", r.out_bytes as f64 * 100.0 / r.in_bytes as f64) } else { "—".into() };
                            ui.label(RichText::new(format!("{} » {} ({ratio})", human_size(r.in_bytes), human_size(r.out_bytes))).small());
                            ui.label(RichText::new("✔").small().color(theme::p().ok));
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }
}

#[allow(dead_code)]
fn _io(_: io::Result<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgba, RgbaImage};

    fn setup(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("img-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let mut img = RgbaImage::new(200, 100);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgba([(x % 256) as u8, (y * 2 % 256) as u8, 128, if x < 20 { 0 } else { 255 }]);
        }
        img.save(d.join("foto.png")).unwrap();
        d
    }

    fn run(d: &Path, o: &Opts) -> Res {
        run_one(&d.join("foto.png"), 1, o, &Mutex::new(Default::default()))
    }

    #[test]
    fn sizes_are_computed_for_every_mode() {
        let mut o = Opts::default();
        o.w = 100;
        o.h = 100;
        o.resize = Resize::Fit;
        assert_eq!(target_dims(200, 100, &o), (100, 50));
        o.resize = Resize::Width;
        o.w = 50;
        assert_eq!(target_dims(200, 100, &o), (50, 25));
        o.resize = Resize::Height;
        o.h = 300;
        assert_eq!(target_dims(200, 100, &o), (200, 100), "not enlarged");
        o.no_enlarge = false;
        assert_eq!(target_dims(200, 100, &o), (600, 300));
        o.resize = Resize::Percent;
        o.percent = 25;
        assert_eq!(target_dims(200, 100, &o), (50, 25));
        o.resize = Resize::Exact;
        o.w = 10;
        o.h = 20;
        assert_eq!(target_dims(200, 100, &o), (10, 20));
        assert_eq!(expand_name("{nombre}-{ancho}x{alto}-{n}", "a/b", 5, 6, 2, "jpg"), "a-b-5x6-2");
    }

    #[test]
    fn converts_between_formats_and_resizes() {
        let d = setup("conv");
        let mut o = Opts { resize: Resize::Fit, w: 100, h: 100, fmt: Fmt::Jpeg, ..Opts::default() };
        let r = run(&d, &o);
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.to, (100, 50));
        assert!(r.out.to_string_lossy().ends_with("foto-100x50.jpg"));
        let back = image::open(&r.out).unwrap();
        assert_eq!(back.dimensions(), (100, 50));
        let p = back.get_pixel(2, 10);
        assert!(p[0] > 240 && p[1] > 240 && p[2] > 240, "transparent area became white: {p:?}");
        for (fmt, ext) in [(Fmt::Png, "png"), (Fmt::WebP, "webp"), (Fmt::Bmp, "bmp"), (Fmt::Gif, "gif"), (Fmt::Tiff, "tif")] {
            o.fmt = fmt;
            o.name = format!("salida-{ext}");
            let r = run(&d, &o);
            assert!(r.error.is_none(), "{ext}: {:?}", r.error);
            assert_eq!(image::open(&r.out).unwrap().dimensions(), (100, 50), "{ext}");
        }
        // Rotate, fill-and-crop and grayscale.
        let o = Opts { resize: Resize::Fill, w: 40, h: 40, rotate: 1, gray: true, name: "girada".into(), ..Opts::default() };
        let r = run(&d, &o);
        assert_eq!(r.to, (40, 40));
        let img = image::open(&r.out).unwrap();
        let p = img.get_pixel(20, 20);
        assert!(p[0] == p[1] && p[1] == p[2] || img.color().channel_count() <= 2);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn names_never_overwrite_originals_by_default_and_folders_expand() {
        let d = setup("names");
        let o = Opts { resize: Resize::None, name: "{nombre}".into(), ..Opts::default() };
        let r = run(&d, &o);
        assert!(r.out.ends_with("foto (copia).png"), "{:?}", r.out);
        let mut o2 = o.clone();
        o2.conflict = 2;
        assert!(run(&d, &o2).skipped);
        fs::write(d.join("nota.txt"), "x").unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::copy(d.join("foto.png"), d.join("sub/otra.png")).unwrap();
        assert_eq!(collect(&[d.clone()], false).len(), 2, "foto.png and the copy");
        assert_eq!(collect(&[d.clone()], true).len(), 3);
        assert_eq!(orientation(&d.join("foto.png")), 1);
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(4, 2));
        assert_eq!(apply_orientation(img.clone(), 6).dimensions(), (2, 4));
        assert_eq!(apply_orientation(img, 3).dimensions(), (4, 2));
        fs::remove_dir_all(&d).unwrap();
    }
}
