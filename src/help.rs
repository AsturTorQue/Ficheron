//! The help: a «❓ Ayuda» menu in every top bar (and F1) that opens a window with a quick start,
//! what each utility does, how to write name patterns (wildcards and regular expressions),
//! the keyboard shortcuts and what the program is.

use crate::i18n::tr;
use crate::theme;
use crate::tools::GROUPS;
use eframe::egui::{self, RichText};

pub const START: u8 = 0;
pub const TOOLS: u8 = 1;
pub const PATTERNS: u8 = 2;
pub const KEYS: u8 = 3;
pub const ABOUT: u8 = 4;

fn tabs() -> [(u8, &'static str); 5] {
    [(START, tr("Primeros pasos")), (TOOLS, tr("Utilidades")), (PATTERNS, tr("Patrones y filtros")), (KEYS, tr("Atajos de teclado")), (ABOUT, tr("Acerca de"))]
}

/// The «❓ Ayuda» menu; picking an entry opens the help window on that page.
pub fn menu(ui: &mut egui::Ui, open: &mut Option<u8>) {
    ui.menu_button(tr("❓ Ayuda"), |ui| {
        for (k, label) in tabs() {
            if ui.button(label).clicked() {
                *open = Some(k);
                ui.close_menu();
            }
            if k == START || k == KEYS {
                ui.separator();
            }
        }
    })
    .response
    .on_hover_text(tr("F1"));
}

/// F1 opens the help (or closes it).
pub fn shortcut(ctx: &egui::Context, open: &mut Option<u8>) {
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1)) {
        *open = if open.is_some() { None } else { Some(START) };
    }
}

pub fn window(ctx: &egui::Context, open: &mut Option<u8>) {
    let Some(mut page) = *open else { return };
    let mut keep = true;
    egui::Window::new(tr("❓ Ayuda de Ficherón"))
        .id(egui::Id::new("ventana_ayuda"))
        .open(&mut keep)
        .collapsible(false)
        .default_size([720.0, 520.0])
        .min_width(480.0)
        .default_pos(ctx.screen_rect().center() - egui::vec2(360.0, 260.0))
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (k, label) in tabs() {
                    ui.selectable_value(&mut page, k, label);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("ayuda_texto").auto_shrink([false, false]).show(ui, |ui| match page {
                START => start(ui),
                TOOLS => tools(ui),
                PATTERNS => patterns(ui),
                KEYS => keys(ui),
                _ => about(ui),
            });
        });
    *open = if keep { Some(page) } else { None };
}

fn title(ui: &mut egui::Ui, t: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(t).strong().size(15.0).color(theme::accent()));
}

fn para(ui: &mut egui::Ui, t: &str) {
    ui.label(t);
    ui.add_space(2.0);
}

fn start(ui: &mut egui::Ui) {
    title(ui, tr("Qué es Ficherón"));
    para(ui, tr("Un conjunto de utilidades para trabajar con muchos archivos y carpetas a la vez: renombrar, buscar duplicados, comprimir, comparar, sincronizar, limpiar…"));
    title(ui, tr("Cómo se usa"));
    para(ui, tr("1. En la pantalla de inicio, pulsa la utilidad que necesites. Cada una se abre en su ventana."));
    para(ui, tr("2. Elige sobre qué trabaja: con «Examinar…», pegando una ruta o arrastrando archivos y carpetas a la ventana."));
    para(ui, tr("3. Ajusta las opciones. Casi todas las utilidades enseñan antes lo que van a hacer (una vista previa o una lista) y no tocan nada hasta que pulsas el botón final."));
    para(ui, tr("4. Lo que se borra va a la papelera salvo que marques lo contrario, y varias utilidades tienen «Deshacer»."));
    title(ui, tr("Otras formas de abrirlas"));
    para(ui, tr("«📂 Explorador de archivos» abre un explorador sencillo: selecciona archivos y abre las utilidades sobre ellos desde «🔧 Utilidades»."));
    para(ui, tr("«🖱 Menú contextual del sistema…» añade las utilidades al clic derecho del explorador de tu sistema (Windows, macOS o Linux), para abrirlas directamente sobre lo que elijas."));
    title(ui, tr("Apariencia"));
    para(ui, tr("En «🎨 Apariencia» eliges idioma, tema claro u oscuro, color y el tamaño de la interfaz (también con Ctrl + y Ctrl −)."));
}

fn tools(ui: &mut egui::Ui) {
    for (group, kinds) in GROUPS {
        title(ui, tr(group));
        egui::Grid::new(("ayuda_grupo", group)).num_columns(2).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
            for k in kinds.iter().copied() {
                ui.label(RichText::new(format!("{}  {}", k.icon(), k.label())).strong());
                ui.add(egui::Label::new(k.description()).wrap());
                ui.end_row();
            }
        });
    }
}

fn patterns(ui: &mut egui::Ui) {
    title(ui, tr("Comodines"));
    para(ui, tr("Los filtros de nombre usan comodines salvo que marques «Expresión regular». No distinguen mayúsculas."));
    egui::Grid::new("ayuda_comodines").num_columns(2).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
        for (p, m) in [
            ("*", tr("cualquier texto (también nada)")),
            ("?", tr("un carácter cualquiera")),
            ("*.jpg", tr("los que terminan en .jpg")),
            ("foto*", tr("los que empiezan por «foto»")),
            ("*2024*", tr("los que llevan «2024» en cualquier parte")),
            ("*.jpg, *.png", tr("varios patrones: separados por comas")),
        ] {
            ui.label(RichText::new(p).monospace().strong());
            ui.label(m);
            ui.end_row();
        }
    });
    title(ui, tr("Expresiones regulares"));
    para(ui, tr("Para casos más finos. Se buscan en el nombre (sin la ruta) y tampoco distinguen mayúsculas."));
    egui::Grid::new("ayuda_regex").num_columns(2).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
        for (p, m) in [
            ("\\.(jpe?g|png)$", tr("termina en .jpg, .jpeg o .png")),
            ("^IMG_\\d+", tr("empieza por IMG_ seguido de números")),
            ("\\d{4}-\\d{2}-\\d{2}", tr("lleva una fecha como 2024-05-31")),
            ("^(?!.*copia)", tr("no lleva la palabra «copia»")),
            ("(.+) - (\\d+)", tr("guarda partes para usarlas al renombrar: $1, $2…")),
        ] {
            ui.label(RichText::new(p).monospace().strong());
            ui.label(m);
            ui.end_row();
        }
    });
    title(ui, tr("Marcadores al renombrar"));
    para(ui, tr("En «Renombrado avanzado», el apartado «Marcadores disponibles» de cada regla lista lo que puedes insertar: {n} (contador), {fecha}, {carpeta}, {captura} (fecha de la foto)…"));
}

fn keys(ui: &mut egui::Ui) {
    title(ui, tr("En toda la aplicación"));
    shortcuts(ui, "ayuda_atajos_app", &[("F1", tr("Abrir o cerrar esta ayuda")), ("Ctrl +  /  Ctrl −", tr("Agrandar o achicar la interfaz")), ("Ctrl 0", tr("Tamaño normal"))]);
    title(ui, tr("En el explorador de archivos"));
    shortcuts(
        ui,
        "ayuda_atajos_explorador",
        &[
            ("Intro", tr("Abrir")),
            ("Espacio", tr("Vista rápida")),
            ("F2", tr("Renombrar")),
            ("Supr", tr("Mover a la papelera")),
            ("Mayús + Supr", tr("Eliminar definitivamente")),
            ("Ctrl A", tr("Seleccionar todo")),
            ("Ctrl C  /  Ctrl X  /  Ctrl V", tr("Copiar, cortar y pegar")),
            ("Ctrl Mayús N", tr("Carpeta nueva")),
            ("Alt Intro", tr("Propiedades")),
            ("F5", tr("Actualizar")),
            ("Ctrl H", tr("Mostrar u ocultar los archivos ocultos")),
            ("Ctrl L", tr("Escribir una ruta")),
            ("Alt ←  /  Alt →", tr("Atrás y adelante")),
            ("Alt ↑  /  Retroceso", tr("Subir a la carpeta de arriba")),
            ("Ctrl T  /  Ctrl W", tr("Pestaña nueva y cerrar pestaña")),
            ("Ctrl Tab", tr("Pasar a la siguiente pestaña")),
        ],
    );
}

fn shortcuts(ui: &mut egui::Ui, id: &str, rows: &[(&str, &str)]) {
    egui::Grid::new(id).num_columns(2).spacing([20.0, 4.0]).striped(true).show(ui, |ui| {
        for (k, m) in rows {
            ui.label(RichText::new(*k).monospace().strong());
            ui.label(*m);
            ui.end_row();
        }
    });
}

fn about(ui: &mut egui::Ui) {
    title(ui, crate::APP_NAME);
    para(ui, &trf!("Versión {}", env!("CARGO_PKG_VERSION")));
    para(ui, tr("Utilidades para archivos y carpetas, para Windows, macOS y Linux, en español, inglés, portugués y francés."));
    para(ui, tr("Funciona sin conexión: no envía tus archivos ni ningún dato a ninguna parte."));
    if let Some(f) = crate::settings::file() {
        para(ui, &trf!("Los ajustes se guardan en {}", f.display()));
    }
}
