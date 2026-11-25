//! Text files: see and change line endings (LF, CRLF, CR), the text encoding (UTF-8 with or
//! without BOM, UTF-16, Windows-1252, ISO-8859-15…), tabs and spaces, trailing spaces, blank
//! lines and the final newline. First an analysis of what each file has, then the changes;
//! only files that would really change are rewritten, optionally keeping a .bak copy.

use crate::fsops;
use crate::i18n::tr;
use crate::roots::{picker_window, DestField, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use encoding_rs::Encoding;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Enc {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Win1252,
    Iso8859_15,
    Win1250,
    Iso8859_2,
}

const ENCODINGS: [(Enc, &str); 8] = [
    (Enc::Utf8, "UTF-8"),
    (Enc::Utf8Bom, "UTF-8 con BOM"),
    (Enc::Utf16Le, "UTF-16 LE"),
    (Enc::Utf16Be, "UTF-16 BE"),
    (Enc::Win1252, "Windows-1252 (Europa occidental)"),
    (Enc::Iso8859_15, "ISO-8859-15 (Latin-9)"),
    (Enc::Win1250, "Windows-1250 (Europa central)"),
    (Enc::Iso8859_2, "ISO-8859-2 (Latin-2)"),
];

fn label(e: Enc) -> &'static str {
    ENCODINGS.iter().find(|x| x.0 == e).map(|x| x.1).unwrap_or("?")
}

fn rs_encoding(e: Enc) -> &'static Encoding {
    match e {
        Enc::Win1252 => encoding_rs::WINDOWS_1252,
        Enc::Iso8859_15 => encoding_rs::ISO_8859_15,
        Enc::Win1250 => encoding_rs::WINDOWS_1250,
        Enc::Iso8859_2 => encoding_rs::ISO_8859_2,
        _ => encoding_rs::UTF_8,
    }
}

/// Reads the bytes as text; `from` None detects the encoding (BOMs, then valid UTF-8, then Windows-1252).
fn decode(bytes: &[u8], from: Option<Enc>) -> Result<(String, Enc), String> {
    let dec16 = |b: &[u8], le: bool| -> Result<String, String> {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16(&units).map_err(|_| tr("UTF-16 no válido").to_string())
    };
    let e = match from {
        Some(e) => e,
        None => {
            if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                Enc::Utf8Bom
            } else if bytes.starts_with(&[0xFF, 0xFE]) {
                Enc::Utf16Le
            } else if bytes.starts_with(&[0xFE, 0xFF]) {
                Enc::Utf16Be
            } else if std::str::from_utf8(bytes).is_ok() {
                Enc::Utf8
            } else {
                Enc::Win1252
            }
        }
    };
    let text = match e {
        Enc::Utf8 | Enc::Utf8Bom => {
            let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
            std::str::from_utf8(b).map_err(|_| tr("no es UTF-8 válido").to_string())?.to_string()
        }
        Enc::Utf16Le => dec16(bytes.strip_prefix(&[0xFF, 0xFE][..]).unwrap_or(bytes), true)?,
        Enc::Utf16Be => dec16(bytes.strip_prefix(&[0xFE, 0xFF][..]).unwrap_or(bytes), false)?,
        other => rs_encoding(other).decode_without_bom_handling(bytes).0.to_string(),
    };
    Ok((text, e))
}

fn encode(text: &str, e: Enc) -> Result<Vec<u8>, String> {
    Ok(match e {
        Enc::Utf8 => text.as_bytes().to_vec(),
        Enc::Utf8Bom => [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat(),
        Enc::Utf16Le => [vec![0xFF, 0xFE], text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()].concat(),
        Enc::Utf16Be => [vec![0xFE, 0xFF], text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()].concat(),
        other => {
            let (b, _, errors) = rs_encoding(other).encode(text);
            if errors {
                return Err(trf!("hay caracteres que no existen en {}", label(other)));
            }
            b.into_owned()
        }
    })
}

/// Lines with the ending each one had.
fn split_lines(text: &str) -> Vec<(&str, &'static str)> {
    let b = text.as_bytes();
    let (mut out, mut start, mut i) = (vec![], 0, 0);
    while i < b.len() {
        match b[i] {
            b'\n' => {
                out.push((&text[start..i], "\n"));
                start = i + 1;
            }
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                out.push((&text[start..i], "\r\n"));
                i += 1;
                start = i + 1;
            }
            b'\r' => {
                out.push((&text[start..i], "\r"));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < b.len() {
        out.push((&text[start..], ""));
    }
    out
}

#[derive(Clone, Default, PartialEq)]
struct Analysis {
    enc: String,
    lines: usize,
    lf: usize,
    crlf: usize,
    cr: usize,
    trailing_ws: usize,
    tabs: usize,
    final_newline: bool,
}

impl Analysis {
    fn eol(&self) -> &'static str {
        match (self.lf > 0, self.crlf > 0, self.cr > 0) {
            (false, false, false) => "—",
            (true, false, false) => "LF",
            (false, true, false) => "CRLF",
            (false, false, true) => "CR",
            _ => "mixto",
        }
    }
}

fn analyse(text: &str, enc: Enc) -> Analysis {
    let mut a = Analysis { enc: label(enc).to_string(), ..Default::default() };
    let lines = split_lines(text);
    a.lines = lines.len();
    for (l, e) in &lines {
        match *e {
            "\n" => a.lf += 1,
            "\r\n" => a.crlf += 1,
            "\r" => a.cr += 1,
            _ => {}
        }
        if l.ends_with([' ', '\t']) {
            a.trailing_ws += 1;
        }
        if l.contains('\t') {
            a.tabs += 1;
        }
    }
    a.final_newline = lines.last().map(|l| !l.1.is_empty()).unwrap_or(false);
    a
}

#[derive(Clone)]
struct Opts {
    from: Option<Enc>,
    to: Option<Enc>,
    eol: u8,
    tabs: u8,
    tab_width: usize,
    trim: bool,
    blank_max: usize,
    final_nl: u8,
    backup: bool,
    dest_other: bool,
    dest: PathBuf,
    conflict: u8,
    include: String,
    recursive: bool,
}

/// Applies the line, tab and blank-line rules to text that is already decoded.
fn transform(text: &str, o: &Opts) -> String {
    let lines = split_lines(text);
    let eol = match o.eol {
        1 => Some("\n"),
        2 => Some("\r\n"),
        3 => Some("\r"),
        _ => None,
    };
    let dominant = eol.unwrap_or("\n");
    let mut out = String::with_capacity(text.len() + 16);
    let mut blanks = 0usize;
    let mut kept: Vec<(String, &str)> = Vec::with_capacity(lines.len());
    for (line, end) in &lines {
        let mut l = line.to_string();
        if o.trim {
            l = l.trim_end_matches([' ', '\t']).to_string();
        }
        match o.tabs {
            1 => {
                let mut col = 0;
                let mut s = String::with_capacity(l.len());
                for ch in l.chars() {
                    if ch == '\t' {
                        let n = o.tab_width - col % o.tab_width;
                        s.extend(std::iter::repeat(' ').take(n));
                        col += n;
                    } else {
                        s.push(ch);
                        col += 1;
                    }
                }
                l = s;
            }
            2 => {
                let spaces = l.len() - l.trim_start_matches(' ').len();
                l = format!("{}{}{}", "\t".repeat(spaces / o.tab_width), " ".repeat(spaces % o.tab_width), &l[spaces..]);
            }
            _ => {}
        }
        if l.trim().is_empty() {
            blanks += 1;
            if o.blank_max > 0 && blanks > o.blank_max {
                continue;
            }
        } else {
            blanks = 0;
        }
        kept.push((l, end));
    }
    let n = kept.len();
    for (i, (l, end)) in kept.iter().enumerate() {
        out.push_str(l);
        let last = i + 1 == n;
        let ending = if last {
            match o.final_nl {
                1 => if end.is_empty() { dominant } else { eol.unwrap_or(end) },
                2 => "",
                _ => if end.is_empty() { "" } else { eol.unwrap_or(end) },
            }
        } else {
            eol.unwrap_or(end)
        };
        out.push_str(ending);
    }
    out
}

fn is_text_name(name: &str, patterns: &Option<regex::Regex>) -> bool {
    patterns.as_ref().map(|re| re.is_match(name)).unwrap_or(true)
}

fn collect(items: &[PathBuf], o: &Opts) -> Vec<PathBuf> {
    let pats = crate::rename::name_matcher(&o.include, false).ok().flatten();
    let mut out = vec![];
    for p in items {
        if p.is_file() {
            out.push(p.clone());
        } else if p.is_dir() {
            let mut stack = vec![p.clone()];
            while let Some(d) = stack.pop() {
                let mut kids: Vec<PathBuf> = fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
                kids.sort();
                for k in kids {
                    let name = k.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if k.is_dir() {
                        if o.recursive && !name.starts_with('.') {
                            stack.push(k);
                        }
                    } else if k.is_file() && is_text_name(&name, &pats) {
                        out.push(k);
                    }
                }
            }
        }
    }
    out.dedup();
    out
}

#[derive(Clone, Default)]
struct Res {
    path: PathBuf,
    before: Analysis,
    after: Option<Analysis>,
    changed: bool,
    skipped: Option<String>,
    error: Option<String>,
    out: PathBuf,
}

fn load(path: &Path, from: Option<Enc>) -> Result<(String, Enc), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.iter().take(8000).any(|b| *b == 0) && !(bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])) {
        return Err(tr("parece un archivo binario").to_string());
    }
    decode(&bytes, from)
}

fn analyse_file(path: &Path, o: &Opts) -> Res {
    let mut r = Res { path: path.to_path_buf(), ..Default::default() };
    match load(path, o.from) {
        Ok((text, enc)) => r.before = analyse(&text, enc),
        Err(e) => r.skipped = Some(e),
    }
    r
}

fn convert_file(path: &Path, o: &Opts) -> Res {
    let mut r = analyse_file(path, o);
    if r.skipped.is_some() {
        return r;
    }
    let result = (|| -> Result<(), String> {
        let original = fs::read(path).map_err(|e| e.to_string())?;
        let (text, enc) = decode(&original, o.from)?;
        let new_text = transform(&text, o);
        let target = o.to.unwrap_or(enc);
        let bytes = encode(&new_text, target)?;
        r.after = Some(analyse(&new_text, target));
        let dir = if o.dest_other { o.dest.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if bytes == original && !o.dest_other {
            return Ok(());
        }
        r.changed = true;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut out = dir.join(&name);
        if o.dest_other && out.exists() {
            match o.conflict {
                2 => {
                    r.changed = false;
                    r.skipped = Some(tr("ya existe en el destino").to_string());
                    return Ok(());
                }
                0 => out = fsops::unique_name(&dir, &name),
                _ => {}
            }
        }
        if o.backup && !o.dest_other {
            let bak = fsops::unique_name(&dir, &format!("{name}.bak"));
            fs::copy(path, &bak).map_err(|e| e.to_string())?;
        }
        let tmp = out.with_file_name(format!(".{}.part", out.file_name().unwrap_or_default().to_string_lossy()));
        fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        if !o.dest_other {
            if let Ok(m) = fs::metadata(path) {
                let _ = fs::set_permissions(&tmp, m.permissions());
            }
        }
        fs::rename(&tmp, &out).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })?;
        r.out = out;
        Ok(())
    })();
    if let Err(e) = result {
        r.error = Some(e);
        r.changed = false;
    }
    r
}

struct Job {
    rx: Receiver<(Vec<Res>, bool, f32)>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct TextConv {
    items: Vec<PathBuf>,
    picker: Option<Picker>,
    dest: DestField,
    from: Option<Enc>,
    to: Option<Enc>,
    eol: u8,
    tabs: u8,
    tab_width: usize,
    trim: bool,
    blank_max: usize,
    final_nl: u8,
    backup: bool,
    dest_other: bool,
    conflict: u8,
    include: String,
    recursive: bool,
    job: Option<Job>,
    results: Vec<Res>,
    applied: bool,
    status: Option<(String, bool)>,
}

const EOLS: [&str; 4] = ["No cambiar", "LF (Linux y Mac)", "CRLF (Windows)", "CR (Mac antiguo)"];
const TABS: [&str; 3] = ["No cambiar", "Tabuladores » espacios", "Espacios iniciales » tabuladores"];
const FINAL: [&str; 3] = ["No cambiar", "Asegurar un salto al final", "Quitar el salto del final"];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

impl TextConv {
    pub fn new(targets: Vec<PathBuf>) -> TextConv {
        TextConv {
            items: targets.into_iter().filter(|p| p.exists()).collect(),
            picker: None,
            dest: DestField::new("textconv_dest"),
            from: None,
            to: None,
            eol: 0,
            tabs: 0,
            tab_width: 4,
            trim: false,
            blank_max: 0,
            final_nl: 0,
            backup: true,
            dest_other: false,
            conflict: 0,
            include: "*.txt; *.md; *.csv; *.json; *.xml; *.html; *.css; *.js; *.ts; *.py; *.rs; *.c; *.h; *.cpp; *.java; *.yml; *.yaml; *.toml; *.ini; *.log; *.sh; *.bat; *.sql".into(),
            recursive: true,
            job: None,
            results: vec![],
            applied: false,
            status: None,
        }
    }

    /// Analyses the folder at once, for trying the tool (`--text`).
    pub fn demo(&mut self, dir: &Path) {
        self.items = vec![dir.to_path_buf()];
        self.eol = 1;
        self.trim = true;
        self.start(false);
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.exists() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn opts(&self) -> Opts {
        Opts {
            from: self.from,
            to: self.to,
            eol: self.eol,
            tabs: self.tabs,
            tab_width: self.tab_width.max(1),
            trim: self.trim,
            blank_max: self.blank_max,
            final_nl: self.final_nl,
            backup: self.backup,
            dest_other: self.dest_other,
            dest: self.dest.path(),
            conflict: self.conflict,
            include: self.include.clone(),
            recursive: self.recursive,
        }
    }

    fn start(&mut self, apply: bool) {
        if self.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas con texto.").to_string(), true));
            return;
        }
        if apply && self.dest_other && self.dest.text.trim().is_empty() {
            self.status = Some((tr("Elige la carpeta de destino.").to_string(), true));
            return;
        }
        if let Err(e) = crate::rename::name_matcher(&self.include, false) {
            self.status = Some((trf!("Expresión no válida: {}", e), true));
            return;
        }
        let o = self.opts();
        let files = collect(&self.items, &o);
        if files.is_empty() {
            self.status = Some((tr("No hay archivos de texto en lo que has añadido.").to_string(), true));
            return;
        }
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(files.len())));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (d2, c2) = (done.clone(), cancel.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut v = vec![];
            for f in files {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                v.push(if apply { convert_file(&f, &o) } else { analyse_file(&f, &o) });
                d2.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send((v, apply, started.elapsed().as_secs_f32()));
        });
        self.results.clear();
        self.status = None;
        self.job = Some(Job { rx, done, total, cancel, started: Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok((results, applied, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        let _ = job.started;
        self.job = None;
        self.applied = applied;
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        if applied {
            let changed = results.iter().filter(|r| r.changed).count();
            self.status = Some((trf!("Listo: {} archivo(s) modificados de {} · {} error(es) · {} s", changed, results.len(), bad, format!("{secs:.1}")), bad > 0 || cancelled));
            events.push(ToolEvent::Reload);
        } else {
            self.status = Some((trf!("Análisis: {} archivo(s) revisados · {} s", results.len(), format!("{secs:.1}")), cancelled));
        }
        self.results = results;
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("textconv_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.items_ui(ui, selection, cwd);
                ui.separator();
                self.options_ui(ui, cwd, selection);
            });
            ui.separator();
            self.action_ui(ui);
            self.results_ui(ui, events);
        });
        let ctx = ui.ctx().clone();
        let added = picker_window(&ctx, "textconv_picker", &mut self.picker);
        self.add(added);
    }

    fn items_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("ARCHIVOS DE TEXTO")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("textconv_items").max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
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
            if ui.add_enabled(!sel.is_empty(), egui::Button::new(trf!("➕ Añadir seleccionados ({})", sel.len()))).clicked() {
                self.add(sel);
            }
            if ui.button(tr("📂 Elegir archivos…")).clicked() {
                self.picker = Some(Picker::new(self.items.last().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| cwd.to_path_buf())));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
            ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas"));
        });
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("De las carpetas, solo")));
            ui.add(egui::TextEdit::singleline(&mut self.include).desired_width(520.0)).on_hover_text(tr("Patrones de nombre separados por «;». Los archivos que añadas uno a uno se usan siempre."));
        });
    }

    fn enc_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<Enc>, none: &str) {
        let shown = value.map(label).unwrap_or(none);
        egui::ComboBox::from_id_salt(id).width(260.0).selected_text(tr(shown)).show_ui(ui, |ui| {
            ui.selectable_value(value, None, tr(none));
            for (e, l) in ENCODINGS {
                ui.selectable_value(value, Some(e), l);
            }
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.label(RichText::new(tr("QUÉ CAMBIAR")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Los archivos están en")));
            Self::enc_combo(ui, "tc_from", &mut self.from, "Detectar automáticamente");
            ui.add_sized([60.0, 18.0], egui::Label::new(tr("Pasar a")));
            Self::enc_combo(ui, "tc_to", &mut self.to, "No cambiar la codificación");
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Saltos de línea")));
            for (k, l) in EOLS.iter().enumerate() {
                ui.radio_value(&mut self.eol, k as u8, tr(l));
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Tabuladores")));
            for (k, l) in TABS.iter().enumerate() {
                ui.radio_value(&mut self.tabs, k as u8, tr(l));
            }
            if self.tabs != 0 {
                ui.label(tr("ancho"));
                ui.add(egui::DragValue::new(&mut self.tab_width).range(1..=16));
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Final del archivo")));
            for (k, l) in FINAL.iter().enumerate() {
                ui.radio_value(&mut self.final_nl, k as u8, tr(l));
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Limpieza")));
            ui.checkbox(&mut self.trim, tr("Quitar espacios al final de cada línea"));
            ui.label(tr("Líneas vacías seguidas, como mucho"));
            ui.add(egui::DragValue::new(&mut self.blank_max).range(0..=20)).on_hover_text(tr("0 = no tocarlas"));
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(tr("Guardar")));
            ui.radio_value(&mut self.dest_other, false, tr("En los mismos archivos"));
            ui.radio_value(&mut self.dest_other, true, tr("En otra carpeta"));
            if self.dest_other {
                self.dest.ui(ui, cwd, selection);
                for (k, l) in CONFLICTS.iter().enumerate() {
                    ui.radio_value(&mut self.conflict, k as u8, tr(l));
                }
            } else {
                ui.checkbox(&mut self.backup, tr("Guardar una copia .bak del original")).on_hover_text(tr("Por si algo no sale como esperabas."));
            }
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy && !self.items.is_empty(), egui::Button::new(RichText::new(tr("🔍 Analizar")).strong())).on_hover_text(tr("Muestra cómo es cada archivo, sin tocar nada.")).clicked() {
                self.start(false);
            }
            let btn = egui::Button::new(RichText::new(tr("✏ Aplicar los cambios")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.items.is_empty(), btn).clicked() {
                self.start(true);
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let (d, t) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(180.0).show_percentage());
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
        egui::ScrollArea::both().id_salt("textconv_res").max_height(crate::tools::grow(ui, 300.0)).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("textconv_grid").striped(true).num_columns(8).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["Archivo", "Codificación", "Saltos", "Líneas", "Espacios al final", "Con tabuladores", "Final", "Estado"] {
                    ui.label(RichText::new(tr(h)).small().strong().color(theme::p().sub));
                }
                ui.end_row();
                for r in &self.results {
                    let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if ui.link(name).on_hover_text(r.path.to_string_lossy()).clicked() {
                        if let Some(p) = r.path.parent() {
                            events.push(ToolEvent::Navigate(p.to_path_buf()));
                        }
                    }
                    let a = &r.before;
                    if r.skipped.is_some() && a.lines == 0 {
                        for _ in 0..5 {
                            ui.label("");
                        }
                    } else {
                        ui.label(RichText::new(&a.enc).small());
                        ui.label(RichText::new(a.eol()).small().color(if a.eol() == "mixto" { theme::p().danger } else { theme::p().sub }));
                        ui.label(RichText::new(a.lines.to_string()).small());
                        ui.label(RichText::new(a.trailing_ws.to_string()).small());
                        ui.label(RichText::new(a.tabs.to_string()).small());
                        ui.label(RichText::new(if a.final_newline { tr("con salto") } else { tr("sin salto") }).small());
                    }
                    let (txt, col) = match (&r.error, &r.skipped) {
                        (Some(e), _) => (format!("✖ {e}"), theme::p().danger),
                        (_, Some(s)) => (s.clone(), theme::p().muted),
                        _ if self.applied && r.changed => (tr("✔ modificado").to_string(), theme::p().ok),
                        _ if self.applied => (tr("sin cambios").to_string(), theme::p().muted),
                        _ => (String::new(), theme::p().muted),
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

    fn opts() -> Opts {
        Opts { from: None, to: None, eol: 0, tabs: 0, tab_width: 4, trim: false, blank_max: 0, final_nl: 0, backup: false, dest_other: false, dest: PathBuf::new(), conflict: 0, include: String::new(), recursive: true }
    }

    #[test]
    fn line_endings_tabs_trim_blank_lines_and_final_newline() {
        let t = "uno  \r\ndos\t\n\n\n\ntres\r";
        let a = analyse(t, Enc::Utf8);
        assert_eq!((a.crlf, a.lf, a.cr, a.trailing_ws), (1, 4, 1, 2));
        assert_eq!(a.eol(), "mixto");
        let mut o = opts();
        o.eol = 1;
        assert_eq!(transform(t, &o), "uno  \ndos\t\n\n\n\ntres\n");
        o.eol = 2;
        o.trim = true;
        o.blank_max = 1;
        assert_eq!(transform(t, &o), "uno\r\ndos\r\n\r\ntres\r\n");
        // No change keeps every ending as it was, even mixed.
        assert_eq!(transform(t, &opts()), t);
        o = opts();
        o.final_nl = 2;
        assert_eq!(transform("a\nb\n", &o), "a\nb");
        o.final_nl = 1;
        assert_eq!(transform("a\r\nb", &o), "a\r\nb\n", "uses the file's own ending");
        o = opts();
        o.tabs = 1;
        assert_eq!(transform("\tx\ty", &o), "    x   y");
        o.tabs = 2;
        assert_eq!(transform("        x\n      y", &o), "\t\tx\n\t  y");
    }

    #[test]
    fn encodings_detect_convert_and_refuse_lossy() {
        let text = "Canción: ñandú — ✓\n";
        for e in [Enc::Utf8, Enc::Utf8Bom, Enc::Utf16Le, Enc::Utf16Be] {
            let b = encode(text, e).unwrap();
            let (back, det) = decode(&b, None).unwrap();
            assert_eq!((back.as_str(), det), (text, e), "{e:?}");
        }
        // Windows-1252 has «ñ» but not «✓»: refused instead of silently damaging the text.
        assert!(encode(text, Enc::Win1252).is_err());
        let latin = encode("Canción ñandú\n", Enc::Win1252).unwrap();
        assert_eq!(latin[5], 0xF3, "ó as a single byte");
        let (back, det) = decode(&latin, None).unwrap();
        assert_eq!((back.as_str(), det), ("Canción ñandú\n", Enc::Win1252), "not valid UTF-8, so it is read as Windows-1252");
        assert_eq!(decode(&encode("zażółć", Enc::Win1250).unwrap(), Some(Enc::Win1250)).unwrap().0, "zażółć");
        assert!(decode(&latin, Some(Enc::Utf8)).is_err());
    }

    #[test]
    fn converts_files_with_backup_and_leaves_unchanged_ones_alone() {
        let d = std::env::temp_dir().join(format!("tc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), b"hola\r\nmundo \r\n").unwrap();
        fs::write(d.join("limpio.txt"), b"ya\nlimpio\n").unwrap();
        fs::write(d.join("bin.dat"), [0u8, 1, 2, 0]).unwrap();
        let mut o = opts();
        o.eol = 1;
        o.trim = true;
        o.to = Some(Enc::Utf8Bom);
        o.backup = true;
        let r = convert_file(&d.join("a.txt"), &o);
        assert!(r.error.is_none() && r.changed, "{:?}", r.error);
        assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"\xEF\xBB\xBFhola\nmundo\n");
        assert_eq!(fs::read(d.join("a.txt.bak")).unwrap(), b"hola\r\nmundo \r\n");
        o.to = None;
        assert!(!convert_file(&d.join("limpio.txt"), &o).changed);
        assert!(convert_file(&d.join("bin.dat"), &o).skipped.is_some());
        // Into another folder, renaming on conflict.
        let mut o2 = opts();
        o2.eol = 2;
        o2.dest_other = true;
        o2.dest = d.join("salida");
        assert!(convert_file(&d.join("limpio.txt"), &o2).changed);
        assert_eq!(fs::read(d.join("salida/limpio.txt")).unwrap(), b"ya\r\nlimpio\r\n");
        assert!(convert_file(&d.join("limpio.txt"), &o2).out.ends_with("limpio (copia).txt"));
        o2.include = "*.txt".into();
        assert_eq!(collect(&[d.clone()], &o2).len(), 4, "a.txt, limpio.txt and the .bak is not a match; salida files count");
        fs::remove_dir_all(&d).unwrap();
    }
}
