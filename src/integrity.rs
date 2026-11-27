//! Check files for damage: every picture is decoded, every ZIP-based file (zip, docx, xlsx,
//! epub, apk…) and tar/gz/bz2/xz/zst archive is read to the end with its checksums, PDFs must
//! start and end as a PDF does, music must have readable headers and JSON must parse. Empty
//! files are reported too. Several files are checked at once; nothing is changed until the
//! person sends the damaged ones to the trash.

use crate::fsops::human_size;
use crate::i18n::tr;
use crate::sources::Sources;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

#[derive(Clone, PartialEq, Debug)]
enum Verdict {
    Ok,
    Empty,
    Damaged(String),
    /// A kind of file this tool doesn't know how to check.
    Unchecked,
}

fn ext_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).map(|n| {
        for double in [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"] {
            if n.ends_with(double) {
                return double[1..].to_string();
            }
        }
        Path::new(&n).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default()
    }).unwrap_or_default()
}

fn read_all(mut r: impl Read) -> Result<(), String> {
    io::copy(&mut r, &mut io::sink()).map(|_| ()).map_err(|e| e.to_string())
}

fn check_zip(p: &Path) -> Result<(), String> {
    let f = fs::File::open(p).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(BufReader::new(f)).map_err(|e| e.to_string())?;
    for i in 0..z.len() {
        match z.by_index(i) {
            Ok(entry) => {
                let name = entry.name().map(|n| n.to_string()).unwrap_or_default();
                read_all(entry).map_err(|e| format!("{name}: {e}"))?;
            }
            // Password-protected entries can't be read without the password: not damage.
            Err(zip::result::ZipError::UnsupportedArchive(m)) if m.to_lowercase().contains("password") => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn check_tar(r: impl Read) -> Result<(), String> {
    let mut a = tar::Archive::new(r);
    for e in a.entries().map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        read_all(e)?;
    }
    Ok(())
}

fn check_pdf(p: &Path) -> Result<(), String> {
    let mut f = fs::File::open(p).map_err(|e| e.to_string())?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut head = [0u8; 5];
    f.read_exact(&mut head).map_err(|e| e.to_string())?;
    if &head != b"%PDF-" {
        return Err(tr("no empieza como un PDF").to_string());
    }
    use std::io::{Seek, SeekFrom};
    let tail_len = len.min(2048);
    f.seek(SeekFrom::Start(len - tail_len)).map_err(|e| e.to_string())?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).map_err(|e| e.to_string())?;
    if !tail.windows(5).any(|w| w == b"%%EOF") {
        return Err(tr("está cortado: falta el final del PDF").to_string());
    }
    Ok(())
}

fn check(p: &Path) -> Verdict {
    let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Verdict::Empty;
    }
    let open = || fs::File::open(p).map(BufReader::new).map_err(|e| e.to_string());
    let r: Result<(), String> = match ext_of(p).as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico" | "tga" | "qoi" => {
            image::ImageReader::open(p).map_err(|e| e.to_string()).and_then(|r| r.with_guessed_format().map_err(|e| e.to_string())).and_then(|r| r.decode().map(|_| ()).map_err(|e| e.to_string()))
        }
        "zip" | "jar" | "apk" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub" | "cbz" | "xpi" => check_zip(p),
        "tar" => open().and_then(check_tar),
        "tar.gz" | "tgz" => open().and_then(|f| check_tar(flate2::read::MultiGzDecoder::new(f))),
        "gz" => open().and_then(|f| read_all(flate2::read::MultiGzDecoder::new(f))),
        "tar.bz2" | "tbz2" => open().and_then(|f| check_tar(bzip2::read::MultiBzDecoder::new(f))),
        "bz2" => open().and_then(|f| read_all(bzip2::read::MultiBzDecoder::new(f))),
        "tar.xz" | "txz" => open().and_then(|f| check_tar(lzma_rust2::XzReader::new(f, true))),
        "xz" => open().and_then(|f| read_all(lzma_rust2::XzReader::new(f, true))),
        "tar.zst" => open().and_then(|f| zstd::stream::read::Decoder::new(f).map_err(|e| e.to_string())).and_then(check_tar),
        "zst" => open().and_then(|f| zstd::stream::read::Decoder::new(f).map_err(|e| e.to_string())).and_then(read_all),
        "pdf" => check_pdf(p),
        "mp3" | "flac" | "ogg" | "opus" | "m4a" | "wav" | "aiff" | "wv" | "ape" => lofty::read_from_path(p).map(|_| ()).map_err(|e| e.to_string()),
        "json" => fs::read(p).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).map(|_| ()).map_err(|e| e.to_string())),
        _ => return Verdict::Unchecked,
    };
    match r {
        Ok(()) => Verdict::Ok,
        Err(e) => Verdict::Damaged(e.lines().next().unwrap_or("").chars().take(200).collect()),
    }
}

#[derive(Clone)]
struct Row {
    path: PathBuf,
    size: u64,
    verdict: Verdict,
    on: bool,
}

struct Job {
    rx: Receiver<Vec<Row>>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
}

pub struct Integrity {
    src: Sources,
    job: Option<Job>,
    rows: Vec<Row>,
    only_problems: bool,
    status: Option<(String, bool)>,
}

impl Integrity {
    pub fn new(targets: Vec<PathBuf>) -> Integrity {
        let mut src = Sources::new("integrity_src", targets);
        src.exclude_dirs = ".git, node_modules, .cache".into();
        Integrity { src, job: None, rows: vec![], only_problems: true, status: None }
    }

    /// Checks a folder at once, for trying the tool (`--integrity`).
    pub fn demo(&mut self, dir: &Path) {
        self.src.items = vec![dir.to_path_buf()];
        self.start();
    }

    fn start(&mut self) {
        let Ok(filters) = self.src.filters() else {
            self.status = Some((tr("Algún patrón no es válido.").to_string(), true));
            return;
        };
        if self.src.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas.").to_string(), true));
            return;
        }
        let (items, recursive, hidden) = (self.src.items.clone(), self.src.recursive, self.src.hidden);
        let (done, total, cancel) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (tx, rx) = channel();
        let (d2, t2, c2) = (done.clone(), total.clone(), cancel.clone());
        std::thread::spawn(move || {
            let (files, _) = crate::sources::collect_files(&items, &filters, recursive, hidden, &c2);
            t2.store(files.len(), Ordering::Relaxed);
            let next = AtomicUsize::new(0);
            let out: Mutex<Vec<(usize, Row)>> = Mutex::new(Vec::with_capacity(files.len()));
            let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8);
            std::thread::scope(|sc| {
                for _ in 0..workers {
                    sc.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() || c2.load(Ordering::Relaxed) {
                            break;
                        }
                        let p = &files[i];
                        let row = Row { path: p.clone(), size: fs::metadata(p).map(|m| m.len()).unwrap_or(0), verdict: check(p), on: true };
                        d2.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut o) = out.lock() {
                            o.push((i, row));
                        }
                    });
                }
            });
            let mut v = out.into_inner().unwrap_or_default();
            v.sort_by_key(|(i, _)| *i);
            let _ = tx.send(v.into_iter().map(|(_, r)| r).collect());
        });
        self.rows.clear();
        self.status = None;
        self.job = Some(Job { rx, done, total, cancel });
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(rows) = job.rx.try_recv() else { return };
        self.job = None;
        let bad = rows.iter().filter(|r| matches!(r.verdict, Verdict::Damaged(_) | Verdict::Empty)).count();
        let ok = rows.iter().filter(|r| r.verdict == Verdict::Ok).count();
        let unchecked = rows.iter().filter(|r| r.verdict == Verdict::Unchecked).count();
        self.status = Some((trf!("{} correctos · {} con problemas · {} sin comprobar (tipo desconocido)", ok, bad, unchecked), bad > 0));
        self.rows = rows;
        // Only problems get ticked, ready to be sent to the trash.
        for r in &mut self.rows {
            r.on = matches!(r.verdict, Verdict::Damaged(_) | Verdict::Empty);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll();
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| self.src.ui(ui, selection, cwd, tr("ARCHIVOS Y CARPETAS")));
        crate::ui_kit::note(ui, tr("Se comprueban fotos, comprimidos (zip, docx, xlsx, epub, tar, gz, xz…), PDF, música y JSON. Nada se modifica."));
        ui.separator();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tr("🔍 Comprobar")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy, btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let (d, t) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(200.0).text(format!("{d}/{t}")));
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if self.rows.is_empty() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.only_problems, true, tr("Solo los que tienen problemas"));
            ui.selectable_value(&mut self.only_problems, false, tr("Todos"));
        });
        let view: Vec<usize> = (0..self.rows.len()).filter(|&i| !self.only_problems || matches!(self.rows[i].verdict, Verdict::Damaged(_) | Verdict::Empty)).collect();
        let mut nav = None;
        egui::ScrollArea::vertical().id_salt("integrity_rows").max_height(crate::tools::grow(ui, 300.0)).auto_shrink([false, false]).show_rows(ui, 22.0, view.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &i in &view[range] {
                let r = &mut self.rows[i];
                ui.horizontal(|ui| {
                    ui.set_height(22.0);
                    let problem = matches!(r.verdict, Verdict::Damaged(_) | Verdict::Empty);
                    ui.add_enabled(problem, egui::Checkbox::new(&mut r.on, ""));
                    let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if crate::checksum::cell(ui, 260.0, 22.0, |ui| ui.add(egui::Label::new(name).truncate().sense(egui::Sense::click()))).on_hover_text(r.path.to_string_lossy()).clicked() {
                        nav = r.path.parent().map(Path::to_path_buf);
                    }
                    crate::checksum::cell(ui, 70.0, 22.0, |ui| ui.label(RichText::new(human_size(r.size)).small().color(theme::p().sub)));
                    let (t, c) = match &r.verdict {
                        Verdict::Ok => (tr("✔ correcto").to_string(), theme::p().ok),
                        Verdict::Empty => (tr("✖ vacío (0 bytes)").to_string(), theme::p().danger),
                        Verdict::Damaged(e) => (format!("✖ {e}"), theme::p().danger),
                        Verdict::Unchecked => (tr("— sin comprobar").to_string(), theme::p().muted),
                    };
                    ui.add(egui::Label::new(RichText::new(t).small().color(c)).truncate());
                });
            }
        });
        if let Some(p) = nav {
            events.push(ToolEvent::Navigate(p));
        }
        let marked: Vec<PathBuf> = self.rows.iter().filter(|r| r.on).map(|r| r.path.clone()).collect();
        ui.horizontal(|ui| {
            if ui.add_enabled(!marked.is_empty(), egui::Button::new(tr("📋 Copiar rutas"))).clicked() {
                events.push(ToolEvent::Clipboard(marked.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n")));
            }
            let btn = egui::Button::new(RichText::new(trf!("🗑 Mover {} a la papelera", marked.len())).color(Color32::WHITE)).fill(theme::p().danger);
            if ui.add_enabled(!marked.is_empty() && !busy, btn).clicked() {
                let (mut ok, mut errs) = (0, 0);
                for p in &marked {
                    if trash::delete(p).is_ok() {
                        ok += 1;
                    } else {
                        errs += 1;
                    }
                }
                self.rows.retain(|r| r.path.exists());
                self.status = Some((trf!("{} archivo(s) a la papelera · {} error(es)", ok, errs), errs > 0));
                events.push(ToolEvent::Reload);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_damaged_pictures_archives_pdfs_and_empty_files() {
        let d = std::env::temp_dir().join(format!("integ-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        image::RgbImage::new(8, 8).save(d.join("bien.png")).unwrap();
        let mut png = fs::read(d.join("bien.png")).unwrap();
        png.truncate(png.len() / 2);
        fs::write(d.join("cortada.png"), &png).unwrap();
        fs::write(d.join("vacio.txt"), b"").unwrap();
        fs::write(d.join("doc.pdf"), b"%PDF-1.4\n...\n%%EOF\n").unwrap();
        fs::write(d.join("roto.pdf"), b"%PDF-1.4\n... cortado").unwrap();
        fs::write(d.join("bien.json"), b"{\"a\": [1, 2]}").unwrap();
        fs::write(d.join("mal.json"), b"{\"a\": [1, 2}").unwrap();
        fs::write(d.join("nota.xyz"), b"hola").unwrap();
        {
            let f = fs::File::create(d.join("ok.zip")).unwrap();
            let mut z = zip::ZipWriter::new(f);
            z.start_file("a.txt", zip::write::SimpleFileOptions::default()).unwrap();
            io::Write::write_all(&mut z, &vec![b'x'; 5000]).unwrap();
            z.finish().unwrap();
        }
        let mut zbytes = fs::read(d.join("ok.zip")).unwrap();
        let mid = zbytes.len() / 3;
        zbytes[mid] ^= 0xFF;
        fs::write(d.join("malo.zip"), &zbytes).unwrap();
        let v = |n: &str| check(&d.join(n));
        assert_eq!(v("bien.png"), Verdict::Ok);
        assert!(matches!(v("cortada.png"), Verdict::Damaged(_)));
        assert_eq!(v("vacio.txt"), Verdict::Empty);
        assert_eq!(v("doc.pdf"), Verdict::Ok);
        assert!(matches!(v("roto.pdf"), Verdict::Damaged(_)));
        assert_eq!(v("bien.json"), Verdict::Ok);
        assert!(matches!(v("mal.json"), Verdict::Damaged(_)));
        assert_eq!(v("nota.xyz"), Verdict::Unchecked);
        assert_eq!(v("ok.zip"), Verdict::Ok);
        assert!(matches!(v("malo.zip"), Verdict::Damaged(_)), "{:?}", v("malo.zip"));
        assert_eq!(ext_of(Path::new("a/b.TAR.GZ")), "tar.gz");
        fs::remove_dir_all(&d).unwrap();
    }
}
