//! File utilities in the spirit of a context-menu tool suite: each one opens in its own window
//! and works on the selected items (or the current folder).

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::theme;
use eframe::egui::{self, Color32, RichText};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    BatchRename,
    Checksum,
    Duplicates,
    Split,
    Join,
    DeleteByMask,
    RemoveEmpty,
    Sync,
    Compress,
    Extract,
    Organize,
    Grep,
    Images,
    Crypt,
    MakeFolders,
    TextConv,
    Replace,
    Meta,
    Compare,
    Integrity,
    Shred,
    ChangeDate,
    Attributes,
    CopyList,
    PackToFolder,
    Symlink,
}

pub const GROUPS: [(&str, &[Kind]); 4] = [
    ("Nombres y listas", &[Kind::BatchRename, Kind::CopyList, Kind::PackToFolder, Kind::Symlink]),
    ("Análisis", &[Kind::Checksum, Kind::Duplicates, Kind::Grep, Kind::Replace, Kind::TextConv, Kind::Meta, Kind::Compare, Kind::Integrity]),
    ("Organizar", &[Kind::Sync, Kind::Compress, Kind::Extract, Kind::Organize, Kind::MakeFolders, Kind::Images, Kind::Crypt, Kind::Split, Kind::Join, Kind::ChangeDate, Kind::Attributes]),
    ("Limpieza", &[Kind::DeleteByMask, Kind::RemoveEmpty, Kind::Shred]),
];

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::BatchRename => tr("Renombrado avanzado"),
            Kind::Checksum => tr("Suma de verificación"),
            Kind::Duplicates => tr("Buscar duplicados"),
            Kind::Split => tr("Dividir archivo"),
            Kind::Join => tr("Unir archivos"),
            Kind::DeleteByMask => tr("Eliminar por patrón"),
            Kind::RemoveEmpty => tr("Quitar carpetas vacías"),
            Kind::Sync => tr("Sincronizar carpetas"),
            Kind::Compress => tr("Comprimir"),
            Kind::Extract => tr("Descomprimir"),
            Kind::Organize => tr("Organizar por tipo o fecha"),
            Kind::Grep => tr("Buscar texto en archivos"),
            Kind::Images => tr("Imágenes: convertir y redimensionar"),
            Kind::Crypt => tr("Cifrar y descifrar"),
            Kind::MakeFolders => tr("Crear carpetas desde una lista"),
            Kind::TextConv => tr("Texto: saltos de línea y codificación"),
            Kind::Replace => tr("Buscar y reemplazar en archivos"),
            Kind::Meta => tr("Metadatos: EXIF y etiquetas"),
            Kind::Compare => tr("Comparar archivos"),
            Kind::Integrity => tr("Comprobar archivos dañados"),
            Kind::Shred => tr("Borrado seguro"),
            Kind::ChangeDate => tr("Cambiar fecha"),
            Kind::Attributes => tr("Atributos"),
            Kind::CopyList => tr("Árbol de carpetas y listados"),
            Kind::PackToFolder => tr("Agrupar en carpeta"),
            Kind::Symlink => tr("Crear enlace"),
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Kind::BatchRename => "✏",
            Kind::Checksum => "🔐",
            Kind::Duplicates => "♊",
            Kind::Split => "✂",
            Kind::Join => "🔗",
            Kind::DeleteByMask => "🗑",
            Kind::RemoveEmpty => "✖",
            Kind::Sync => "🔄",
            Kind::Compress => "📦",
            Kind::Extract => "📂",
            Kind::Organize => "📁",
            Kind::Grep => "🔎",
            Kind::Images => "🖼",
            Kind::Crypt => "🔒",
            Kind::MakeFolders => "➕",
            Kind::TextConv => "📝",
            Kind::Replace => "🔁",
            Kind::Meta => "🏷",
            Kind::Compare => "⚖",
            Kind::Integrity => "⚠",
            Kind::Shred => "🔥",
            Kind::ChangeDate => "🕓",
            Kind::Attributes => "🔒",
            Kind::CopyList => "🌲",
            Kind::PackToFolder => "📦",
            Kind::Symlink => "⤴",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Kind::BatchRename => tr("Cambia el nombre de muchos archivos a la vez: buscar y reemplazar, prefijos, numeración, mayúsculas."),
            Kind::Checksum => tr("Calcula CRC32, SHA-1, SHA-256, SHA-512 y BLAKE3 de muchos archivos y carpetas, y los compara con un valor, una lista o un archivo de sumas."),
            Kind::Duplicates => tr("Busca archivos repetidos en una o varias carpetas, por contenido, nombre o tamaño, y decide qué hacer con las copias."),
            Kind::Split => tr("Corta un archivo grande en partes de un tamaño fijo."),
            Kind::Join => tr("Vuelve a unir las partes (.001, .002…) en un solo archivo."),
            Kind::DeleteByMask => tr("Busca y borra archivos o carpetas por patrón (comodines o regex), tamaño y antigüedad, revisando antes la lista."),
            Kind::RemoveEmpty => tr("Busca y elimina las carpetas vacías (o que solo tienen archivos basura) de las carpetas que elijas, con o sin subcarpetas."),
            Kind::Extract => tr("Abre zip, 7z, tar.gz, tar.xz, tar.zst y más, con contraseña, en una carpeta propia y sin que ningún archivo se salga de ella."),
            Kind::Organize => tr("Reparte los archivos de una carpeta en subcarpetas por tipo, extensión, fecha, inicial, tamaño o reglas propias, con vista previa y deshacer."),
            Kind::Images => tr("Convierte entre PNG, JPEG, WebP, BMP, GIF y TIFF, cambia el tamaño, gira según el EXIF y procesa muchas fotos a la vez."),
            Kind::Crypt => tr("Protege archivos con una contraseña (XChaCha20-Poly1305 y Argon2id), con verificación y opción de ocultar el nombre."),
            Kind::MakeFolders => tr("Crea de golpe un árbol de carpetas a partir de una lista, con plantillas y vista previa."),
            Kind::TextConv => tr("Cambia los saltos de línea (LF, CRLF), la codificación (UTF-8, UTF-16, Windows-1252…), tabuladores y espacios sobrantes en muchos archivos de texto."),
            Kind::Integrity => tr("Busca fotos, comprimidos, PDF, música y JSON dañados o vacíos, leyéndolos enteros, y deja enviarlos a la papelera."),
            Kind::Compare => tr("Compara dos archivos de texto lado a lado, con las palabras cambiadas resaltadas, o dos archivos binarios byte a byte."),
            Kind::Meta => tr("Mira y edita el EXIF de las fotos y las etiquetas de la música en muchos archivos, y quita los metadatos o la ubicación GPS antes de compartir."),
            Kind::Replace => tr("Cambia un texto por otro dentro de muchos archivos a la vez, con expresiones regulares, vista previa y copia de seguridad."),
            Kind::Grep => tr("Busca un texto o una expresión regular dentro de los archivos y muestra las líneas que coinciden."),
            Kind::Compress => tr("Comprime en ZIP, 7z, TAR.GZ, TAR.XZ, TAR.ZST y más: todo en un paquete o uno por cada archivo, con contraseña y niveles."),
            Kind::Sync => tr("Mantiene dos carpetas al día: espejo, actualizar, solo añadir, bidireccional o mover, con vista previa de todo lo que se hará."),
            Kind::Shred => tr("Sobrescribe el contenido antes de borrar para que no se pueda recuperar."),
            Kind::ChangeDate => tr("Cambia la fecha de modificación de los elementos."),
            Kind::Attributes => tr("Marca como solo lectura o ejecutable."),
            Kind::CopyList => tr("Muestra las carpetas como un árbol (o una lista de nombres o rutas) con sus totales: archivos, carpetas, tamaño y tipos. Cópialo o guárdalo."),
            Kind::PackToFolder => tr("Mueve los elementos seleccionados a una carpeta nueva."),
            Kind::Symlink => tr("Crea un acceso directo (enlace simbólico) al elemento."),
        }
    }
    /// Whether the tool works without a selection (on the current folder).
    pub fn folder_ok(self) -> bool {
        matches!(self, Kind::Duplicates | Kind::DeleteByMask | Kind::RemoveEmpty | Kind::Sync | Kind::Compress | Kind::Extract | Kind::Organize | Kind::Grep | Kind::Images | Kind::Crypt | Kind::MakeFolders | Kind::TextConv | Kind::Replace | Kind::Meta | Kind::Compare | Kind::Integrity | Kind::CopyList | Kind::BatchRename | Kind::Checksum)
    }
}

pub enum ToolEvent {
    Reload,
    Clipboard(String),
    Navigate(PathBuf),
}

enum Out {
    Done(String),
}

enum Msg {
    Progress(String),
    Finished(Out, Vec<String>),
}

struct Job {
    rx: Receiver<Msg>,
    progress: String,
    cancel: Arc<AtomicBool>,
    executes: bool,
}

pub struct ToolWindow {
    target_picker: Option<crate::roots::Picker>,
    /// Frames this tool has been on screen as its own window (to bring it forward at first).
    frames_shown: u32,
    pub id: u64,
    pub kind: Kind,
    pub targets: Vec<PathBuf>,
    pub base: PathBuf,
    pub open: bool,
    // renombrado
    renamer: Option<crate::rename::Renamer>,
    dupes: Option<crate::dupes::Dupes>,
    syncer: Option<crate::sync::Syncer>,
    purge: Option<crate::purge::Purge>,
    compressor: Option<crate::compress::Compressor>,
    extractor: Option<crate::extract::Extractor>,
    organizer: Option<crate::organize::Organizer>,
    searcher: Option<crate::grep::Searcher>,
    imager: Option<crate::images::Imager>,
    crypter: Option<crate::crypt::Crypter>,
    mkdirs: Option<crate::mkdirs::MakeFolders>,
    textconv: Option<crate::textconv::TextConv>,
    replacer: Option<crate::replace::Replacer>,
    checksummer: Option<crate::checksum::Checksummer>,
    metatool: Option<crate::meta::MetaTool>,
    comparer: Option<crate::compare::Comparer>,
    integrity: Option<crate::integrity::Integrity>,
    emptydirs: Option<crate::emptydirs::EmptyDirs>,
    treetool: Option<crate::tree::TreeTool>,
    // dividir
    part_mb: f64,
    // patrón
    recursive: bool,
    // sincronizar
    dest: String,
    // borrado seguro
    passes: u32,
    confirm: bool,
    // fecha
    date_text: String,
    // atributos
    readonly: bool,
    executable: bool,
    // listas
    // agrupar / enlace
    name: String,
    // resultados
    job: Option<Job>,
    errors: Vec<String>,
    status: Option<(String, bool)>,
}

/// The indexes of the rows a text filter lets through, kept until the filter, the list or its
/// ordering change. With tens of thousands of rows, lowercasing every path on every frame is
/// what made the lists feel stuck.
#[derive(Default)]
pub struct ViewCache(Option<((String, u64), Vec<usize>)>);

impl ViewCache {
    /// `salt` must change whenever the list's content or order does (length, sort, toggles…).
    pub fn get(&mut self, filter: &str, salt: u64, build: impl FnOnce() -> Vec<usize>) -> Vec<usize> {
        if let Some(((f, s), v)) = &self.0 {
            if f == filter && *s == salt {
                return v.clone();
            }
        }
        let v = build();
        self.0 = Some(((filter.to_string(), salt), v.clone()));
        v
    }
}

/// Whether tools are drawn as windows inside the main one (screenshots, or the person's choice).
pub fn embedded(ctx: &egui::Context) -> bool {
    ctx.embed_viewports() || !crate::settings::separate_windows()
}

/// A list height that grows with the window it is in (`base` at 800 px tall, up to three times
/// that), so a tall tool window shows more rows instead of leaving blank space under the lists.
pub fn grow(ui: &egui::Ui, base: f32) -> f32 {
    base * (ui.ctx().screen_rect().height() / 800.0).clamp(1.0, 3.0)
}

impl ToolWindow {
    pub fn new(id: u64, kind: Kind, targets: Vec<PathBuf>, base: PathBuf) -> Self {
        let first_name = targets.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let now: chrono::DateTime<chrono::Local> = SystemTime::now().into();
        let mut t = ToolWindow {
            target_picker: None,
            frames_shown: 0,
            id,
            kind,
            base,
            open: true,
            renamer: None,
            dupes: None,
            syncer: None,
            purge: None,
            compressor: None,
            extractor: None,
            organizer: None,
            searcher: None,
            imager: None,
            crypter: None,
            mkdirs: None,
            textconv: None,
            replacer: None,
            checksummer: None,
            metatool: None,
            comparer: None,
            integrity: None,
            emptydirs: None,
            treetool: None,
            part_mb: 100.0,
            recursive: true,
            dest: String::new(),
            passes: 1,
            confirm: false,
            date_text: now.format("%d/%m/%Y %H:%M").to_string(),
            readonly: false,
            executable: false,
            name: match kind {
                Kind::PackToFolder => tr("Carpeta nueva").into(),
                Kind::Symlink => trf!("{} - enlace", first_name),
                _ => String::new(),
            },
            job: None,
            errors: vec![],
            status: None,
            targets,
        };
        if kind == Kind::Attributes {
            if let Some(m) = t.targets.first().and_then(|p| fs::metadata(p).ok()) {
                t.readonly = m.permissions().readonly();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    t.executable = m.permissions().mode() & 0o111 != 0;
                }
            }
        }
        if kind == Kind::DeleteByMask {
            t.purge = Some(crate::purge::Purge::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Extract {
            t.extractor = Some(crate::extract::Extractor::new(t.targets.clone()));
        }
        if kind == Kind::Organize {
            t.organizer = Some(crate::organize::Organizer::new(t.targets.clone()));
        }
        if kind == Kind::Images {
            t.imager = Some(crate::images::Imager::new(t.targets.clone()));
        }
        if kind == Kind::Crypt {
            t.crypter = Some(crate::crypt::Crypter::new(t.targets.clone()));
        }
        if kind == Kind::MakeFolders {
            t.mkdirs = Some(crate::mkdirs::MakeFolders::new(t.base.clone()));
        }
        if kind == Kind::TextConv {
            t.textconv = Some(crate::textconv::TextConv::new(t.targets.clone()));
        }
        if kind == Kind::Integrity {
            t.integrity = Some(crate::integrity::Integrity::new(t.targets.clone()));
        }
        if kind == Kind::Compare {
            t.comparer = Some(crate::compare::Comparer::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Meta {
            t.metatool = Some(crate::meta::MetaTool::new(t.targets.clone()));
        }
        if kind == Kind::Checksum {
            t.checksummer = Some(crate::checksum::Checksummer::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::RemoveEmpty {
            t.emptydirs = Some(crate::emptydirs::EmptyDirs::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Replace {
            t.replacer = Some(crate::replace::Replacer::new(t.targets.clone()));
        }
        if kind == Kind::Grep {
            t.searcher = Some(crate::grep::Searcher::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Compress {
            t.compressor = Some(crate::compress::Compressor::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Sync {
            t.syncer = Some(crate::sync::Syncer::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::Duplicates {
            t.dupes = Some(crate::dupes::Dupes::new(t.targets.clone(), t.base.clone()));
        }
        if kind == Kind::BatchRename {
            t.renamer = Some(crate::rename::Renamer::new(std::mem::take(&mut t.targets), t.base.clone()));
        }
        if kind == Kind::CopyList {
            t.treetool = Some(crate::tree::TreeTool::new(t.targets.clone(), t.base.clone()));
        }
        t
    }

    pub fn show(&mut self, ctx: &egui::Context, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let title = format!("{}  {}", self.kind.icon(), self.kind.label());
        let big = self.is_big();
        if embedded(ctx) {
            // No separate operating-system windows (screenshots, web): a window inside the main one.
            let mut open = self.open;
            let off = (self.id % 5) as f32 * 24.0;
            egui::Window::new(title)
                .id(egui::Id::new(("herramienta", self.id)))
                .open(&mut open)
                .collapsible(false)
                .default_width(if big { 1060.0 } else { 560.0 })
                .default_height(if big { 800.0 } else { 0.0 })
                .default_pos(ctx.screen_rect().center() - if big { egui::vec2(530.0, 330.0) } else { egui::vec2(280.0 - off, 240.0 - off) })
                .show(ctx, |ui| self.body(ctx, ui, events, selection, cwd, big));
            self.open = open;
        } else {
            // Its own operating-system window, so it can be resized past the main window,
            // moved to another screen and kept beside the file list.
            let id = egui::ViewportId::from_hash_of(("herramienta", self.id));
            let size = if big { [1060.0, 800.0] } else { [580.0, 540.0] };
            let builder = egui::ViewportBuilder::default()
                .with_title(title)
                .with_app_id(crate::APP_ID)
                .with_icon(std::sync::Arc::new(crate::icon::window_icon()))
                .with_inner_size(size)
                .with_min_inner_size([460.0, 320.0])
                .with_resizable(true);
            let first_frames = self.frames_shown < 3;
            self.frames_shown += 1;
            ctx.show_viewport_immediate(id, builder, |ctx, _class| {
                // Bring the new window to the front: some desktops open it behind the main one.
                if first_frames {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::p().base).inner_margin(egui::Margin::symmetric(12, 10))).show(ctx, |ui| self.body(ctx, ui, events, selection, cwd, big));
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.open = false;
                }
            });
        }
        if !self.open {
            if let Some(j) = &self.job {
                j.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn is_big(&self) -> bool {
        matches!(self.kind, Kind::BatchRename | Kind::Duplicates | Kind::Sync | Kind::DeleteByMask | Kind::Compress | Kind::Extract | Kind::Organize | Kind::Grep | Kind::Images | Kind::Crypt | Kind::MakeFolders | Kind::TextConv | Kind::Replace | Kind::Checksum | Kind::RemoveEmpty | Kind::Meta | Kind::Compare | Kind::Integrity | Kind::CopyList)
    }

    /// The tool drawn straight into `ui`, filling it (when it is the whole program's window).
    pub fn show_inline(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let big = self.is_big();
        self.body(ctx, ui, events, selection, cwd, big);
    }

    fn body(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path, big: bool) {
                ui.label(RichText::new(self.kind.description()).color(theme::p().sub));
                ui.add_space(4.0);
                if !big {
                    self.targets_ui(ui, selection);
                }
                ui.separator();
                match self.kind {
                    Kind::BatchRename => {
                        if let Some(r) = &mut self.renamer {
                            r.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Integrity => {
                        if let Some(x) = &mut self.integrity {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Compare => {
                        if let Some(x) = &mut self.comparer {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Meta => {
                        if let Some(x) = &mut self.metatool {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Checksum => {
                        if let Some(x) = &mut self.checksummer {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Duplicates => {
                        if let Some(d) = &mut self.dupes {
                            d.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Split => self.ui_split(ui),
                    Kind::Join => self.ui_join(ui),
                    Kind::DeleteByMask => {
                        if let Some(pg) = &mut self.purge {
                            pg.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::RemoveEmpty => {
                        if let Some(x) = &mut self.emptydirs {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Extract => {
                        if let Some(x) = &mut self.extractor {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Organize => {
                        if let Some(x) = &mut self.organizer {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Images => {
                        if let Some(x) = &mut self.imager {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Crypt => {
                        if let Some(x) = &mut self.crypter {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::MakeFolders => {
                        if let Some(x) = &mut self.mkdirs {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::TextConv => {
                        if let Some(x) = &mut self.textconv {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Replace => {
                        if let Some(x) = &mut self.replacer {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Grep => {
                        if let Some(x) = &mut self.searcher {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Compress => {
                        if let Some(c) = &mut self.compressor {
                            c.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Sync => {
                        if let Some(sy) = &mut self.syncer {
                            sy.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::Shred => self.ui_shred(ui),
                    Kind::ChangeDate => self.ui_date(ui, events),
                    Kind::Attributes => self.ui_attrs(ui, events),
                    Kind::CopyList => {
                        if let Some(x) = &mut self.treetool {
                            x.ui(ui, events, selection, cwd);
                        }
                    }
                    Kind::PackToFolder => self.ui_pack(ui, events),
                    Kind::Symlink => self.ui_symlink(ui, events),
                }
                if let Some(job) = &self.job {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&job.progress);
                        if ui.button(tr("Cancelar")).clicked() {
                            job.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                if let Some((s, err)) = &self.status {
                    ui.separator();
                    let col = if *err { theme::p().danger } else { theme::p().ok };
                    ui.label(RichText::new(s).color(col));
                }
                if !self.errors.is_empty() {
                    ui.collapsing(trf!("⚠ {} problema(s)", self.errors.len()), |ui| {
                        egui::ScrollArea::vertical().max_height(100.0).id_salt("errores").show(ui, |ui| {
                            for e in &self.errors {
                                ui.label(RichText::new(e).small().color(theme::p().danger));
                            }
                        });
                    });
                }
    }

    pub fn dupes_demo(&mut self) {
        if let Some(d) = &mut self.dupes {
            d.demo();
        }
    }

    pub fn purge_demo(&mut self, dir: &Path) {
        if let Some(pg) = &mut self.purge {
            pg.demo(dir);
        }
    }

    pub fn extract_demo(&mut self, dir: &Path) {
        if let Some(x) = &mut self.extractor {
            x.demo(dir);
        }
    }

    pub fn organize_demo(&mut self, dir: &Path) {
        if let Some(x) = &mut self.organizer {
            x.demo(dir);
        }
    }

    pub fn grep_demo(&mut self, dir: &Path, word: &str) {
        if let Some(x) = &mut self.searcher {
            x.demo(dir, word);
        }
    }

    pub fn new_demo(&mut self, dir: &Path) {
        if let Some(x) = &mut self.imager {
            x.demo(dir);
        }
        if let Some(x) = &mut self.crypter {
            x.demo(dir);
        }
        if let Some(x) = &mut self.mkdirs {
            x.demo(dir);
        }
        if let Some(x) = &mut self.textconv {
            x.demo(dir);
        }
        if let Some(x) = &mut self.replacer {
            x.demo(dir);
        }
        if let Some(x) = &mut self.checksummer {
            x.demo(dir);
        }
        if let Some(x) = &mut self.metatool {
            x.demo(dir);
        }
        if let Some(x) = &mut self.comparer {
            x.demo(dir);
        }
        if let Some(x) = &mut self.integrity {
            x.demo(dir);
        }
        if let Some(x) = &mut self.emptydirs {
            x.demo(dir);
        }
    }

    pub fn compress_demo(&mut self, dir: &Path) {
        if let Some(c) = &mut self.compressor {
            c.demo(dir);
        }
    }

    pub fn sync_demo(&mut self, a: &Path, b: &Path) {
        if let Some(sy) = &mut self.syncer {
            sy.demo(a, b);
        }
    }

    pub fn rename_demo(&mut self) {
        if let Some(r) = &mut self.renamer {
            r.demo();
        }
    }

    /// What the tool works on, with «Examinar…» and «Usar los seleccionados» to change it:
    /// the same line in every simple tool.
    fn targets_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf]) {
        let text = if self.targets.is_empty() {
            trf!("En la carpeta: {}", self.base.display())
        } else if self.targets.len() == 1 {
            self.targets[0].display().to_string()
        } else {
            let names: Vec<String> = self.targets.iter().take(3).map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()).collect();
            let more = if self.targets.len() > 3 { trf!(" y {} más", self.targets.len() - 3) } else { String::new() };
            trf!("{} elementos: {}{}", self.targets.len(), names.join(", "), more)
        };
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Sobre"));
            if ui.button(tr("📂 Examinar…")).clicked() {
                let start = self.targets.first().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| self.base.clone());
                self.target_picker = Some(crate::roots::Picker::new(start));
            }
            if !selection.is_empty() && selection != self.targets.as_slice() && ui.button(trf!("Usar los seleccionados ({})", selection.len())).clicked() {
                self.targets = selection.to_vec();
            }
            ui.add(egui::Label::new(RichText::new(text)).truncate()).on_hover_text(self.targets.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
        });
        let ctx = ui.ctx().clone();
        let picked = crate::roots::picker_window(&ctx, &format!("tool_targets_{}", self.id), &mut self.target_picker);
        if !picked.is_empty() {
            if let Some(p) = picked[0].parent() {
                self.base = p.to_path_buf();
            }
            self.targets = picked;
            self.target_picker = None;
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn set_status(&mut self, s: impl Into<String>, err: bool) {
        self.status = Some((s.into(), err));
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &mut self.job else { return };
        let mut finished = None;
        while let Ok(m) = job.rx.try_recv() {
            match m {
                Msg::Progress(p) => job.progress = p,
                Msg::Finished(out, errs) => finished = Some((out, errs)),
            }
        }
        if let Some((out, errs)) = finished {
            let executes = job.executes;
            self.job = None;
            self.errors = errs;
            let Out::Done(s) = out;
            self.set_status(s, false);
            if executes {
                events.push(ToolEvent::Reload);
            }
        }
    }

    fn spawn<F>(&mut self, executes: bool, f: F)
    where
        F: FnOnce(&Sender<Msg>, &AtomicBool) -> (Out, Vec<String>) + Send + 'static,
    {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let (out, errs) = f(&tx, &c2);
            let _ = tx.send(Msg::Finished(out, errs));
        });
        self.status = None;
        self.errors.clear();
        self.job = Some(Job { rx, progress: tr("Trabajando…").into(), cancel, executes });
    }

    fn run_button(&self, ui: &mut egui::Ui, label: &str, danger: bool) -> bool {
        let col = if danger { Color32::from_rgb(200, 50, 50) } else { theme::accent() };
        ui.add_enabled(!self.busy(), egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(col)).clicked()
    }

    // ---------------- Suma de verificación ----------------

    // ---------------- Duplicados ----------------

    // ---------------- Dividir / Unir ----------------

    fn ui_split(&mut self, ui: &mut egui::Ui) {
        let file = self.targets.iter().find(|p| p.is_file()).cloned();
        let Some(file) = file else {
            ui.label(tr("Selecciona un archivo para dividirlo."));
            return;
        };
        let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Tamaño de cada parte:").trim_end_matches([':', ' ']));
            ui.add(egui::DragValue::new(&mut self.part_mb).range(0.01..=100_000.0).speed(1.0).suffix(tr(" MB")));
            for (l, v) in [(tr("1,44 MB"), 1.44), (tr("100 MB"), 100.0), (tr("700 MB"), 700.0), (tr("4 GB"), 4095.0)] {
                if ui.small_button(l).clicked() {
                    self.part_mb = v;
                }
            }
        });
        let part = (self.part_mb * 1024.0 * 1024.0).max(1.0) as u64;
        let parts = size.div_ceil(part).max(1);
        let fname = file.file_name().unwrap_or_default().to_string_lossy().to_string();
        ui.label(trf!("{} » {} parte(s): {}.001, {}.002…", human_size(size), parts, fname, fname));
        if self.run_button(ui, tr("Dividir"), false) {
            self.spawn(true, move |tx, cancel| match split_file(&file, part, tx, cancel) {
                Ok(n) => (Out::Done(trf!("Archivo dividido en {} partes", n)), vec![]),
                Err(e) => (Out::Done(tr("No se completó la división").into()), vec![e.to_string()]),
            });
        }
    }

    fn ui_join(&mut self, ui: &mut egui::Ui) {
        let mut parts: Vec<PathBuf> = self.targets.iter().filter(|p| p.is_file()).cloned().collect();
        if parts.len() == 1 {
            // Given only the first part, find its siblings (.001, .002…).
            parts = find_parts(&parts[0]);
        }
        parts.sort();
        if parts.len() < 2 {
            ui.label(tr("Selecciona las partes (o solo la primera, .001) para unirlas."));
            return;
        }
        let first = parts[0].clone();
        let total: u64 = parts.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
        ui.label(trf!("{} partes, {} en total, en este orden:", parts.len(), human_size(total)));
        egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 150.0)).id_salt("parts").show(ui, |ui| {
            for p in &parts {
                ui.label(RichText::new(p.file_name().unwrap_or_default().to_string_lossy()).small());
            }
        });
        if self.name.is_empty() {
            self.name = strip_part_ext(&first);
        }
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Archivo resultante:").trim_end_matches([':', ' ']));
            ui.text_edit_singleline(&mut self.name);
        });
        if self.run_button(ui, tr("Unir"), false) {
            let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
            let out = fsops::unique_name(&dir, &self.name);
            self.spawn(true, move |tx, cancel| match join_files(&parts, &out, tx, cancel) {
                Ok(()) => (Out::Done(trf!("Creado {}", out.file_name().unwrap_or_default().to_string_lossy())), vec![]),
                Err(e) => {
                    let _ = fs::remove_file(&out);
                    (Out::Done(tr("No se completó la unión").into()), vec![e.to_string()])
                }
            });
        }
    }

    // ---------------- Eliminar por patrón / carpetas vacías ----------------

    // ---------------- Borrado seguro ----------------

    fn ui_shred(&mut self, ui: &mut egui::Ui) {
        if self.targets.is_empty() {
            ui.label(tr("Selecciona los archivos o carpetas que quieres destruir."));
            return;
        }
        let files = expand_files(&self.targets);
        let total: u64 = files.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
        ui.label(trf!("{} archivo(s), {}", files.len(), human_size(total)));
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Pasadas de sobrescritura:").trim_end_matches([':', ' ']));
            ui.add(egui::DragValue::new(&mut self.passes).range(1..=7));
        });
        ui.label(RichText::new(tr("En discos SSD y algunos sistemas de archivos no se garantiza que los datos desaparezcan del todo.")).small().color(theme::p().muted));
        ui.checkbox(&mut self.confirm, tr("Entiendo que esto no se puede deshacer"));
        if self.confirm && self.run_button(ui, tr("Destruir"), true) {
            let targets = self.targets.clone();
            let passes = self.passes;
            self.spawn(true, move |tx, cancel| {
                let mut errs = vec![];
                let mut ok = 0;
                for t in &targets {
                    for f in expand_files(std::slice::from_ref(t)) {
                        if cancel.load(Ordering::Relaxed) {
                            return (Out::Done(tr("Cancelado").into()), errs);
                        }
                        let _ = tx.send(Msg::Progress(trf!("Sobrescribiendo {}", f.file_name().unwrap_or_default().to_string_lossy())));
                        match shred_file(&f, passes) {
                            Ok(()) => ok += 1,
                            Err(e) => errs.push(format!("{}: {e}", f.display())),
                        }
                    }
                    if t.is_dir() {
                        if let Err(e) = fs::remove_dir_all(t) {
                            errs.push(format!("{}: {e}", t.display()));
                        }
                    }
                }
                (Out::Done(trf!("{} archivo(s) destruidos", ok)), errs)
            });
            self.targets.clear();
            self.confirm = false;
        }
    }

    // ---------------- Fecha / atributos ----------------

    fn ui_date(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.targets.is_empty() {
            ui.label(tr("Selecciona los elementos a los que cambiar la fecha."));
            return;
        }
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Nueva fecha de modificación:").trim_end_matches([':', ' ']));
            ui.add(egui::TextEdit::singleline(&mut self.date_text).desired_width(140.0)).on_hover_text(tr("dd/mm/aaaa hh:mm"));
            if ui.small_button(tr("Ahora")).clicked() {
                let now: chrono::DateTime<chrono::Local> = SystemTime::now().into();
                self.date_text = now.format("%d/%m/%Y %H:%M").to_string();
            }
        });
        ui.checkbox(&mut self.recursive, tr("Aplicar también al contenido de las carpetas"));
        let parsed = chrono::NaiveDateTime::parse_from_str(self.date_text.trim(), "%d/%m/%Y %H:%M")
            .ok()
            .and_then(|n| n.and_local_timezone(chrono::Local).single())
            .map(SystemTime::from);
        let Some(t) = parsed else {
            ui.colored_label(theme::p().danger, tr("Escribe la fecha como dd/mm/aaaa hh:mm"));
            return;
        };
        if self.run_button(ui, tr("Aplicar"), false) {
            let mut items = self.targets.clone();
            if self.recursive {
                let never = AtomicBool::new(false);
                for d in self.targets.iter().filter(|p| p.is_dir()) {
                    walk(d, true, &never, &mut |p, _| items.push(p.to_path_buf()));
                }
            }
            let mut errs = vec![];
            for p in &items {
                if let Err(e) = set_mtime(p, t) {
                    errs.push(format!("{}: {e}", p.display()));
                }
            }
            self.set_status(trf!("Fecha cambiada en {} elemento(s)", items.len() - errs.len()), false);
            self.errors = errs;
            events.push(ToolEvent::Reload);
        }
    }

    fn ui_attrs(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.targets.is_empty() {
            ui.label(tr("Selecciona los elementos."));
            return;
        }
        ui.checkbox(&mut self.readonly, tr("Solo lectura"));
        #[cfg(unix)]
        ui.checkbox(&mut self.executable, tr("Ejecutable (se puede abrir como programa)"));
        if self.run_button(ui, tr("Aplicar"), false) {
            let mut errs = vec![];
            for p in &self.targets {
                let r = fs::metadata(p).and_then(|m| {
                    let mut perm = m.permissions();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mut mode = perm.mode();
                        if self.readonly {
                            mode &= !0o222;
                        } else {
                            mode |= 0o200;
                        }
                        if self.executable {
                            mode |= (mode & 0o444) >> 2;
                        } else if !m.is_dir() {
                            mode &= !0o111;
                        }
                        perm.set_mode(mode);
                    }
                    #[cfg(not(unix))]
                    perm.set_readonly(self.readonly);
                    fs::set_permissions(p, perm)
                });
                if let Err(e) = r {
                    errs.push(format!("{}: {e}", p.display()));
                }
            }
            self.set_status(trf!("Atributos cambiados en {} elemento(s)", self.targets.len() - errs.len()), !errs.is_empty());
            self.errors = errs;
            events.push(ToolEvent::Reload);
        }
    }

    // ---------------- Listas, agrupar y enlaces ----------------

    fn ui_pack(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        if self.targets.is_empty() {
            ui.label(tr("Selecciona los elementos que quieres agrupar."));
            return;
        }
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Nombre de la carpeta:").trim_end_matches([':', ' ']));
            ui.text_edit_singleline(&mut self.name);
        });
        let bad = self.name.trim().is_empty() || self.name.contains('/') || self.name.contains('\\');
        if !bad && self.run_button(ui, tr("Agrupar"), false) {
            let dir = self.base.join(self.name.trim());
            let mut errs = vec![];
            if let Err(e) = fs::create_dir_all(&dir) {
                errs.push(e.to_string());
            } else {
                for p in &self.targets {
                    if p == &dir {
                        continue;
                    }
                    let to = fsops::unique_name(&dir, &p.file_name().unwrap_or_default().to_string_lossy());
                    if let Err(e) = fsops::move_path(p, &to) {
                        errs.push(format!("{}: {e}", p.display()));
                    }
                }
            }
            self.set_status(trf!("{} elemento(s) movidos a «{}»", self.targets.len() - errs.len(), self.name.trim()), false);
            self.errors = errs;
            self.targets.clear();
            events.push(ToolEvent::Reload);
        }
    }

    fn ui_symlink(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let Some(target) = self.targets.first().cloned() else {
            ui.label(tr("Selecciona el elemento al que quieres enlazar."));
            return;
        };
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Nombre del enlace:").trim_end_matches([':', ' ']));
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Crear en:").trim_end_matches([':', ' ']));
            ui.add(egui::TextEdit::singleline(&mut self.dest).hint_text(self.base.to_string_lossy()).desired_width(f32::INFINITY));
        });
        if self.run_button(ui, tr("Crear enlace"), false) {
            let dir = if self.dest.trim().is_empty() { self.base.clone() } else { PathBuf::from(self.dest.trim()) };
            let link = dir.join(self.name.trim());
            match make_symlink(&target, &link) {
                Ok(()) => {
                    self.set_status(trf!("Enlace creado: {}", link.display()), false);
                    events.push(ToolEvent::Reload);
                }
                Err(e) => self.set_status(trf!("No se pudo crear el enlace: {}", e), true),
            }
        }
    }
}

// ---------------- helpers ----------------

fn walk(dir: &Path, recursive: bool, cancel: &AtomicBool, f: &mut dyn FnMut(&Path, bool)) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for item in rd.flatten() {
            let p = item.path();
            let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
            f(&p, is_dir);
            if is_dir && recursive {
                stack.push(p);
            }
        }
    }
}

fn expand_files(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    let never = AtomicBool::new(false);
    for t in targets {
        if t.is_dir() {
            walk(t, true, &never, &mut |p, is_dir| {
                if !is_dir {
                    out.push(p.to_path_buf());
                }
            });
        } else if t.is_file() {
            out.push(t.clone());
        }
    }
    out.sort();
    out
}

fn split_file(file: &Path, part: u64, tx: &Sender<Msg>, cancel: &AtomicBool) -> io::Result<u64> {
    let mut f = fs::File::open(file)?;
    let size = f.metadata()?.len();
    let parts = size.div_ceil(part).max(1);
    let mut buf = vec![0u8; 1 << 20];
    for k in 1..=parts {
        let name = format!("{}.{:03}", file.file_name().unwrap_or_default().to_string_lossy(), k);
        let mut out = fs::File::create(file.with_file_name(name))?;
        let mut left = part.min(size - (k - 1) * part);
        while left > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other("cancelado"));
            }
            let want = left.min(buf.len() as u64) as usize;
            let n = f.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            left -= n as u64;
        }
        let _ = tx.send(Msg::Progress(trf!("Parte {} de {}", k, parts)));
    }
    Ok(parts)
}

fn find_parts(first: &Path) -> Vec<PathBuf> {
    let base = strip_part_ext(first);
    let dir = first.parent().unwrap_or(Path::new("."));
    let mut v = vec![];
    for k in 1..10000 {
        let p = dir.join(format!("{base}.{k:03}"));
        if p.exists() {
            v.push(p);
        } else {
            break;
        }
    }
    if v.is_empty() {
        v.push(first.to_path_buf());
    }
    v
}

fn strip_part_ext(p: &Path) -> String {
    let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
    match name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_digit()) => stem.to_string(),
        _ => format!("{name}.unido"),
    }
}

fn join_files(parts: &[PathBuf], out: &Path, tx: &Sender<Msg>, cancel: &AtomicBool) -> io::Result<()> {
    let mut o = fs::File::create(out)?;
    for (k, p) in parts.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelado"));
        }
        let _ = tx.send(Msg::Progress(trf!("Parte {} de {}", k + 1, parts.len())));
        let mut f = fs::File::open(p)?;
        io::copy(&mut f, &mut o)?;
    }
    o.sync_all()
}

/// Adds `dir` (after its empty sub-folders) when nothing but empty folders is inside it.
pub(crate) fn shred_file(p: &Path, passes: u32) -> io::Result<()> {
    use std::io::Seek;
    let len = fs::metadata(p)?.len();
    {
        let mut f = fs::OpenOptions::new().write(true).open(p)?;
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15 ^ len;
        let mut buf = vec![0u8; 1 << 16];
        for pass in 0..passes {
            f.seek(io::SeekFrom::Start(0))?;
            let mut left = len;
            while left > 0 {
                let n = left.min(buf.len() as u64) as usize;
                if pass + 1 == passes && passes > 1 {
                    buf[..n].fill(0);
                } else {
                    for chunk in buf[..n].chunks_mut(8) {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let b = state.to_le_bytes();
                        chunk.copy_from_slice(&b[..chunk.len()]);
                    }
                }
                f.write_all(&buf[..n])?;
                left -= n as u64;
            }
            f.sync_all()?;
        }
        f.set_len(0)?;
    }
    let renamed = p.with_file_name(format!("{:016x}", std::process::id() as u64 ^ len));
    let p = if fs::rename(p, &renamed).is_ok() { renamed } else { p.to_path_buf() };
    fs::remove_file(p)
}

fn set_mtime(p: &Path, t: SystemTime) -> io::Result<()> {
    let f = if p.is_dir() { fs::File::open(p)? } else { fs::OpenOptions::new().write(true).open(p).or_else(|_| fs::File::open(p))? };
    f.set_modified(t)
}

fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    if link.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, tr("ya existe un elemento con ese nombre")));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_join() {
        let dir = std::env::temp_dir().join(format!("adm-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("datos.bin");
        let data: Vec<u8> = (0..10_000u32).map(|x| (x % 251) as u8).collect();
        fs::write(&f, &data).unwrap();
        let (tx, _rx) = channel();
        let never = AtomicBool::new(false);
        assert_eq!(split_file(&f, 3000, &tx, &never).unwrap(), 4);
        let parts = find_parts(&dir.join("datos.bin.001"));
        assert_eq!(parts.len(), 4);
        let out = dir.join("unido.bin");
        join_files(&parts, &out, &tx, &never).unwrap();
        assert_eq!(fs::read(&out).unwrap(), data);
        fs::remove_dir_all(&dir).unwrap();
    }
}
