macro_rules! trf {
    ($s:literal $(, $a:expr)* $(,)?) => {
        crate::i18n::fill(crate::i18n::tr($s), &[$(&$a as &dyn std::fmt::Display),*])
    };
}

mod compress;
mod dupes;
mod extract;
mod crypt;
mod grep;
mod images;
mod icon;
mod integration;
mod integrity;
mod checksum;
mod compare;
mod emptydirs;
mod meta;
mod mkdirs;
mod sources;
mod netpath;
mod quicklook;
mod textconv;
mod fsops;
mod rename;
mod i18n;
mod organize;
mod purge;
mod replace;
mod roots;
mod settings;
mod sync;
mod theme;
mod tools;
mod ui_kit;

use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, RichText, Sense, Stroke, Vec2};
use fsops::{human_size, Entry};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tools::{Kind, ToolEvent, ToolWindow};

use i18n::tr;

fn main() -> eframe::Result<()> {
    settings::load();
    let args: Vec<String> = std::env::args().collect();
    // `--install-menu` / `--uninstall-menu`: the system's right-click menu, without opening the window.
    if let Some(a) = args.iter().find(|a| *a == "--install-menu" || *a == "--uninstall-menu") {
        let r = if a == "--install-menu" { integration::install() } else { integration::uninstall() };
        match r {
            Ok(done) => done.iter().for_each(|d| println!("{d}")),
            Err(e) => eprintln!("{e}"),
        }
        return Ok(());
    }
    // `--export-icon <carpeta>` writes the icon files used for packaging (png sizes and .ico).
    if let Some(p) = args.iter().position(|a| a == "--export-icon") {
        let dir = args.get(p + 1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("assets"));
        match icon::export(&dir) {
            Ok(()) => println!("Iconos escritos en {}", dir.display()),
            Err(e) => eprintln!("No se pudo escribir el icono: {e}"),
        }
        return Ok(());
    }
    let mut screenshot: Option<PathBuf> = None;
    let mut start_dir: Option<PathBuf> = None;
    let mut open_rename = false;
    let mut open_dupes = false;
    let mut open_sync = false;
    let mut open_purge = false;
    let mut open_compress = false;
    let mut open_tool: Option<&'static str> = None;
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "--lang" || args[i] == "--theme") && i + 1 < args.len() {
            // Try a language or theme for this run only (not saved).
            let v = &args[i + 1];
            if args[i] == "--lang" {
                if let Some(l) = i18n::LANGS.iter().position(|(c, _)| c == v) {
                    i18n::set_lang(l);
                }
            } else {
                theme::set_theme(v.parse().unwrap_or(0));
            }
            i += 2;
        } else if let Some(t) = ["--extract", "--organize", "--grep", "--images", "--crypt", "--mkdirs", "--text", "--replace", "--checksum", "--empty", "--meta", "--compare", "--integrity"].iter().find(|t| **t == args[i]) {
            open_tool = Some(t);
            i += 1;
        } else if args[i] == "--compress" {
            open_compress = true;
            i += 1;
        } else if args[i] == "--purge" {
            open_purge = true;
            i += 1;
        } else if args[i] == "--sync" {
            open_sync = true;
            i += 1;
        } else if args[i] == "--dupes" {
            open_dupes = true;
            i += 1;
        } else if args[i] == "--rename" {
            // Open the batch rename tool on start (handy for trying it).
            open_rename = true;
            i += 1;
        } else if args[i] == "--tool" && i + 1 < args.len() {
            // From the system's right-click menu: `--tool <id> <files…>` opens that tool on them.
            if let Some(kind) = integration::kind_of(&args[i + 1]) {
                let paths: Vec<PathBuf> = args[i + 2..].iter().map(PathBuf::from).filter(|p| p.exists()).collect();
                if let Some(first) = paths.first() {
                    let only_folder = paths.len() == 1 && first.is_dir();
                    start_dir = if only_folder { Some(first.clone()) } else { first.parent().map(Path::to_path_buf) };
                }
                let _ = LAUNCH.set((kind, paths));
            }
            break;
        } else if args[i] == "--screenshot" && i + 1 < args.len() {
            screenshot = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else {
            start_dir = Some(PathBuf::from(&args[i]));
            i += 1;
        }
    }

    // Two graphics engines: OpenGL and wgpu (DirectX 12 / Vulkan / Metal). The first that can
    // start is used; on Windows wgpu goes first because DirectX 12 also brings Windows' own
    // software drawing (WARP), which works in virtual machines with no 3D acceleration.
    // FICHERON_RENDERER=glow|wgpu forces one of them.
    let forced = std::env::var("FICHERON_RENDERER").ok().map(|v| v.to_lowercase());
    let order: Vec<eframe::Renderer> = match forced.as_deref() {
        Some("glow") | Some("opengl") => vec![eframe::Renderer::Glow],
        Some("wgpu") => vec![eframe::Renderer::Wgpu],
        _ if cfg!(windows) => vec![eframe::Renderer::Wgpu, eframe::Renderer::Glow],
        _ => vec![eframe::Renderer::Glow, eframe::Renderer::Wgpu],
    };
    // Opened from the system's right-click menu: the window is just that tool.
    let launch_title = LAUNCH.get().map(|(k, _)| format!("{} — {APP_NAME}", k.label())).unwrap_or_else(|| APP_NAME.to_string());
    if screenshot.is_none() {
        integration::ensure_desktop_entry();
    }
    let mut last: eframe::Result<()> = Ok(());
    for renderer in order {
        let mut viewport = egui::ViewportBuilder::default()
            .with_title(launch_title.clone())
            .with_app_id(APP_ID)
            .with_icon(std::sync::Arc::new(icon::window_icon()))
            .with_inner_size(if LAUNCH.get().is_some() { [1100.0, 820.0] } else { [1240.0, 760.0] })
            .with_min_inner_size([720.0, 440.0]);
        if screenshot.is_some() {
            viewport = viewport.with_active(false);
            #[cfg(target_os = "windows")]
            {
                viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnBottom);
            }
        }
        #[allow(unused_mut)]
        let mut options = eframe::NativeOptions { viewport, renderer, ..Default::default() };
        if renderer == eframe::Renderer::Wgpu {
            options.wgpu_options = software_friendly_wgpu();
        }
        #[cfg(target_os = "macos")]
        if screenshot.is_some() {
            options.event_loop_builder = Some(Box::new(|b| {
                use winit::platform::macos::EventLoopBuilderExtMacOS;
                b.with_activate_ignoring_other_apps(false);
            }));
        }
        let (sd, sc, ot) = (start_dir.clone(), screenshot.clone(), open_tool);
        last = eframe::run_native(
            APP_NAME,
            options,
            Box::new(move |cc| Ok(Box::new(App::new(cc, sd, sc, open_rename, open_dupes, open_sync, open_purge, open_compress, ot)))),
        );
        match &last {
            Ok(()) => return Ok(()),
            Err(e) => eprintln!("{APP_NAME}: el motor gráfico {renderer:?} no pudo arrancar ({e}); se prueba el siguiente."),
        }
    }
    last
}

/// wgpu setup that takes any adapter it can use: a real graphics card first and, failing that,
/// the software one (WARP on Windows, llvmpipe on Linux).
fn software_friendly_wgpu() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
    let mut setup = WgpuSetupCreateNew::default();
    setup.native_adapter_selector = Some(std::sync::Arc::new(|adapters, surface| {
        let usable: Vec<&eframe::egui_wgpu::wgpu::Adapter> = adapters.iter().filter(|a| surface.map(|s| a.is_surface_supported(s)).unwrap_or(true)).collect();
        let rank = |a: &eframe::egui_wgpu::wgpu::Adapter| match a.get_info().device_type {
            eframe::egui_wgpu::wgpu::DeviceType::DiscreteGpu => 0,
            eframe::egui_wgpu::wgpu::DeviceType::IntegratedGpu => 1,
            eframe::egui_wgpu::wgpu::DeviceType::VirtualGpu => 2,
            eframe::egui_wgpu::wgpu::DeviceType::Other => 3,
            eframe::egui_wgpu::wgpu::DeviceType::Cpu => 4,
        };
        usable.into_iter().min_by_key(|a| rank(a)).cloned().ok_or_else(|| "no hay ningún adaptador gráfico compatible".to_string())
    }));
    WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() }
}

const APP_NAME: &str = "Ficherón";
/// A tool to open on start, asked for from the system's right-click menu.
static LAUNCH: std::sync::OnceLock<(Kind, Vec<PathBuf>)> = std::sync::OnceLock::new();
/// Matches `packaging/ficheron.desktop` so Wayland shows the program's own icon.
const APP_ID: &str = "ficheron";

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Size,
    Kind,
    Modified,
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    List,
    Icons,
}

enum Dialog {
    NewFolder(String),
    NewFile(String),
    Rename { path: PathBuf, name: String },
    Delete { paths: Vec<PathBuf>, permanent: bool },
    Properties { entry: Entry, size: Option<(u64, usize)> },
}

enum Action {
    Navigate(PathBuf),
    Open(Entry),
    Click(usize, Modifiers),
    ClearSelection,
    SelectOnly(usize),
    Copy,
    Cut,
    Paste,
    Rename,
    Trash,
    DeletePermanently,
    NewFolder,
    NewFile,
    Properties,
    AddFavorite(PathBuf),
    RemoveFavorite(PathBuf),
    CopyPath,
    CopyNetPath,
    Terminal,
    Tool(Kind),
    Refresh,
    Back,
    Forward,
    Up,
    SelectAll,
    SortBy(SortBy),
    QuickLook,
    Integration,
    ToolPicker(egui::Pos2),
    CloseQuickLook,
    NewTab(Option<PathBuf>),
    CloseTab(usize),
    SwitchTab(usize),
    ClearRecent,
}

/// What a tab remembers while another one is shown.
#[derive(Clone)]
struct TabState {
    cwd: PathBuf,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    selection: BTreeSet<PathBuf>,
}

struct App {
    tabs: Vec<TabState>,
    active_tab: usize,
    recent: Vec<PathBuf>,
    cwd: PathBuf,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    entries: Vec<Entry>,
    visible: Vec<Entry>,
    load_error: Option<String>,
    selection: BTreeSet<PathBuf>,
    anchor: Option<usize>,
    editing_path: bool,
    path_text: String,
    filter: String,
    search_sub: bool,
    search_results: Option<Vec<Entry>>,
    sort_by: SortBy,
    ascending: bool,
    show_hidden: bool,
    view_mode: ViewMode,
    show_panel: bool,
    panel_tab: u8,
    clipboard: Option<(Vec<PathBuf>, bool)>,
    dialog: Option<Dialog>,
    message: Option<(String, bool, Instant)>,
    favorites: Vec<PathBuf>,
    preview: Option<(PathBuf, Option<String>)>,
    quick: Option<quicklook::QuickLook>,
    /// The «Menú contextual del sistema» window: open, and what the last action said.
    integration: Option<Option<(String, bool)>>,
    /// Started for one tool from the system's right-click menu: the window shows only that tool.
    solo: bool,
    /// The file explorer is shown; otherwise the start screen with the utilities.
    explorer: bool,
    logo: Option<egui::TextureHandle>,
    /// Where the «Utilidades…» panel of the right-click menu is open.
    tool_picker: Option<egui::Pos2>,
    tools: Vec<ToolWindow>,
    next_tool: u64,
    screenshot: Option<PathBuf>,
    frames: u32,
    shot_requested: bool,
}

fn config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("favoritos.txt"))
}

fn recent_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("recientes.txt"))
}

/// Short name of a folder for a tab or a list.
fn folder_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().to_string())
}

fn appearance_menu(ui: &mut egui::Ui) {
    let mut changed = false;
    ui.label(RichText::new(tr("Idioma")).small().color(theme::p().sub));
    for (i, (_, name)) in i18n::LANGS.iter().enumerate() {
        if ui.radio(i18n::lang() == i, *name).clicked() {
            i18n::set_lang(i);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(APP_NAME.into()));
            changed = true;
        }
    }
    ui.separator();
    ui.label(RichText::new(tr("Tema")).small().color(theme::p().sub));
    if ui.radio(theme::theme_index() == 0, tr("Automático (sistema)")).clicked() {
        theme::set_theme(0);
        changed = true;
    }
    for (i, pal) in theme::PALETTES.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui.radio(theme::theme_index() == i + 1, tr(pal.name)).clicked() {
                theme::set_theme(i + 1);
                changed = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for c in [pal.accent, pal.base, pal.bar].iter().rev() {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
                    ui.painter().circle(r.center(), 5.5, *c, Stroke::new(1.0_f32, theme::p().border));
                }
            });
        });
    }
    ui.separator();
    ui.label(RichText::new(tr("Tamaño de la interfaz")).small().color(theme::p().sub));
    // Buttons only (no slider, no drop-down): a slider moved under the pointer as the size
    // changed, and a drop-down inside this menu closed it before the choice counted.
    let ctx = ui.ctx().clone();
    let tip = tr("También con Ctrl + y Ctrl − (Ctrl 0 vuelve al 100 %)");
    let pct = (ctx.zoom_factor() * 100.0).round() as u32;
    let mut new: Option<f32> = None;
    ui.horizontal(|ui| {
        if ui.button("−").on_hover_text(tip).clicked() {
            new = Some(ctx.zoom_factor() - 0.1);
        }
        ui.label(RichText::new(format!("{pct} %")).strong());
        if ui.button("+").on_hover_text(tip).clicked() {
            new = Some(ctx.zoom_factor() + 0.1);
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for p in [75u32, 90, 100, 110, 125, 150, 175, 200] {
            if ui.selectable_label(p == pct, format!("{p} %")).clicked() {
                new = Some(p as f32 / 100.0);
            }
        }
    });
    if let Some(z) = new {
        let z = (z * 20.0).round() / 20.0;
        let z = z.clamp(0.5, 2.5);
        ctx.set_zoom_factor(z);
        settings::set_zoom(z);
    }
    ui.separator();
    let mut ex = settings::start_with_explorer();
    if ui.checkbox(&mut ex, tr("Empezar en el explorador de archivos")).on_hover_text(tr("Si no, al abrir se ve la pantalla con todas las utilidades.")).changed() {
        settings::set_start_with_explorer(ex);
    }
    let mut sep = settings::separate_windows();
    if ui.checkbox(&mut sep, tr("Herramientas en ventanas aparte")).on_hover_text(tr("Cada herramienta en su propia ventana, que se puede agrandar más que la principal y llevar a otra pantalla. Desmarcado: se abren dentro de la ventana principal, siempre por delante.")).changed() {
        settings::set_separate_windows(sep);
    }
    ui.separator();
    ui.label(RichText::new(tr("Color de acento")).small().color(theme::p().sub));
    ui.horizontal_wrapped(|ui| {
        for (i, (name, c)) in theme::ACCENTS.iter().enumerate() {
            let col = if i == 0 { theme::p().accent } else { *c };
            let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), Sense::click());
            ui.painter().circle_filled(r.center(), 9.0, col);
            if theme::accent_index() == i {
                ui.painter().circle_stroke(r.center(), 10.5, Stroke::new(2.0_f32, theme::p().text));
            }
            if resp.on_hover_text(tr(name)).clicked() {
                theme::set_accent(i);
                changed = true;
            }
        }
    });
    if changed {
        theme::apply(ui.ctx());
        settings::save();
    }
}

/// Text laid out to fit `max_w`: up to `rows` lines, the rest cut with «…».
fn fitted(ui: &egui::Ui, text: &str, size: f32, color: Color32, max_w: f32, rows: usize) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(size), color, max_w.max(10.0));
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = rows == 1;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|f| f.layout_job(job))
}

fn section_title(ui: &mut egui::Ui, s: &str) {
    ui.label(RichText::new(s).small().strong().color(theme::p().sub));
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>, screenshot: Option<PathBuf>, open_rename: bool, open_dupes: bool, open_sync: bool, open_purge: bool, open_compress: bool, open_tool: Option<&'static str>) -> Self {
        // Screenshots only see the main window, so tools are drawn inside it in that mode.
        if screenshot.is_some() {
            cc.egui_ctx.set_embed_viewports(true);
        }
        theme::apply(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(settings::zoom());
        cc.egui_ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
        });

        let favorites = config_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
            .unwrap_or_default();

        // A folder given on the command line opens the explorer there.
        let start_given = start.is_some() && LAUNCH.get().is_none();
        let cwd = start.filter(|p| p.is_dir()).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("/"));
        let cwd = fsops::canonical(&cwd);
        let recent: Vec<PathBuf> = recent_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        let mut app = App {
            tabs: vec![TabState { cwd: cwd.clone(), back: vec![], forward: vec![], selection: BTreeSet::new() }],
            active_tab: 0,
            recent,
            path_text: cwd.to_string_lossy().to_string(),
            cwd,
            back: vec![],
            forward: vec![],
            entries: vec![],
            visible: vec![],
            load_error: None,
            selection: BTreeSet::new(),
            anchor: None,
            editing_path: false,
            filter: String::new(),
            search_sub: false,
            search_results: None,
            sort_by: SortBy::Name,
            ascending: true,
            show_hidden: false,
            view_mode: ViewMode::List,
            show_panel: true,
            panel_tab: 0,
            clipboard: None,
            dialog: None,
            message: None,
            favorites,
            preview: None,
            quick: None,
            integration: None,
            tool_picker: None,
            solo: false,
            explorer: settings::start_with_explorer() || start_given,
            logo: None,
            tools: vec![],
            next_tool: 1,
            screenshot,
            frames: 0,
            shot_requested: false,
        };
        app.reload();
        if let Some((kind, paths)) = LAUNCH.get() {
            // A folder alone (or the background of a folder) means «this folder»; files are the targets.
            let targets = if paths.len() == 1 && paths[0].is_dir() && kind.folder_ok() && !matches!(kind, Kind::Compress | Kind::Checksum | Kind::Meta | Kind::Crypt | Kind::Images) { vec![] } else { paths.clone() };
            app.selection = paths.iter().cloned().collect();
            app.tools.push(ToolWindow::new(app.next_tool, *kind, targets, app.cwd.clone()));
            app.next_tool += 1;
            app.solo = true;
        }
        // For checking the «Utilidades…» panel in screenshots.
        if std::env::var("FICHERON_TOOLPICKER").is_ok() {
            app.tool_picker = Some(egui::pos2(260.0, 160.0));
        }
        // For checking the quick look in screenshots: FICHERON_QUICK=<file> opens it at start.
        if let Ok(p) = std::env::var("FICHERON_QUICK") {
            if let Some(e) = Entry::from_path(Path::new(&p)) {
                app.quick = Some(quicklook::QuickLook::open(&cc.egui_ctx, &e));
            }
        }
        if let Some(tool) = open_tool {
            let kind = match tool {
                "--extract" => Kind::Extract,
                "--organize" => Kind::Organize,
                "--images" => Kind::Images,
                "--crypt" => Kind::Crypt,
                "--mkdirs" => Kind::MakeFolders,
                "--text" => Kind::TextConv,
                "--replace" => Kind::Replace,
                "--checksum" => Kind::Checksum,
                "--empty" => Kind::RemoveEmpty,
                "--meta" => Kind::Meta,
                "--compare" => Kind::Compare,
                "--integrity" => Kind::Integrity,
                _ => Kind::Grep,
            };
            let mut t = ToolWindow::new(0, kind, vec![], app.cwd.clone());
            match kind {
                Kind::Extract => t.extract_demo(&app.cwd),
                Kind::Organize => t.organize_demo(&app.cwd),
                Kind::Images | Kind::Crypt | Kind::MakeFolders | Kind::TextConv | Kind::Replace | Kind::Checksum | Kind::RemoveEmpty | Kind::Meta | Kind::Compare | Kind::Integrity => t.new_demo(&app.cwd),
                _ => t.grep_demo(&app.cwd, "mundo"),
            }
            app.tools.push(t);
        }
        if open_compress {
            let mut t = ToolWindow::new(0, Kind::Compress, vec![], app.cwd.clone());
            t.compress_demo(&app.cwd);
            app.tools.push(t);
        }
        if open_purge {
            let mut t = ToolWindow::new(0, Kind::DeleteByMask, vec![], app.cwd.clone());
            t.purge_demo(&app.cwd);
            app.tools.push(t);
        }
        if open_sync {
            let mut t = ToolWindow::new(0, Kind::Sync, vec![], app.cwd.clone());
            t.sync_demo(&app.cwd.join("a"), &app.cwd.join("b"));
            app.tools.push(t);
        }
        if open_dupes {
            let mut t = ToolWindow::new(0, Kind::Duplicates, vec![], app.cwd.clone());
            t.dupes_demo();
            app.tools.push(t);
        }
        if open_rename {
            let mut t = ToolWindow::new(0, Kind::BatchRename, vec![], app.cwd.clone());
            t.rename_demo();
            app.tools.push(t);
        }
        app
    }

    fn save_favorites(&self) {
        if let Some(f) = config_file() {
            if let Some(d) = f.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let s: Vec<String> = self.favorites.iter().map(|p| p.to_string_lossy().to_string()).collect();
            let _ = std::fs::write(f, s.join("\n"));
        }
    }

    fn info(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), false, Instant::now()));
    }
    fn error(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), true, Instant::now()));
    }

    fn reload(&mut self) {
        match fsops::list_dir(&self.cwd) {
            Ok(e) => {
                self.entries = e;
                self.load_error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.load_error = Some(trf!("No se puede abrir esta carpeta: {}", e));
            }
        }
        if self.search_results.is_some() {
            self.run_search();
        }
        self.refresh_view();
    }

    fn run_search(&mut self) {
        if self.filter.trim().is_empty() || !self.search_sub {
            self.search_results = None;
        } else {
            self.search_results = Some(fsops::search(&self.cwd, self.filter.trim(), self.show_hidden, 2000));
        }
    }

    fn refresh_view(&mut self) {
        let q = self.filter.to_lowercase();
        let source = self.search_results.as_ref().unwrap_or(&self.entries);
        let mut v: Vec<Entry> = source
            .iter()
            .filter(|e| self.show_hidden || !e.hidden)
            .filter(|e| self.search_results.is_some() || q.is_empty() || e.name.to_lowercase().contains(&q))
            .cloned()
            .collect();
        let asc = self.ascending;
        let by = self.sort_by;
        v.sort_by(|a, b| {
            // Folders always first.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = match by {
                    SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortBy::Size => a.size.cmp(&b.size),
                    SortBy::Kind => a.kind().cmp(&b.kind()).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    SortBy::Modified => a.modified.cmp(&b.modified),
                };
                if asc {
                    o
                } else {
                    o.reverse()
                }
            })
        });
        self.visible = v;
        let still: BTreeSet<PathBuf> = self.visible.iter().map(|e| e.path.clone()).collect();
        self.selection.retain(|p| still.contains(p));
        if self.anchor.map(|a| a >= self.visible.len()).unwrap_or(false) {
            self.anchor = None;
        }
    }

    fn navigate(&mut self, p: PathBuf) {
        let p = fsops::canonical(&p);
        if !p.is_dir() {
            self.error(trf!("«{}» no es una carpeta", p.display()));
            return;
        }
        if p != self.cwd {
            self.back.push(std::mem::replace(&mut self.cwd, p));
            self.forward.clear();
        }
        self.after_move();
    }

    /// Saves the shown folder into its tab, so switching back finds it as it was.
    fn store_tab(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active_tab) {
            *t = TabState { cwd: self.cwd.clone(), back: self.back.clone(), forward: self.forward.clone(), selection: self.selection.clone() };
        }
    }

    fn show_tab(&mut self, k: usize) {
        let Some(t) = self.tabs.get(k).cloned() else { return };
        self.active_tab = k;
        self.cwd = t.cwd;
        self.back = t.back;
        self.forward = t.forward;
        self.after_move();
        let still: BTreeSet<PathBuf> = self.visible.iter().map(|e| e.path.clone()).collect();
        self.selection = t.selection.into_iter().filter(|p| still.contains(p)).collect();
    }

    fn remember_recent(&mut self) {
        let p = self.cwd.clone();
        self.recent.retain(|r| r != &p);
        self.recent.insert(0, p);
        self.recent.truncate(12);
        if let Some(f) = recent_file() {
            if let Some(d) = f.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let s: Vec<String> = self.recent.iter().map(|p| p.to_string_lossy().to_string()).collect();
            let _ = std::fs::write(f, s.join("\n"));
        }
    }

    fn after_move(&mut self) {
        if self.screenshot.is_none() {
            self.remember_recent();
        }
        if let Some(t) = self.tabs.get_mut(self.active_tab) {
            t.cwd = self.cwd.clone();
        }
        self.path_text = self.cwd.to_string_lossy().to_string();
        self.selection.clear();
        self.anchor = None;
        self.filter.clear();
        self.search_results = None;
        self.editing_path = false;
        self.reload();
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        if self.selection.is_empty() {
            return vec![];
        }
        self.visible.iter().filter(|e| self.selection.contains(&e.path)).map(|e| e.path.clone()).collect()
    }

    fn selected_entries(&self) -> Vec<&Entry> {
        if self.selection.is_empty() {
            return vec![];
        }
        self.visible.iter().filter(|e| self.selection.contains(&e.path)).collect()
    }

    fn paste(&mut self) {
        let Some((paths, cut)) = self.clipboard.clone() else {
            self.error(tr("No hay nada para pegar"));
            return;
        };
        let mut done = 0;
        let mut errors = vec![];
        for src in &paths {
            let Some(name) = src.file_name() else { continue };
            let name = name.to_string_lossy().to_string();
            if cut && src.parent() == Some(self.cwd.as_path()) {
                continue;
            }
            let dst = fsops::unique_name(&self.cwd, &name);
            let r = if cut { fsops::move_path(src, &dst) } else { fsops::copy_recursive(src, &dst) };
            match r {
                Ok(()) => done += 1,
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        if cut {
            self.clipboard = None;
        }
        self.reload();
        if errors.is_empty() {
            self.info(if cut { trf!("Movido(s) {} elemento(s)", done) } else { trf!("Copiado(s) {} elemento(s)", done) });
        } else {
            self.error(trf!("Algunos elementos fallaron: {}", errors.join("; ")));
        }
    }

    fn open_terminal(&mut self) {
        let dir = self.cwd.clone();
        #[cfg(target_os = "windows")]
        let r = std::process::Command::new("cmd").args(["/C", "start", "cmd"]).current_dir(&dir).spawn().map(|_| ());
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").args(["-a", "Terminal"]).arg(&dir).spawn().map(|_| ());
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = {
            let mut r = Err(std::io::Error::other(tr("no se encontró ningún terminal")));
            for t in ["x-terminal-emulator", "kgx", "gnome-terminal", "ptyxis", "konsole", "xfce4-terminal", "alacritty", "kitty", "xterm"] {
                if std::process::Command::new(t).current_dir(&dir).spawn().is_ok() {
                    r = Ok(());
                    break;
                }
            }
            r
        };
        match r {
            Ok(()) => self.info(tr("Terminal abierto en esta carpeta")),
            Err(e) => self.error(trf!("No se pudo abrir el terminal: {}", e)),
        }
    }

    fn apply(&mut self, a: Action, ctx: &egui::Context) {
        match a {
            Action::Navigate(p) => self.navigate(p),
            Action::Open(e) => {
                if e.is_dir {
                    self.navigate(e.path);
                } else if let Err(err) = fsops::open_with_system(&e.path) {
                    self.error(trf!("No se pudo abrir «{}»: {}", e.name, err));
                } else {
                    self.info(trf!("Abriendo «{}»…", e.name));
                }
            }
            Action::Click(i, m) => {
                let Some(e) = self.visible.get(i) else { return };
                let p = e.path.clone();
                if m.shift {
                    let a = self.anchor.unwrap_or(i);
                    let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
                    if !m.command {
                        self.selection.clear();
                    }
                    for e in &self.visible[lo..=hi] {
                        self.selection.insert(e.path.clone());
                    }
                } else if m.command {
                    if !self.selection.remove(&p) {
                        self.selection.insert(p);
                    }
                    self.anchor = Some(i);
                } else {
                    self.selection.clear();
                    self.selection.insert(p);
                    self.anchor = Some(i);
                }
            }
            Action::ClearSelection => {
                self.selection.clear();
                self.anchor = None;
            }
            Action::SelectOnly(i) => {
                if let Some(e) = self.visible.get(i) {
                    if !self.selection.contains(&e.path) {
                        self.selection.clear();
                        self.selection.insert(e.path.clone());
                        self.anchor = Some(i);
                    }
                }
            }
            Action::Copy | Action::Cut => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.error(tr("Selecciona algo primero"));
                } else {
                    let cut = matches!(a, Action::Cut);
                    self.info(if cut { trf!("{} elemento(s) cortados — usa Pegar en la carpeta de destino", paths.len()) } else { trf!("{} elemento(s) copiados — usa Pegar en la carpeta de destino", paths.len()) });
                    self.clipboard = Some((paths, cut));
                }
            }
            Action::Paste => self.paste(),
            Action::Rename => {
                if let Some(e) = self.selected_entries().first() {
                    self.dialog = Some(Dialog::Rename { path: e.path.clone(), name: e.name.clone() });
                }
            }
            Action::Trash | Action::DeletePermanently => {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    self.dialog = Some(Dialog::Delete { paths, permanent: matches!(a, Action::DeletePermanently) });
                }
            }
            Action::NewFolder => self.dialog = Some(Dialog::NewFolder(tr("Carpeta nueva").into())),
            Action::NewFile => self.dialog = Some(Dialog::NewFile(tr("Documento nuevo.txt").into())),
            Action::QuickLook => {
                if self.quick.is_some() {
                    self.quick = None;
                } else if let Some(e) = self.selected_entries().first().map(|e| (*e).clone()) {
                    self.quick = Some(quicklook::QuickLook::open(ctx, &e));
                }
            }
            Action::CloseQuickLook => self.quick = None,
            Action::Integration => self.integration = Some(None),
            Action::ToolPicker(p) => self.tool_picker = Some(p),
            Action::Properties => {
                let e = self.selected_entries().first().map(|e| (*e).clone()).or_else(|| Entry::from_path(&self.cwd));
                if let Some(entry) = e {
                    let size = if entry.is_dir {
                        let mut budget = 50_000;
                        Some(fsops::dir_size(&entry.path, &mut budget))
                    } else {
                        None
                    };
                    self.dialog = Some(Dialog::Properties { entry, size });
                }
            }
            Action::AddFavorite(p) => {
                if !self.favorites.contains(&p) {
                    self.favorites.push(p);
                    self.save_favorites();
                    self.info(tr("Añadido a favoritos"));
                }
            }
            Action::RemoveFavorite(p) => {
                self.favorites.retain(|f| f != &p);
                self.save_favorites();
            }
            Action::CopyPath => {
                let sel = self.selected_paths();
                let s: Vec<String> = if sel.is_empty() { vec![self.cwd.to_string_lossy().to_string()] } else { sel.iter().map(|p| p.to_string_lossy().to_string()).collect() };
                ctx.copy_text(s.join("\n"));
                self.info(tr("Ruta copiada al portapapeles"));
            }
            Action::CopyNetPath => {
                let sel = self.selected_paths();
                let list = if sel.is_empty() { vec![self.cwd.clone()] } else { sel };
                let nets: Vec<String> = list.iter().filter_map(|p| netpath::network_path(p)).collect();
                if nets.is_empty() {
                    self.error(tr("Esta ruta no está en una carpeta compartida de red"));
                } else {
                    ctx.copy_text(nets.join("\n"));
                    self.info(tr("Ruta de red copiada al portapapeles"));
                }
            }
            Action::Terminal => self.open_terminal(),
            Action::NewTab(p) => {
                self.store_tab();
                let cwd = p.filter(|p| p.is_dir()).unwrap_or_else(|| self.cwd.clone());
                self.tabs.insert(self.active_tab + 1, TabState { cwd, back: vec![], forward: vec![], selection: BTreeSet::new() });
                self.show_tab(self.active_tab + 1);
            }
            Action::CloseTab(k) => {
                if self.tabs.len() > 1 && k < self.tabs.len() {
                    self.store_tab();
                    self.tabs.remove(k);
                    let next = if k < self.active_tab || self.active_tab >= self.tabs.len() { self.active_tab.saturating_sub(1) } else { self.active_tab };
                    self.active_tab = usize::MAX;
                    self.show_tab(next.min(self.tabs.len() - 1));
                }
            }
            Action::SwitchTab(k) => {
                if k != self.active_tab && k < self.tabs.len() {
                    self.store_tab();
                    self.show_tab(k);
                }
            }
            Action::ClearRecent => {
                self.recent.clear();
                if let Some(f) = recent_file() {
                    let _ = std::fs::write(f, "");
                }
            }
            Action::Tool(kind) => {
                let targets = self.selected_paths();
                if targets.is_empty() && !kind.folder_ok() {
                    self.error(trf!("«{}» necesita que selecciones algún elemento", kind.label()));
                    return;
                }
                self.tools.push(ToolWindow::new(self.next_tool, kind, targets, self.cwd.clone()));
                self.next_tool += 1;
            }
            Action::Refresh => self.reload(),
            Action::Back => {
                if let Some(p) = self.back.pop() {
                    self.forward.push(std::mem::replace(&mut self.cwd, p));
                    self.after_move();
                }
            }
            Action::Forward => {
                if let Some(p) = self.forward.pop() {
                    self.back.push(std::mem::replace(&mut self.cwd, p));
                    self.after_move();
                }
            }
            Action::Up => {
                if let Some(p) = self.cwd.parent().map(Path::to_path_buf) {
                    self.navigate(p);
                }
            }
            Action::SelectAll => {
                self.selection = self.visible.iter().map(|e| e.path.clone()).collect();
            }
            Action::SortBy(s) => {
                if self.sort_by == s {
                    self.ascending = !self.ascending;
                } else {
                    self.sort_by = s;
                    self.ascending = true;
                }
                self.refresh_view();
            }
        }
    }

    // ---------- UI pieces ----------

    fn toolbar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            if ui.button(RichText::new(tr("◀ Inicio")).strong().color(theme::accent())).on_hover_text(tr("Volver a la pantalla de utilidades")).clicked() {
                self.explorer = false;
            }
            ui.separator();
            let nav = |ui: &mut egui::Ui, enabled: bool, icon: &str, tip: &str| ui.add_enabled(enabled, egui::Button::new(RichText::new(icon).size(16.0))).on_hover_text(tip).clicked();
            if nav(ui, !self.back.is_empty(), "⬅", tr("Atrás (Alt+←)")) {
                acts.push(Action::Back);
            }
            if nav(ui, !self.forward.is_empty(), "➡", tr("Adelante (Alt+→)")) {
                acts.push(Action::Forward);
            }
            if nav(ui, self.cwd.parent().is_some(), "⬆", tr("Carpeta superior (Retroceso)")) {
                acts.push(Action::Up);
            }
            if nav(ui, true, "🏠", tr("Carpeta personal")) {
                if let Some(h) = dirs::home_dir() {
                    acts.push(Action::Navigate(h));
                }
            }
            if nav(ui, true, "⟳", tr("Actualizar (F5)")) {
                acts.push(Action::Refresh);
            }

            let search_w = 270.0;
            let path_w = (ui.available_width() - search_w - 16.0).max(120.0);
            ui.allocate_ui_with_layout(egui::vec2(path_w, 26.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                self.path_bar(ui, acts);
            });
            let resp = ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("🔍 Buscar…")).desired_width(search_w - 46.0));
            if resp.changed() && !self.search_sub {
                self.refresh_view();
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && self.search_sub {
                self.run_search();
                self.refresh_view();
            }
            if ui.selectable_label(self.search_sub, "⤵").on_hover_text(tr("Buscar también dentro de las subcarpetas (pulsa Intro)")).clicked() {
                self.search_sub = !self.search_sub;
                self.run_search();
                self.refresh_view();
            }
        });
    }

    fn path_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        if self.editing_path {
            let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(ui.available_width()));
            if !r.has_focus() && !r.lost_focus() {
                r.request_focus();
            }
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    acts.push(Action::Navigate(PathBuf::from(self.path_text.trim())));
                }
                self.editing_path = false;
                self.path_text = self.cwd.to_string_lossy().to_string();
            }
            return;
        }
        egui::Frame::new()
            .fill(theme::p().base)
            .stroke(Stroke::new(1.0_f32, theme::p().border))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(4, 1))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let mut parts: Vec<(String, PathBuf)> = vec![];
                    let mut acc = PathBuf::new();
                    for c in self.cwd.components() {
                        acc.push(c);
                        let label = match c {
                            std::path::Component::RootDir => "🖴".to_string(),
                            other => other.as_os_str().to_string_lossy().to_string(),
                        };
                        if matches!(c, std::path::Component::RootDir) && !parts.is_empty() {
                            continue; // Windows: "C:" already shown.
                        }
                        parts.push((label, acc.clone()));
                    }
                    // Keep the end visible when the path is long.
                    let skip = parts.len().saturating_sub(7);
                    if skip > 0 {
                        ui.label(RichText::new("…").color(theme::p().muted));
                    }
                    let n = parts.len();
                    for (i, (label, p)) in parts.into_iter().enumerate().skip(skip) {
                        if i > skip || skip > 0 {
                            ui.label(RichText::new(" › ").color(theme::p().muted));
                        }
                        let txt = if i == n - 1 { RichText::new(label).strong() } else { RichText::new(label) };
                        if ui.add(egui::Button::new(txt).frame(false)).clicked() {
                            acts.push(Action::Navigate(p));
                        }
                    }
                    let rest = ui.available_rect_before_wrap();
                    let r = ui.allocate_rect(rest, Sense::click());
                    if r.clicked() {
                        self.editing_path = true;
                    }
                    r.on_hover_text(tr("Haz clic para escribir una ruta (Ctrl+L)"));
                });
            });
    }

    fn actions_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let has_sel = !self.selection.is_empty();
        let one = self.selection.len() == 1;
        ui.horizontal(|ui| {
            if ui.button(tr("📁 Nueva carpeta")).on_hover_text(tr("Ctrl+Mayús+N")).clicked() {
                acts.push(Action::NewFolder);
            }
            if ui.button(tr("🗋 Nuevo archivo")).clicked() {
                acts.push(Action::NewFile);
            }
            ui.separator();
            if ui.add_enabled(has_sel, egui::Button::new(tr("✂ Cortar"))).on_hover_text(tr("Ctrl+X")).clicked() {
                acts.push(Action::Cut);
            }
            if ui.add_enabled(has_sel, egui::Button::new(tr("🗐 Copiar"))).on_hover_text(tr("Ctrl+C")).clicked() {
                acts.push(Action::Copy);
            }
            let paste_label = match &self.clipboard {
                Some((p, _)) => trf!("📋 Pegar ({})", p.len()),
                None => tr("📋 Pegar").into(),
            };
            if ui.add_enabled(self.clipboard.is_some(), egui::Button::new(paste_label)).on_hover_text(tr("Ctrl+V")).clicked() {
                acts.push(Action::Paste);
            }
            ui.separator();
            if ui.add_enabled(one, egui::Button::new(tr("✏ Renombrar"))).on_hover_text("F2").clicked() {
                acts.push(Action::Rename);
            }
            if ui.add_enabled(has_sel, egui::Button::new(tr("🗑 Papelera"))).on_hover_text(tr("Supr")).clicked() {
                acts.push(Action::Trash);
            }
            ui.separator();
            ui.menu_button(RichText::new(tr("🔧 Utilidades ⏷")).strong().color(theme::accent()), |ui| {
                self.tools_menu(ui, acts);
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(tr("🎨 Apariencia"), |ui| appearance_menu(ui));
                if ui.selectable_label(self.show_panel, tr("Panel")).on_hover_text(tr("Panel lateral derecho")).clicked() {
                    self.show_panel = !self.show_panel;
                }
                if ui.selectable_label(self.view_mode == ViewMode::Icons, "⊞").on_hover_text(tr("Vista de iconos")).clicked() {
                    self.view_mode = ViewMode::Icons;
                }
                if ui.selectable_label(self.view_mode == ViewMode::List, "☰").on_hover_text(tr("Vista de lista")).clicked() {
                    self.view_mode = ViewMode::List;
                }
                ui.separator();
                if ui.checkbox(&mut self.show_hidden, tr("Ocultos")).on_hover_text(tr("Mostrar archivos ocultos (Ctrl+H)")).changed() {
                    self.run_search();
                    self.refresh_view();
                }
            });
        });
    }

    fn tools_menu(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let has_sel = !self.selection.is_empty();
        for (group, kinds) in tools::GROUPS {
            ui.label(RichText::new(tr(group)).small().color(theme::p().muted));
            for k in kinds.iter().copied() {
                let enabled = has_sel || k.folder_ok();
                if ui.add_enabled(enabled, egui::Button::new(format!("{}  {}", k.icon(), k.label()))).on_hover_text(k.description()).on_disabled_hover_text(tr("Selecciona primero algún elemento")).clicked() {
                    acts.push(Action::Tool(k));
                    ui.close_menu();
                }
            }
            ui.separator();
        }
        if ui.button(tr("🖥  Abrir terminal aquí")).clicked() {
            acts.push(Action::Terminal);
            ui.close_menu();
        }
        if ui.button(tr("🔗  Copiar ruta")).clicked() {
            acts.push(Action::CopyPath);
            ui.close_menu();
        }
        if ui.button(tr("🌐  Copiar ruta de red (UNC)")).on_hover_text(tr("Para carpetas compartidas SMB/NFS: \\\\servidor\\recurso\\carpeta")).clicked() {
            acts.push(Action::CopyNetPath);
            ui.close_menu();
        }
        ui.separator();
        if ui.button(tr("🖱  Menú contextual del sistema…")).on_hover_text(tr("Añade las herramientas al clic derecho del explorador de archivos del sistema")).clicked() {
            acts.push(Action::Integration);
            ui.close_menu();
        }
    }

    /// The file explorer: toolbar, tabs, places, the file list and the side panel.
    fn explorer_panels(&mut self, ctx: &egui::Context, acts: &mut Vec<Action>) {
        let bar = theme::p().bar;

        egui::TopBottomPanel::top("barra").frame(egui::Frame::new().fill(bar).inner_margin(egui::Margin::symmetric(10, 8))).show(ctx, |ui| {
            self.toolbar(ui, acts);
            ui.add_space(2.0);
            self.actions_bar(ui, acts);
        });

        egui::TopBottomPanel::top("pestanas").frame(egui::Frame::new().fill(bar).inner_margin(egui::Margin { left: 10, right: 10, top: 0, bottom: 4 })).show(ctx, |ui| {
            self.tab_bar(ui, acts);
        });

        egui::TopBottomPanel::bottom("estado").frame(egui::Frame::new().fill(bar).inner_margin(egui::Margin::symmetric(12, 5))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let n = self.visible.len();
                ui.label(if self.search_results.is_some() { trf!("{} resultado(s) de búsqueda", n) } else { trf!("{} elemento(s)", n) });
                if !self.selection.is_empty() {
                    let bytes: u64 = self.selected_entries().iter().map(|e| e.size).sum();
                    ui.separator();
                    ui.label(trf!("{} seleccionado(s) · {}", self.selection.len(), human_size(bytes)));
                }
                if let Some((p, cut)) = &self.clipboard {
                    ui.separator();
                    ui.label(RichText::new(trf!("{} en el portapapeles ({})", p.len(), if *cut { tr("cortar") } else { tr("copiar") })).color(theme::p().muted));
                }
                if let Some((m, err, t)) = &self.message {
                    if t.elapsed().as_secs() < 6 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let col = if *err { theme::p().danger } else { theme::p().ok };
                            ui.label(RichText::new(m).color(col));
                        });
                        ctx.request_repaint_after(std::time::Duration::from_secs(1));
                    }
                }
            });
        });

        egui::SidePanel::left("lugares")
            .resizable(true)
            .default_width(210.0)
            .width_range(150.0..=360.0)
            .frame(egui::Frame::new().fill(theme::p().side).inner_margin(egui::Margin::symmetric(8, 6)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui, acts));
            });

        if self.show_panel {
            egui::SidePanel::right("panel_derecho")
                .resizable(true)
                .default_width(280.0)
                .width_range(210.0..=480.0)
                .frame(egui::Frame::new().fill(theme::p().side2).inner_margin(10))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.panel_tab, 0, tr("🔧 Utilidades"));
                        ui.selectable_value(&mut self.panel_tab, 1, tr("ℹ Detalles"));
                    });
                    ui.separator();
                    if self.panel_tab == 0 {
                        self.utilities_panel(ui, acts);
                    } else {
                        self.details_panel(ui);
                    }
                });
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::p().base).inner_margin(egui::Margin::symmetric(6, 4))).show(ctx, |ui| {
            if let Some(err) = &self.load_error {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("⚠").size(40.0).color(Color32::from_rgb(200, 120, 30)));
                    ui.label(err);
                });
                return;
            }
            if self.visible.is_empty() {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("📂").size(48.0).color(theme::p().muted));
                    let msg = if !self.filter.is_empty() { tr("No hay nada que coincida con la búsqueda") } else { tr("Esta carpeta está vacía") };
                    ui.label(RichText::new(msg).color(theme::p().muted));
                });
                self.empty_space(ui, acts);
                return;
            }
            match self.view_mode {
                ViewMode::List => self.list_view(ui, acts),
                ViewMode::Icons => self.icons_view(ui, acts),
            }
        });

    }

    /// Opens a tool from the start screen.
    fn hub_open(&mut self, k: Kind) {
        // Every tool chooses its files itself («Examinar…»), so it opens straight away.
        self.tools.push(ToolWindow::new(self.next_tool, k, vec![], self.cwd.clone()));
        self.next_tool += 1;
    }

    /// The start screen: every utility, grouped, each with what it does.
    fn hub(&mut self, ctx: &egui::Context) {
        let mut open: Option<Kind> = None;
        egui::TopBottomPanel::top("hub_bar").frame(egui::Frame::new().fill(theme::p().bar).inner_margin(egui::Margin::symmetric(16, 10))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let logo = self.logo.get_or_insert_with(|| ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied([64, 64], &icon::rgba(64)), egui::TextureOptions::LINEAR)).clone();
                ui.image((logo.id(), egui::vec2(30.0, 30.0)));
                ui.label(RichText::new(APP_NAME).size(22.0).strong().color(theme::accent()));
                ui.label(RichText::new(tr("Utilidades para archivos y carpetas")).color(theme::p().sub));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button(tr("🎨 Apariencia"), |ui| appearance_menu(ui));
                    if ui.button(tr("🖱 Menú contextual del sistema…")).on_hover_text(tr("Añade las herramientas al clic derecho del explorador de archivos del sistema")).clicked() {
                        self.integration = Some(None);
                    }
                    if ui.button(tr("📂 Explorador de archivos")).on_hover_text(tr("Un explorador sencillo para elegir archivos y abrir las utilidades sobre ellos")).clicked() {
                        self.explorer = true;
                        self.reload();
                    }
                });
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::p().base).inner_margin(egui::Margin::symmetric(18, 12))).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let card_w = 300.0;
                let cols = ((ui.available_width() + 10.0) / (card_w + 10.0)).floor().max(1.0) as usize;
                let card_w = (ui.available_width() - 10.0 * (cols as f32 - 1.0)) / cols as f32;
                for (group, kinds) in tools::GROUPS {
                    ui.add_space(6.0);
                    ui.label(RichText::new(tr(group).to_uppercase()).small().strong().color(theme::p().sub));
                    ui.add_space(2.0);
                    for chunk in kinds.chunks(cols) {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            for k in chunk.iter().copied() {
                                let (rect, resp) = ui.allocate_exact_size(egui::vec2(card_w, 74.0), Sense::click());
                                let hovered = resp.hovered();
                                let p = ui.painter();
                                p.rect(rect, 10.0, if hovered { theme::accent().gamma_multiply(0.10) } else { theme::p().side }, Stroke::new(1.0_f32, if hovered { theme::accent().gamma_multiply(0.6) } else { theme::p().border }), egui::StrokeKind::Inside);
                                p.text(rect.left_top() + egui::vec2(16.0, 22.0), Align2::LEFT_CENTER, k.icon(), FontId::proportional(22.0), theme::accent());
                                let title = fitted(ui, k.label(), 15.0, theme::p().text, rect.width() - 64.0, 1);
                                ui.painter().galley(rect.left_top() + egui::vec2(50.0, 12.0), title, theme::p().text);
                                let desc = fitted(ui, k.description(), 12.0, theme::p().sub, rect.width() - 64.0, 2);
                                ui.painter().galley(rect.left_top() + egui::vec2(50.0, 34.0), desc, theme::p().sub);
                                if resp.on_hover_text(k.description()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    open = Some(k);
                                }
                            }
                        });
                        ui.add_space(8.0);
                    }
                }
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Consejo: con «Menú contextual del sistema» las abres con clic derecho sobre los archivos, desde el explorador de tu sistema.")).small().color(theme::p().muted));
            });
        });
        if let Some(k) = open {
            self.hub_open(k);
        }
    }

    /// The window when Ficherón was opened for one tool: that tool alone, and a button to the explorer.
    fn solo_update(&mut self, ctx: &egui::Context) {
        let mut events = vec![];
        let sel = self.selected_paths();
        let cwd = self.cwd.clone();
        let mut to_explorer = false;
        egui::TopBottomPanel::top("solo_bar").frame(egui::Frame::new().fill(theme::p().bar).inner_margin(egui::Margin::symmetric(12, 6))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(t) = self.tools.first() {
                    ui.label(RichText::new(format!("{}  {}", t.kind.icon(), t.kind.label())).strong().size(16.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr("📂 Abrir el explorador de Ficherón")).on_hover_text(self.cwd.to_string_lossy()).clicked() {
                        to_explorer = true;
                    }
                });
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::p().base).inner_margin(egui::Margin::symmetric(12, 8))).show(ctx, |ui| {
            if let Some(t) = self.tools.first_mut() {
                t.show_inline(ctx, ui, &mut events, &sel, &cwd);
            }
        });
        for ev in events {
            match ev {
                ToolEvent::Reload => self.reload(),
                ToolEvent::Clipboard(s) => ctx.copy_text(s),
                ToolEvent::Navigate(p) => {
                    to_explorer = true;
                    self.navigate(p);
                }
            }
        }
        if to_explorer {
            // The tool goes on as a window over the explorer.
            self.solo = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(APP_NAME.into()));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1240.0, 760.0)));
        }
    }

    /// The tools, grouped in columns, in a panel above everything else.
    fn tool_picker(&mut self, ctx: &egui::Context, acts: &mut Vec<Action>) {
        let Some(pos) = self.tool_picker else { return };
        let mut close = false;
        let has_sel = !self.selection.is_empty();
        let area = egui::Area::new(egui::Id::new("tool_picker")).order(egui::Order::Foreground).fixed_pos(pos).constrain(true).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                let what = if has_sel { trf!("Se aplican a {} elemento(s) seleccionado(s)", self.selection.len()) } else { trf!("Se aplican a la carpeta «{}»", self.cwd.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "/".into())) };
                ui.label(RichText::new(what).small().color(theme::p().muted));
                ui.separator();
                ui.horizontal_top(|ui| {
                    for (group, kinds) in tools::GROUPS {
                        ui.vertical(|ui| {
                            ui.set_min_width(190.0);
                            ui.label(RichText::new(tr(group)).small().strong().color(theme::p().sub));
                            for k in kinds.iter().copied() {
                                let enabled = has_sel || k.folder_ok();
                                if ui.add_enabled(enabled, egui::Button::new(format!("{}  {}", k.icon(), k.label())).frame(false)).on_hover_text(k.description()).on_disabled_hover_text(tr("Selecciona primero algún elemento")).clicked() {
                                    acts.push(Action::Tool(k));
                                    close = true;
                                }
                            }
                        });
                    }
                });
            });
        });
        let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().map(|p| !area.response.rect.contains(p)).unwrap_or(false));
        if close || outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.tool_picker = None;
        }
    }

    fn integration_window(&mut self, ctx: &egui::Context) {
        let Some(mut msg) = self.integration.take() else { return };
        let mut open = true;
        egui::Window::new(tr("🖱 Menú contextual del sistema")).open(&mut open).collapsible(false).resizable(false).default_width(520.0).show(ctx, |ui| {
            let system = if cfg!(windows) {
                tr("En Windows aparece un submenú «Ficherón» al hacer clic derecho en archivos, carpetas y el fondo de una carpeta, y una entrada por herramienta en «Enviar a» (la que recibe todos los archivos seleccionados a la vez).")
            } else if cfg!(target_os = "macos") {
                tr("En macOS aparece cada herramienta como Acción rápida: clic derecho en el Finder › Acciones rápidas (o Servicios). Puede que tengas que activarlas en Ajustes del Sistema › Teclado › Funciones rápidas.")
            } else {
                tr("En Linux se añade un submenú «Ficherón» en Nautilus y Caja (Scripts), Nemo y Dolphin. Puede hacer falta cerrar y abrir el explorador de archivos.")
            };
            ui.label(system);
            ui.add_space(4.0);
            ui.label(RichText::new(tr("Herramientas que se añaden:")).small().strong().color(theme::p().sub));
            ui.horizontal_wrapped(|ui| {
                for (_, k) in integration::TOOLS {
                    ui.label(RichText::new(format!("{} {}", k.icon(), k.label())).small());
                }
            });
            ui.label(RichText::new(tr("Solo para tu usuario; no hace falta ser administrador.")).small().color(theme::p().muted));
            ui.separator();
            let on = integration::installed();
            ui.horizontal(|ui| {
                let label = if on { tr("✔ Actualizar en el menú contextual") } else { tr("➕ Añadir al menú contextual") };
                if ui.add(egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(theme::accent())).clicked() {
                    msg = Some(match integration::install() {
                        Ok(_) => (tr("Listo: ya aparece en el clic derecho del explorador de archivos.").to_string(), false),
                        Err(e) => (trf!("No se pudo añadir: {}", e), true),
                    });
                }
                if ui.add_enabled(on, egui::Button::new(tr("Quitar del menú contextual"))).clicked() {
                    msg = Some(match integration::uninstall() {
                        Ok(_) => (tr("Quitado del menú contextual.").to_string(), false),
                        Err(e) => (trf!("No se pudo quitar: {}", e), true),
                    });
                }
            });
            if let Some((m, err)) = &msg {
                ui.label(RichText::new(m).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if open {
            self.integration = Some(msg);
        }
    }

    fn tab_bar(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let many = self.tabs.len() > 1;
            for (k, t) in self.tabs.iter().enumerate() {
                let active = k == self.active_tab;
                let cwd = if active { &self.cwd } else { &t.cwd };
                let mut name = folder_label(cwd);
                if name.chars().count() > 22 {
                    name = name.chars().take(21).collect::<String>() + "…";
                }
                let fill = if active { theme::p().base } else { theme::p().bar };
                let frame = egui::Frame::new().fill(fill).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 3)).stroke(Stroke::new(1.0_f32, if active { theme::accent().gamma_multiply(0.6) } else { theme::p().hover }));
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let col = if active { theme::accent() } else { theme::p().text };
                        let r = ui.add(egui::Label::new(RichText::new(format!("📁 {name}")).color(col)).sense(Sense::click())).on_hover_text(cwd.to_string_lossy().to_string());
                        if r.clicked() {
                            acts.push(Action::SwitchTab(k));
                        }
                        if r.middle_clicked() && many {
                            acts.push(Action::CloseTab(k));
                        }
                        r.context_menu(|ui| {
                            if ui.button(tr("Duplicar pestaña")).clicked() {
                                acts.push(Action::SwitchTab(k));
                                acts.push(Action::NewTab(Some(cwd.clone())));
                                ui.close_menu();
                            }
                            if many && ui.button(tr("Cerrar pestaña")).clicked() {
                                acts.push(Action::CloseTab(k));
                                ui.close_menu();
                            }
                        });
                        if many && ui.small_button("✖").on_hover_text(tr("Cerrar pestaña (Ctrl+W)")).clicked() {
                            acts.push(Action::CloseTab(k));
                        }
                    });
                });
            }
            if ui.small_button("+").on_hover_text(tr("Nueva pestaña (Ctrl+T)")).clicked() {
                acts.push(Action::NewTab(None));
            }
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        ui.add_space(6.0);
        section_title(ui, tr("LUGARES"));
        let places: Vec<(&str, &str, Option<PathBuf>)> = vec![
            ("🏠", tr("Carpeta personal"), dirs::home_dir()),
            ("🖥", tr("Escritorio"), dirs::desktop_dir()),
            ("📄", tr("Documentos"), dirs::document_dir()),
            ("⬇", tr("Descargas"), dirs::download_dir()),
            ("🖼", tr("Imágenes"), dirs::picture_dir()),
            ("🎵", tr("Música"), dirs::audio_dir()),
            ("🎞", tr("Vídeos"), dirs::video_dir()),
        ];
        for (icon, name, p) in places {
            let Some(p) = p else { continue };
            if !p.is_dir() {
                continue;
            }
            self.side_item(ui, icon, name, &p, acts, false);
        }
        ui.add_space(8.0);
        section_title(ui, tr("EQUIPO"));
        #[cfg(unix)]
        {
            self.side_item(ui, "🖴", tr("Sistema de archivos"), Path::new("/"), acts, false);
            for m in ["/media", "/mnt", "/run/media", tr("/Volumes")] {
                if let Ok(rd) = std::fs::read_dir(m) {
                    for d in rd.flatten().take(6) {
                        let path = d.path();
                        if path.is_dir() {
                            let name = d.file_name().to_string_lossy().to_string();
                            self.side_item(ui, "💽", &name, &path, acts, false);
                        }
                    }
                }
            }
        }
        #[cfg(windows)]
        for l in b'C'..=b'Z' {
            let p = PathBuf::from(format!("{}:\\", l as char));
            if p.exists() {
                self.side_item(ui, "🖴", &trf!("Disco ({}:)", l as char), &p, acts, false);
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            section_title(ui, tr("FAVORITOS"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("+").on_hover_text(tr("Añadir la carpeta actual a favoritos")).clicked() {
                    acts.push(Action::AddFavorite(self.cwd.clone()));
                }
            });
        });
        if self.favorites.is_empty() {
            ui.label(RichText::new(tr("Pulsa + para guardar la carpeta actual")).small().color(theme::p().muted));
        }
        for f in self.favorites.clone() {
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| f.to_string_lossy().to_string());
            self.side_item(ui, "★", &name, &f, acts, true);
        }

        let recent: Vec<PathBuf> = self.recent.iter().filter(|p| **p != self.cwd).take(8).cloned().collect();
        if !recent.is_empty() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                section_title(ui, tr("RECIENTES"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖").on_hover_text(tr("Vaciar la lista de recientes")).clicked() {
                        acts.push(Action::ClearRecent);
                    }
                });
            });
            for p in recent {
                self.side_item(ui, "🕘", &folder_label(&p), &p, acts, false);
            }
        }
    }

    fn side_item(&self, ui: &mut egui::Ui, icon: &str, name: &str, p: &Path, acts: &mut Vec<Action>, fav: bool) {
        let active = self.cwd == p;
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), Sense::click());
        let painter = ui.painter();
        if active {
            painter.rect_filled(rect, 6.0, theme::accent().gamma_multiply(0.18));
        } else if resp.hovered() {
            painter.rect_filled(rect, 6.0, theme::p().hover);
        }
        let col = if active { theme::accent() } else { theme::p().text };
        painter.text(rect.left_center() + egui::vec2(8.0, 0.0), Align2::LEFT_CENTER, icon, FontId::proportional(15.0), col);
        // Long names are cut with «…» instead of running out of the panel; the tooltip has the path.
        let g = fitted(ui, name, 14.0, col, rect.width() - 38.0, 1);
        ui.painter().galley(egui::pos2(rect.left() + 32.0, rect.center().y - g.size().y / 2.0), g, col);
        if resp.clicked() {
            acts.push(Action::Navigate(p.to_path_buf()));
        }
        let resp = resp.on_hover_text(p.to_string_lossy().to_string());
        if resp.middle_clicked() {
            acts.push(Action::NewTab(Some(p.to_path_buf())));
        }
        resp.context_menu(|ui| {
            if ui.button(tr("Abrir")).clicked() {
                acts.push(Action::Navigate(p.to_path_buf()));
                ui.close_menu();
            }
            if ui.button(tr("Abrir en pestaña nueva")).clicked() {
                acts.push(Action::NewTab(Some(p.to_path_buf())));
                ui.close_menu();
            }
            if fav {
                if ui.button(tr("Quitar de favoritos")).clicked() {
                    acts.push(Action::RemoveFavorite(p.to_path_buf()));
                    ui.close_menu();
                }
            } else if ui.button(tr("Añadir a favoritos")).clicked() {
                acts.push(Action::AddFavorite(p.to_path_buf()));
                ui.close_menu();
            }
        });
    }

    fn item_menu(&self, ui: &mut egui::Ui, e: &Entry, acts: &mut Vec<Action>) {
        if ui.button(if e.is_dir { tr("📂 Abrir") } else { tr("↗ Abrir con la aplicación predeterminada") }).clicked() {
            acts.push(Action::Open(e.clone()));
            ui.close_menu();
        }
        if ui.button(tr("👁 Vista rápida (Espacio)")).clicked() {
            acts.push(Action::QuickLook);
            ui.close_menu();
        }
        if e.is_dir && ui.button(tr("🗂 Abrir en pestaña nueva")).clicked() {
            acts.push(Action::NewTab(Some(e.path.clone())));
            ui.close_menu();
        }
        if e.is_dir && ui.button(tr("★ Añadir a favoritos")).clicked() {
            acts.push(Action::AddFavorite(e.path.clone()));
            ui.close_menu();
        }
        ui.separator();
        for (label, a) in [(tr("✂ Cortar"), Action::Cut), (tr("🗐 Copiar"), Action::Copy)] {
            if ui.button(label).clicked() {
                acts.push(a);
                ui.close_menu();
            }
        }
        if self.clipboard.is_some() && ui.button(tr("📋 Pegar aquí")).clicked() {
            acts.push(Action::Paste);
            ui.close_menu();
        }
        ui.separator();
        if self.selection.len() <= 1 && ui.button(tr("✏ Renombrar")).clicked() {
            acts.push(Action::Rename);
            ui.close_menu();
        }
        if ui.button(tr("🗑 Mover a la papelera")).clicked() {
            acts.push(Action::Trash);
            ui.close_menu();
        }
        if ui.button(RichText::new(tr("⚠ Eliminar definitivamente")).color(theme::p().danger)).clicked() {
            acts.push(Action::DeletePermanently);
            ui.close_menu();
        }
        ui.separator();
        // Not a submenu: the tool list is long, and a long submenu can open under the menu it
        // comes from. It opens as its own panel where the menu was.
        if ui.button(RichText::new(tr("🔧 Utilidades…")).color(theme::accent())).clicked() {
            acts.push(Action::ToolPicker(ui.ctx().pointer_latest_pos().unwrap_or(ui.min_rect().left_top())));
            ui.close_menu();
        }
        if ui.button(tr("ℹ Propiedades")).clicked() {
            acts.push(Action::Properties);
            ui.close_menu();
        }
    }

    fn background_menu(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        if ui.button(tr("📁 Nueva carpeta")).clicked() {
            acts.push(Action::NewFolder);
            ui.close_menu();
        }
        if ui.button(tr("🗋 Nuevo archivo")).clicked() {
            acts.push(Action::NewFile);
            ui.close_menu();
        }
        if ui.add_enabled(self.clipboard.is_some(), egui::Button::new(tr("📋 Pegar"))).clicked() {
            acts.push(Action::Paste);
            ui.close_menu();
        }
        ui.separator();
        if ui.button(tr("Seleccionar todo")).clicked() {
            acts.push(Action::SelectAll);
            ui.close_menu();
        }
        if ui.button(tr("⟳ Actualizar")).clicked() {
            acts.push(Action::Refresh);
            ui.close_menu();
        }
        // Not a submenu: the tool list is long, and a long submenu can open under the menu it
        // comes from. It opens as its own panel where the menu was.
        if ui.button(RichText::new(tr("🔧 Utilidades…")).color(theme::accent())).clicked() {
            acts.push(Action::ToolPicker(ui.ctx().pointer_latest_pos().unwrap_or(ui.min_rect().left_top())));
            ui.close_menu();
        }
        if ui.button(tr("ℹ Propiedades de la carpeta")).clicked() {
            acts.push(Action::Properties);
            ui.close_menu();
        }
    }

    fn handle_item(&self, resp: &egui::Response, i: usize, e: &Entry, acts: &mut Vec<Action>, ui: &egui::Ui) {
        if resp.double_clicked() {
            acts.push(Action::Open(e.clone()));
        } else if resp.clicked() {
            acts.push(Action::Click(i, ui.input(|inp| inp.modifiers)));
        }
        if resp.secondary_clicked() {
            acts.push(Action::SelectOnly(i));
        }
        if resp.middle_clicked() && e.is_dir {
            acts.push(Action::NewTab(Some(e.path.clone())));
        }
        resp.context_menu(|ui| self.item_menu(ui, e, acts));
    }

    fn list_view(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let total_w = ui.available_width();
        let size_w = 90.0;
        let kind_w = 150.0;
        let mod_w = 135.0;
        let name_w = (total_w - size_w - kind_w - mod_w - 24.0).max(150.0);
        let cols = [(name_w, tr("Nombre"), SortBy::Name), (size_w, tr("Tamaño"), SortBy::Size), (kind_w, tr("Tipo"), SortBy::Kind), (mod_w, tr("Modificado"), SortBy::Modified)];

        let (hrect, _) = ui.allocate_exact_size(egui::vec2(total_w, 26.0), Sense::hover());
        ui.painter().line_segment([hrect.left_bottom(), hrect.right_bottom()], Stroke::new(1.0_f32, theme::p().border));
        let mut x = hrect.left() + 8.0;
        for (w, label, s) in cols {
            let r = egui::Rect::from_min_size(egui::pos2(x, hrect.top()), egui::vec2(w, hrect.height()));
            let resp = ui.interact(r, ui.id().with(label), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, 4.0, theme::p().hover);
            }
            let arrow = if self.sort_by == s {
                if self.ascending {
                    " ⏶"
                } else {
                    " ⏷"
                }
            } else {
                ""
            };
            ui.painter().text(r.left_center() + egui::vec2(4.0, 0.0), Align2::LEFT_CENTER, format!("{label}{arrow}"), FontId::proportional(13.0), theme::p().sub);
            if resp.clicked() {
                acts.push(Action::SortBy(s));
            }
            x += w;
        }

        let row_h = 26.0;
        let n = self.visible.len();
        let in_search = self.search_results.is_some();
        // The spacing must be zero before the scroll area measures its rows: `show_rows` adds it
        // to the row height up front, and with a gap there the last rows were never drawn.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, n, |ui, range| {
            for i in range {
                let e = &self.visible[i];
                let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(total_w, row_h), Sense::click());
                let selected = self.selection.contains(&e.path);
                let painter = ui.painter_at(rect);
                if selected {
                    painter.rect_filled(rect.shrink2(egui::vec2(2.0, 1.0)), 5.0, theme::accent());
                } else if resp.hovered() {
                    painter.rect_filled(rect.shrink2(egui::vec2(2.0, 1.0)), 5.0, theme::p().hover);
                } else if i % 2 == 1 {
                    painter.rect_filled(rect, 0.0, theme::p().stripe);
                }
                let cut = self.clipboard.as_ref().map(|(p, c)| *c && p.contains(&e.path)).unwrap_or(false);
                let txt = if selected {
                    Color32::WHITE
                } else if e.hidden || cut {
                    theme::p().muted
                } else {
                    theme::p().text
                };
                let sub = if selected { Color32::from_gray(235) } else { theme::p().sub };
                let mut x = rect.left() + 12.0;
                let icon_col = if selected {
                    Color32::WHITE
                } else if e.is_dir {
                    Color32::from_rgb(230, 170, 40)
                } else {
                    theme::p().sub
                };
                painter.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, e.icon(), FontId::proportional(15.0), icon_col);
                let mut name = e.name.clone();
                if in_search {
                    if let Ok(rel) = e.path.parent().unwrap_or(&e.path).strip_prefix(&self.cwd) {
                        let r = rel.to_string_lossy();
                        if !r.is_empty() {
                            name = trf!("{}   — en {}", name, r);
                        }
                    }
                }
                if e.is_link {
                    name.push_str("  ⤴");
                }
                let name_rect = egui::Rect::from_min_max(egui::pos2(x + 24.0, rect.top()), egui::pos2(x + name_w - 12.0, rect.bottom()));
                let g = fitted(ui, &name, 14.0, txt, name_rect.width(), 1);
                let cut = g.rows.first().map(|r| r.ends_with_newline || g.rows.len() > 1).unwrap_or(false) || g.size().x >= name_rect.width() - 1.0;
                ui.painter_at(name_rect).galley(egui::pos2(name_rect.left(), name_rect.center().y - g.size().y / 2.0), g, txt);
                if cut {
                    resp = resp.on_hover_text(&name);
                }
                x += name_w - 4.0;
                let size = if e.is_dir { "—".to_string() } else { human_size(e.size) };
                painter.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, size, FontId::proportional(13.0), sub);
                x += size_w;
                painter.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, e.kind_label(), FontId::proportional(13.0), sub);
                x += kind_w;
                painter.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, fsops::format_time(e.modified), FontId::proportional(13.0), sub);
                self.handle_item(&resp, i, e, acts, ui);
            }
            self.empty_space(ui, acts);
        });
    }

    fn icons_view(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let tile = egui::vec2(108.0, 96.0);
        let total_w = ui.available_width();
        let per_row = ((total_w - 8.0) / tile.x).floor().max(1.0) as usize;
        let n = self.visible.len();
        let rows = n.div_ceil(per_row);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, tile.y, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    for c in 0..per_row {
                        let i = r * per_row + c;
                        if i >= n {
                            break;
                        }
                        let e = &self.visible[i];
                        let (rect, resp) = ui.allocate_exact_size(tile, Sense::click());
                        let selected = self.selection.contains(&e.path);
                        let painter = ui.painter_at(rect);
                        let inner = rect.shrink(3.0);
                        if selected {
                            painter.rect_filled(inner, 8.0, theme::accent().gamma_multiply(0.22));
                            painter.rect_stroke(inner, 8.0, Stroke::new(1.5_f32, theme::accent()), egui::StrokeKind::Inside);
                        } else if resp.hovered() {
                            painter.rect_filled(inner, 8.0, theme::p().hover);
                        }
                        let icon_col = if e.is_dir { Color32::from_rgb(230, 170, 40) } else { theme::p().sub };
                        painter.text(egui::pos2(rect.center().x, rect.top() + 32.0), Align2::CENTER_CENTER, e.icon(), FontId::proportional(36.0), icon_col);
                        let galley = ui.fonts(|f| {
                            let mut job = egui::text::LayoutJob::simple(e.name.clone(), FontId::proportional(12.5), theme::p().text, tile.x - 12.0);
                            job.wrap.max_rows = 2;
                            job.halign = egui::Align::Center;
                            f.layout_job(job)
                        });
                        painter.galley(egui::pos2(rect.center().x, rect.top() + 60.0), galley, theme::p().text);
                        let resp = resp.on_hover_text(format!("{}\n{}{}", e.name, e.kind_label(), if e.is_dir { String::new() } else { format!(" · {}", human_size(e.size)) }));
                        self.handle_item(&resp, i, e, acts, ui);
                    }
                });
            }
            self.empty_space(ui, acts);
        });
    }

    fn empty_space(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let h = ui.available_height().max(60.0);
        let (_, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::click());
        if resp.clicked() {
            acts.push(Action::ClearSelection);
        }
        resp.context_menu(|ui| self.background_menu(ui, acts));
    }

    fn utilities_panel(&self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let n = self.selection.len();
        let target = if n == 0 {
            trf!("Se aplican a la carpeta «{}»", self.cwd.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "/".into()))
        } else {
            trf!("Se aplican a {} elemento(s) seleccionado(s)", n)
        };
        ui.label(RichText::new(target).small().color(theme::p().muted));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (group, kinds) in tools::GROUPS {
                section_title(ui, &tr(group).to_uppercase());
                for k in kinds.iter().copied() {
                    let enabled = n > 0 || k.folder_ok();
                    let w = ui.available_width();
                    let col = if enabled { theme::p().text } else { theme::p().muted };
                    let icol = if enabled { theme::accent() } else { theme::p().muted };
                    // A long name goes on two lines (and is cut after that) instead of leaving the panel.
                    let g = fitted(ui, k.label(), 14.0, col, w - 40.0, 2);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, (g.size().y + 12.0).max(30.0)), if enabled { Sense::click() } else { Sense::hover() });
                    let p = ui.painter();
                    if enabled && resp.hovered() {
                        p.rect_filled(rect, 6.0, theme::accent().gamma_multiply(0.12));
                    }
                    p.text(rect.left_center() + egui::vec2(8.0, 0.0), Align2::LEFT_CENTER, k.icon(), FontId::proportional(16.0), icol);
                    p.galley(egui::pos2(rect.left() + 34.0, rect.center().y - g.size().y / 2.0), g, col);
                    let resp = if enabled { resp.on_hover_text(k.description()) } else { resp.on_hover_text(trf!("{}\n\nSelecciona primero algún elemento.", k.description())) };
                    if resp.clicked() {
                        acts.push(Action::Tool(k));
                    }
                }
                ui.add_space(6.0);
            }
            section_title(ui, tr("OTRAS"));
            if ui.button(tr("🖥  Abrir terminal aquí")).clicked() {
                acts.push(Action::Terminal);
            }
            if ui.button(tr("🔗  Copiar ruta")).clicked() {
                acts.push(Action::CopyPath);
            }
            if ui.button(tr("🌐  Copiar ruta de red (UNC)")).clicked() {
                acts.push(Action::CopyNetPath);
            }
        });
    }

    fn details_panel(&mut self, ui: &mut egui::Ui) {
        let sel: Vec<Entry> = self.selected_entries().into_iter().cloned().collect();
        ui.add_space(6.0);
        if sel.is_empty() {
            let name = self.cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "/".into());
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("📂").size(56.0).color(Color32::from_rgb(230, 170, 40)));
                ui.label(RichText::new(name).strong().size(16.0));
            });
            ui.add_space(8.0);
            let dirs = self.visible.iter().filter(|e| e.is_dir).count();
            let files = self.visible.len() - dirs;
            let bytes: u64 = self.visible.iter().map(|e| e.size).sum();
            egui::Grid::new("info_cwd").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new(tr("Carpetas")).color(theme::p().muted));
                ui.label(dirs.to_string());
                ui.end_row();
                ui.label(RichText::new(tr("Archivos")).color(theme::p().muted));
                ui.label(files.to_string());
                ui.end_row();
                ui.label(RichText::new(tr("Ocupan")).color(theme::p().muted));
                ui.label(human_size(bytes));
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.label(RichText::new(tr("Selecciona un archivo para ver sus detalles.")).small().color(theme::p().muted));
            return;
        }
        if sel.len() > 1 {
            let bytes: u64 = sel.iter().map(|e| e.size).sum();
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("🗐").size(56.0).color(theme::accent()));
                ui.label(RichText::new(trf!("{} elementos seleccionados", sel.len())).strong().size(16.0));
                ui.label(trf!("{} en archivos", human_size(bytes)));
            });
            return;
        }
        let e = sel[0].clone();
        ui.vertical_centered(|ui| {
            let col = if e.is_dir { Color32::from_rgb(230, 170, 40) } else { theme::p().sub };
            ui.label(RichText::new(e.icon()).size(56.0).color(col));
            ui.add(egui::Label::new(RichText::new(&e.name).strong().size(15.0)).wrap());
            ui.label(RichText::new(e.kind_label()).color(theme::p().muted));
        });
        ui.add_space(8.0);
        egui::Grid::new("info_sel").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            if !e.is_dir {
                ui.label(RichText::new(tr("Tamaño")).color(theme::p().muted));
                ui.label(trf!("{} ({} bytes)", human_size(e.size), e.size));
                ui.end_row();
            }
            ui.label(RichText::new(tr("Modificado")).color(theme::p().muted));
            ui.label(fsops::format_time(e.modified));
            ui.end_row();
            ui.label(RichText::new(tr("Permisos")).color(theme::p().muted));
            ui.label(if e.readonly { tr("Solo lectura") } else { tr("Lectura y escritura") });
            ui.end_row();
        });
        ui.add_space(8.0);
        if e.is_text() {
            if self.preview.as_ref().map(|(p, _)| p != &e.path).unwrap_or(true) {
                self.preview = Some((e.path.clone(), fsops::preview_text(&e.path)));
            }
            if let Some((_, Some(t))) = &self.preview {
                section_title(ui, tr("VISTA PREVIA"));
                egui::Frame::new().fill(theme::p().base).stroke(Stroke::new(1.0_f32, theme::p().border)).corner_radius(6.0).inner_margin(8).show(ui, |ui| {
                    egui::ScrollArea::both().max_height(ui.available_height() - 10.0).show(ui, |ui| {
                        ui.label(RichText::new(t).monospace().size(11.5));
                    });
                });
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.take() else { return };
        let mut close = false;
        let mut confirm = false;
        let modal = egui::Modal::new(egui::Id::new("dialogo")).show(ctx, |ui| {
            ui.set_width(380.0);
            match &mut d {
                Dialog::NewFolder(name) | Dialog::NewFile(name) | Dialog::Rename { name, .. } => {
                    ui.heading(tr("Nombre"));
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if !r.has_focus() && !r.lost_focus() {
                        r.request_focus();
                    }
                    let bad = name.trim().is_empty() || name.contains('/') || (cfg!(windows) && name.contains('\\'));
                    if bad && !name.is_empty() {
                        ui.colored_label(theme::p().danger, tr("El nombre no puede contener «/»"));
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !bad {
                        confirm = true;
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!bad, egui::Button::new(RichText::new(tr("Aceptar")).color(Color32::WHITE)).fill(theme::accent())).clicked() {
                            confirm = true;
                        }
                        if ui.button(tr("Cancelar")).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Delete { paths, permanent } => {
                    ui.heading(if *permanent { tr("¿Eliminar definitivamente?") } else { tr("¿Mover a la papelera?") });
                    ui.add_space(6.0);
                    let names: Vec<String> = paths.iter().take(6).map(|p| format!("• {}", p.file_name().unwrap_or_default().to_string_lossy())).collect();
                    ui.label(names.join("\n"));
                    if paths.len() > 6 {
                        ui.label(trf!("…y {} más", paths.len() - 6));
                    }
                    if *permanent {
                        ui.colored_label(theme::p().danger, tr("Esto no se puede deshacer."));
                    } else {
                        ui.label(RichText::new(tr("Podrás recuperarlos desde la papelera del sistema.")).color(theme::p().muted));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let col = if *permanent { Color32::from_rgb(200, 50, 50) } else { theme::accent() };
                        if ui.add(egui::Button::new(RichText::new(if *permanent { tr("Eliminar") } else { tr("Mover a la papelera") }).color(Color32::WHITE)).fill(col)).clicked() {
                            confirm = true;
                        }
                        if ui.button(tr("Cancelar")).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Properties { entry, size } => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(entry.icon()).size(36.0));
                        ui.vertical(|ui| {
                            ui.heading(&entry.name);
                            ui.label(RichText::new(entry.kind_label()).color(theme::p().muted));
                        });
                    });
                    ui.separator();
                    egui::Grid::new("props").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label(RichText::new(tr("Ubicación")).color(theme::p().muted));
                        ui.add(egui::Label::new(entry.path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()).wrap());
                        ui.end_row();
                        ui.label(RichText::new(tr("Tamaño")).color(theme::p().muted));
                        match size {
                            Some((b, c)) => ui.label(trf!("{} · {} elementos dentro", human_size(*b), c)),
                            None => ui.label(trf!("{} ({} bytes)", human_size(entry.size), entry.size)),
                        };
                        ui.end_row();
                        ui.label(RichText::new(tr("Modificado")).color(theme::p().muted));
                        ui.label(fsops::format_time(entry.modified));
                        ui.end_row();
                        ui.label(RichText::new(tr("Permisos")).color(theme::p().muted));
                        ui.label(if entry.readonly { tr("Solo lectura") } else { tr("Lectura y escritura") });
                        ui.end_row();
                        if entry.is_link {
                            ui.label(RichText::new(tr("Enlace a")).color(theme::p().muted));
                            ui.label(std::fs::read_link(&entry.path).map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    if ui.button(tr("Cerrar")).clicked() {
                        close = true;
                    }
                }
            }
        });
        if modal.should_close() {
            close = true;
        }
        if confirm {
            self.confirm_dialog(d);
        } else if !close {
            self.dialog = Some(d);
        }
    }

    fn confirm_dialog(&mut self, d: Dialog) {
        match d {
            Dialog::NewFolder(name) => {
                let p = self.cwd.join(name.trim());
                match std::fs::create_dir(&p) {
                    Ok(()) => {
                        self.reload();
                        self.selection = [p].into();
                        self.info(tr("Carpeta creada"));
                    }
                    Err(e) => self.error(trf!("No se pudo crear la carpeta: {}", e)),
                }
            }
            Dialog::NewFile(name) => {
                let p = self.cwd.join(name.trim());
                if p.exists() {
                    self.error(tr("Ya existe un elemento con ese nombre"));
                    return;
                }
                match std::fs::File::create(&p) {
                    Ok(_) => {
                        self.reload();
                        self.selection = [p].into();
                        self.info(tr("Archivo creado"));
                    }
                    Err(e) => self.error(trf!("No se pudo crear el archivo: {}", e)),
                }
            }
            Dialog::Rename { path, name } => {
                let Some(parent) = path.parent() else { return };
                let to = parent.join(name.trim());
                if to == path {
                    return;
                }
                if to.exists() {
                    self.error(tr("Ya existe un elemento con ese nombre"));
                    return;
                }
                match std::fs::rename(&path, &to) {
                    Ok(()) => {
                        self.reload();
                        self.selection = [to].into();
                        self.info(tr("Renombrado"));
                    }
                    Err(e) => self.error(trf!("No se pudo renombrar: {}", e)),
                }
            }
            Dialog::Delete { paths, permanent } => {
                let mut errs = vec![];
                for p in &paths {
                    let r = if permanent { fsops::remove_permanently(p).map_err(|e| e.to_string()) } else { trash::delete(p).map_err(|e| e.to_string()) };
                    if let Err(e) = r {
                        errs.push(format!("{}: {e}", p.file_name().unwrap_or_default().to_string_lossy()));
                    }
                }
                self.reload();
                if errs.is_empty() {
                    self.info(if permanent { trf!("{} elemento(s) eliminados", paths.len()) } else { trf!("{} elemento(s) movidos a la papelera", paths.len()) });
                } else {
                    self.error(trf!("Algunos elementos no se pudieron borrar: {}", errs.join("; ")));
                }
            }
            Dialog::Properties { .. } => {}
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, acts: &mut Vec<Action>) {
        if self.dialog.is_some() {
            return;
        }
        let typing = ctx.wants_keyboard_input();
        let quick_open = self.quick.is_some();
        ctx.input_mut(|i| {
            if !typing && i.consume_key(Modifiers::NONE, Key::Space) {
                acts.push(Action::QuickLook);
            }
            if quick_open && i.consume_key(Modifiers::NONE, Key::Escape) {
                acts.push(Action::CloseQuickLook);
            }
            if i.consume_key(Modifiers::NONE, Key::F5) {
                acts.push(Action::Refresh);
            }
            if i.consume_key(Modifiers::COMMAND, Key::T) {
                acts.push(Action::NewTab(None));
            }
            if i.consume_key(Modifiers::COMMAND, Key::W) && self.tabs.len() > 1 {
                acts.push(Action::CloseTab(self.active_tab));
            }
            if !self.tabs.is_empty() && i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Tab) {
                acts.push(Action::SwitchTab((self.active_tab + self.tabs.len() - 1) % self.tabs.len()));
            }
            if !self.tabs.is_empty() && i.consume_key(Modifiers::COMMAND, Key::Tab) {
                acts.push(Action::SwitchTab((self.active_tab + 1) % self.tabs.len()));
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowLeft) {
                acts.push(Action::Back);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowRight) {
                acts.push(Action::Forward);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowUp) {
                acts.push(Action::Up);
            }
            if i.consume_key(Modifiers::COMMAND, Key::L) {
                self.editing_path = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::H) {
                self.show_hidden = !self.show_hidden;
                acts.push(Action::Refresh);
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::N) {
                acts.push(Action::NewFolder);
            }
            if typing {
                return;
            }
            if i.consume_key(Modifiers::NONE, Key::Backspace) {
                acts.push(Action::Up);
            }
            if i.consume_key(Modifiers::NONE, Key::F2) {
                acts.push(Action::Rename);
            }
            if i.consume_key(Modifiers::SHIFT, Key::Delete) {
                acts.push(Action::DeletePermanently);
            }
            if i.consume_key(Modifiers::NONE, Key::Delete) {
                acts.push(Action::Trash);
            }
            if i.consume_key(Modifiers::COMMAND, Key::A) {
                acts.push(Action::SelectAll);
            }
            if i.consume_key(Modifiers::ALT, Key::Enter) {
                acts.push(Action::Properties);
            }
            for ev in &i.events {
                match ev {
                    egui::Event::Copy => acts.push(Action::Copy),
                    egui::Event::Cut => acts.push(Action::Cut),
                    egui::Event::Paste(_) => acts.push(Action::Paste),
                    _ => {}
                }
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                if let Some(e) = self.visible.iter().find(|e| self.selection.contains(&e.path)) {
                    acts.push(Action::Open(e.clone()));
                }
            }
            let mut mv: Option<isize> = None;
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                mv = Some(1);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                mv = Some(-1);
            }
            if self.view_mode == ViewMode::Icons {
                if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                    mv = Some(1);
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                    mv = Some(-1);
                }
            }
            if let Some(d) = mv {
                if !self.visible.is_empty() {
                    let cur = self.anchor.map(|a| a as isize).unwrap_or(-1);
                    let next = (cur + d).clamp(0, self.visible.len() as isize - 1) as usize;
                    acts.push(Action::Click(next, Modifiers::NONE));
                }
            }
        });
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(out) = self.screenshot.clone() else { return };
        self.frames += 1;
        ctx.request_repaint();
        if self.frames >= 120 && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| i.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
        if let Some(img) = shot {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(parent) = out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                Some(buf) => {
                    if let Err(e) = buf.save(&out) {
                        eprintln!("No se pudo guardar la captura: {e}");
                    }
                }
                None => eprintln!("Captura no válida"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if theme::follow_system(ctx) {
            theme::apply(ctx);
        }
        self.handle_screenshot(ctx);
        // Ctrl + / Ctrl − / Ctrl 0 change the size too: remember it for next time.
        settings::set_zoom(ctx.zoom_factor());
        if self.solo {
            self.solo_update(ctx);
            return;
        }
        let mut acts: Vec<Action> = vec![];
        if self.explorer {
            self.shortcuts(ctx, &mut acts);
            self.explorer_panels(ctx, &mut acts);
        } else {
            self.hub(ctx);
        }

        let mut events = vec![];
        let sel = self.selected_paths();
        for t in &mut self.tools {
            t.show(ctx, &mut events, &sel, &self.cwd);
        }
        self.tools.retain(|t| t.open);
        for ev in events {
            match ev {
                ToolEvent::Reload => self.reload(),
                ToolEvent::Clipboard(s) => {
                    ctx.copy_text(s);
                    self.info(tr("Copiado al portapapeles"));
                }
                ToolEvent::Navigate(p) => {
                    self.explorer = true;
                    acts.push(Action::Navigate(p));
                }
            }
        }

        // Quick look follows the selection: one selected item shows, anything else keeps what is shown.
        if self.quick.is_some() {
            if let [e] = self.selected_entries().as_slice() {
                if self.quick.as_ref().map(|q| q.path != e.path).unwrap_or(false) {
                    self.quick = Some(quicklook::QuickLook::open(ctx, e));
                }
            }
            if self.quick.as_ref().map(|q| !q.show(ctx)).unwrap_or(false) {
                self.quick = None;
            }
        }

        self.integration_window(ctx);
        self.tool_picker(ctx, &mut acts);
        self.dialogs(ctx);

        for a in acts {
            self.apply(a, ctx);
        }
    }
}
