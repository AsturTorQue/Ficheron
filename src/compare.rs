//! Compare two files: text side by side with line numbers, the changed words highlighted,
//! equal stretches folded, ignore whitespace or case, and next / previous difference buttons.
//! Binary files get their sizes, hashes and the first byte that differs.

use crate::fsops::human_size;
use crate::grep::highlighted;
use crate::i18n::tr;
use crate::roots::{picker_window, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, FontId, RichText};
use similar::{capture_diff_slices, Algorithm, DiffOp};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

const MAX_TEXT: u64 = 20 << 20;
const CONTEXT: usize = 3;
const MAX_INLINE: usize = 1500;

#[derive(Clone, Copy, PartialEq)]
struct Options {
    ignore_ws: bool,
    ignore_case: bool,
    show_all: bool,
}

#[derive(Clone)]
enum Row {
    Equal(usize, usize, String),
    Del(usize, String, Vec<(usize, usize)>),
    Ins(usize, String, Vec<(usize, usize)>),
    /// Left and right of a changed line pair.
    Change(usize, String, Vec<(usize, usize)>, usize, String, Vec<(usize, usize)>),
    Folded(usize),
}

impl Row {
    fn is_change(&self) -> bool {
        matches!(self, Row::Del(..) | Row::Ins(..) | Row::Change(..))
    }
}

/// One line of the single-column view.
#[derive(Clone)]
enum URow {
    Equal(usize, String),
    Del(usize, String, Vec<(usize, usize)>),
    Ins(usize, String, Vec<(usize, usize)>),
    Folded(usize),
}

struct TextResult {
    rows: Vec<Row>,
    urows: Vec<URow>,
    ublocks: Vec<usize>,
    names: (String, String),
    /// Characters in the longest line: sets how wide each side must be to show it whole.
    max_chars: usize,
    /// Row index where each block of changes starts.
    blocks: Vec<usize>,
    added: usize,
    removed: usize,
    same: bool,
}

struct BinaryResult {
    sizes: (u64, u64),
    hashes: (String, String),
    first_diff: Option<u64>,
    differing: u64,
}

enum Outcome {
    Text(TextResult),
    Binary(BinaryResult),
    Failed(String),
}

fn normalise(s: &str, o: Options) -> String {
    let mut t = if o.ignore_ws { s.split_whitespace().collect::<Vec<_>>().join(" ") } else { s.to_string() };
    if o.ignore_case {
        t = t.to_lowercase();
    }
    t
}

/// Byte ranges of `a` and `b` that differ, by characters.
fn inline_ranges(a: &str, b: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    if a.len() > MAX_INLINE || b.len() > MAX_INLINE {
        return (vec![], vec![]);
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (ia, ib): (Vec<usize>, Vec<usize>) = (a.char_indices().map(|x| x.0).chain([a.len()]).collect(), b.char_indices().map(|x| x.0).chain([b.len()]).collect());
    let (mut ra, mut rb) = (vec![], vec![]);
    for op in capture_diff_slices(Algorithm::Myers, &ca, &cb) {
        match op {
            DiffOp::Delete { old_index, old_len, .. } => ra.push((ia[old_index], ia[old_index + old_len])),
            DiffOp::Insert { new_index, new_len, .. } => rb.push((ib[new_index], ib[new_index + new_len])),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                ra.push((ia[old_index], ia[old_index + old_len]));
                rb.push((ib[new_index], ib[new_index + new_len]));
            }
            DiffOp::Equal { .. } => {}
        }
    }
    (ra, rb)
}

fn diff_text(a: &str, b: &str, o: Options) -> TextResult {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let (na, nb): (Vec<String>, Vec<String>) = (la.iter().map(|l| normalise(l, o)).collect(), lb.iter().map(|l| normalise(l, o)).collect());
    let ops = capture_diff_slices(Algorithm::Myers, &na, &nb);
    let mut full: Vec<Row> = Vec::with_capacity(la.len().max(lb.len()));
    let (mut added, mut removed) = (0, 0);
    for op in ops {
        match op {
            DiffOp::Equal { old_index, new_index, len } => {
                for k in 0..len {
                    full.push(Row::Equal(old_index + k + 1, new_index + k + 1, la[old_index + k].to_string()));
                }
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                removed += old_len;
                for k in 0..old_len {
                    full.push(Row::Del(old_index + k + 1, la[old_index + k].to_string(), vec![]));
                }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                added += new_len;
                for k in 0..new_len {
                    full.push(Row::Ins(new_index + k + 1, lb[new_index + k].to_string(), vec![]));
                }
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                let pairs = old_len.min(new_len);
                for k in 0..pairs {
                    let (x, y) = (la[old_index + k], lb[new_index + k]);
                    let (rx, ry) = inline_ranges(x, y);
                    full.push(Row::Change(old_index + k + 1, x.to_string(), rx, new_index + k + 1, y.to_string(), ry));
                }
                for k in pairs..old_len {
                    full.push(Row::Del(old_index + k + 1, la[old_index + k].to_string(), vec![]));
                }
                for k in pairs..new_len {
                    full.push(Row::Ins(new_index + k + 1, lb[new_index + k].to_string(), vec![]));
                }
                added += new_len;
                removed += old_len;
            }
        }
    }
    let same = !full.iter().any(Row::is_change);
    // Equal stretches keep CONTEXT lines around changes and fold the rest.
    let rows = if o.show_all { full } else { fold(full) };
    let mut blocks = vec![];
    let mut in_block = false;
    for (i, r) in rows.iter().enumerate() {
        if r.is_change() {
            if !in_block {
                blocks.push(i);
            }
            in_block = true;
        } else {
            in_block = false;
        }
    }
    let mut urows = Vec::with_capacity(rows.len() + 8);
    for r in &rows {
        match r {
            Row::Equal(a, _, s) => urows.push(URow::Equal(*a, s.clone())),
            Row::Del(a, s, rg) => urows.push(URow::Del(*a, s.clone(), rg.clone())),
            Row::Ins(b, s, rg) => urows.push(URow::Ins(*b, s.clone(), rg.clone())),
            Row::Change(a, sa, ra, b, sb, rb) => {
                urows.push(URow::Del(*a, sa.clone(), ra.clone()));
                urows.push(URow::Ins(*b, sb.clone(), rb.clone()));
            }
            Row::Folded(n) => urows.push(URow::Folded(*n)),
        }
    }
    let mut ublocks = vec![];
    let mut in_block = false;
    for (i, r) in urows.iter().enumerate() {
        let change = matches!(r, URow::Del(..) | URow::Ins(..));
        if change && !in_block {
            ublocks.push(i);
        }
        in_block = change;
    }
    let max_chars = la.iter().chain(lb.iter()).map(|l| l.chars().count()).max().unwrap_or(0).min(600);
    TextResult { rows, urows, ublocks, names: (String::new(), String::new()), max_chars, blocks, added, removed, same }
}

fn fold(full: Vec<Row>) -> Vec<Row> {
    let n = full.len();
    let mut keep = vec![false; n];
    for (i, r) in full.iter().enumerate() {
        if r.is_change() {
            for k in i.saturating_sub(CONTEXT)..=(i + CONTEXT).min(n.saturating_sub(1)) {
                keep[k] = true;
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut hidden = 0;
    for (r, k) in full.into_iter().zip(keep) {
        if k {
            if hidden > 0 {
                out.push(Row::Folded(hidden));
                hidden = 0;
            }
            out.push(r);
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        out.push(Row::Folded(hidden));
    }
    out
}

fn is_binary(p: &Path) -> bool {
    let mut buf = [0u8; 8000];
    fs::File::open(p).and_then(|mut f| f.read(&mut buf)).map(|n| buf[..n].contains(&0)).unwrap_or(false)
}

fn hash_hex(p: &Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    if let Ok(mut f) = fs::File::open(p) {
        let mut buf = vec![0u8; 1 << 20];
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn diff_binary(a: &Path, b: &Path) -> BinaryResult {
    let sizes = (fs::metadata(a).map(|m| m.len()).unwrap_or(0), fs::metadata(b).map(|m| m.len()).unwrap_or(0));
    let (mut first, mut differing) = (None, 0u64);
    if let (Ok(fa), Ok(fb)) = (fs::File::open(a), fs::File::open(b)) {
        let (mut ra, mut rb) = (std::io::BufReader::new(fa), std::io::BufReader::new(fb));
        let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
        let mut offset = 0u64;
        loop {
            let (na, nb) = (read_full(&mut ra, &mut ba), read_full(&mut rb, &mut bb));
            let common = na.min(nb);
            for i in 0..common {
                if ba[i] != bb[i] {
                    first.get_or_insert(offset + i as u64);
                    differing += 1;
                }
            }
            if na != nb {
                first.get_or_insert(offset + common as u64);
            }
            if na == 0 || nb == 0 {
                break;
            }
            offset += common as u64;
        }
    }
    differing += sizes.0.abs_diff(sizes.1);
    BinaryResult { sizes, hashes: (hash_hex(a), hash_hex(b)), first_diff: first, differing }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

fn compare(a: &Path, b: &Path, o: Options) -> Outcome {
    for p in [a, b] {
        if !p.is_file() {
            return Outcome::Failed(trf!("«{}» no es un archivo", p.display()));
        }
    }
    if is_binary(a) || is_binary(b) {
        return Outcome::Binary(diff_binary(a, b));
    }
    for p in [a, b] {
        if fs::metadata(p).map(|m| m.len()).unwrap_or(0) > MAX_TEXT {
            return Outcome::Binary(diff_binary(a, b));
        }
    }
    let read = |p: &Path| fs::read(p).map(|b| String::from_utf8_lossy(&b).to_string()).map_err(|e| e.to_string());
    match (read(a), read(b)) {
        (Ok(x), Ok(y)) => {
            let mut t = diff_text(&x, &y, o);
            let name = |p: &Path| format!("{} · {}", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), human_size(fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
            t.names = (name(a), name(b));
            Outcome::Text(t)
        }
        (Err(e), _) | (_, Err(e)) => Outcome::Failed(e),
    }
}

pub struct Comparer {
    a: String,
    b: String,
    pick: Option<(bool, Picker)>,
    opts: Options,
    unified: bool,
    outcome: Option<Outcome>,
    rx: Option<Receiver<Outcome>>,
    scroll_to: Option<f32>,
    /// Where the list was scrolled to last frame, in pixels.
    offset: f32,
    offset_x: f32,
    list_h: f32,
    viewport_h: f32,
    base: PathBuf,
}

const ROW_H: f32 = 22.0;

impl Comparer {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Comparer {
        let files: Vec<&PathBuf> = targets.iter().filter(|p| p.is_file()).collect();
        let mut c = Comparer {
            a: files.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            b: files.get(1).map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            pick: None,
            opts: Options { ignore_ws: false, ignore_case: false, show_all: false },
            unified: false,
            outcome: None,
            rx: None,
            scroll_to: None,
            offset: 0.0,
            offset_x: 0.0,
            list_h: 400.0,
            viewport_h: 400.0,
            base,
        };
        // Two files selected in the main window: compare them right away.
        if files.len() >= 2 {
            c.start();
        }
        c
    }

    /// Compares two small files at once, for trying the tool (`--compare`).
    pub fn demo(&mut self, dir: &Path) {
        let (a, b) = (dir.join("comparar_a.txt"), dir.join("comparar_b.txt"));
        // Line 20 is very long, to try the sideways scroll.
        let sample: String = (1..=60).map(|i| if i == 20 { format!("línea 20, muy larga: {}\n", "texto de relleno ".repeat(14)) } else { format!("línea {i} del documento\n") }).collect();
        let _ = fs::write(&a, &sample);
        let _ = fs::write(&b, sample.replace("línea 5 del", "línea 5 de este").replace("línea 31 del documento\n", "").replace("línea 48 del documento\n", "línea 48 del documento\nuna línea nueva\n"));
        self.a = a.to_string_lossy().to_string();
        self.b = b.to_string_lossy().to_string();
        self.start();
    }

    fn start(&mut self) {
        if self.a.trim().is_empty() || self.b.trim().is_empty() {
            return;
        }
        let (a, b, o) = (PathBuf::from(self.a.trim()), PathBuf::from(self.b.trim()), self.opts);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(compare(&a, &b, o));
        });
        self.scroll_to = Some(0.0);
        self.rx = Some(rx);
    }

    fn path_row(&mut self, ui: &mut egui::Ui, left: bool, enter: &mut bool) {
        ui.horizontal(|ui| {
            ui.add_sized([22.0, 22.0], egui::Label::new(RichText::new(if left { "A" } else { "B" }).strong().color(if left { Color32::from_rgb(210, 70, 60) } else { Color32::from_rgb(50, 160, 90) })));
            let w = (ui.available_width() - 190.0).max(120.0);
            let text = if left { &mut self.a } else { &mut self.b };
            let r = ui.add(egui::TextEdit::singleline(text).hint_text(tr("ruta del archivo")).desired_width(w));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                *enter = true;
            }
            if ui.button(tr("📂 Examinar…")).clicked() {
                let cur = if left { &self.a } else { &self.b };
                let start = Path::new(cur.trim()).parent().filter(|p| p.is_dir()).map(Path::to_path_buf).unwrap_or_else(|| self.base.clone());
                self.pick = Some((left, Picker::new(start)));
            }
        });
    }

    /// The whole tool scrolls when the window is too short for the header plus a useful list;
    /// the difference list inside it scrolls on its own.
    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        let total_h = ui.available_height();
        self.list_h = (total_h - 215.0).max(320.0);
        egui::ScrollArea::vertical().id_salt("compare_outer").max_height(total_h).auto_shrink([false, false]).show(ui, |ui| self.body(ui, events, selection, cwd));
    }

    fn body(&mut self, ui: &mut egui::Ui, _events: &mut Vec<ToolEvent>, selection: &[PathBuf], _cwd: &Path) {
        if let Some(rx) = &self.rx {
            if let Ok(o) = rx.try_recv() {
                self.outcome = Some(o);
                self.rx = None;
            }
        }
        let busy = self.rx.is_some();
        let ctx = ui.ctx().clone();
        // Files dropped on the window go to A, then B.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).filter(|p| p.is_file()).collect());
        let mut go = false;
        for p in dropped {
            if self.a.trim().is_empty() || !self.b.trim().is_empty() && self.a.trim() != p.to_string_lossy() && self.b.trim().is_empty() {
                self.a = p.to_string_lossy().to_string();
            } else {
                self.b = p.to_string_lossy().to_string();
                go = true;
            }
        }
        // Compact header: both files, then the options and the buttons on one row.
        self.path_row(ui, true, &mut go);
        self.path_row(ui, false, &mut go);
        let files: Vec<&PathBuf> = selection.iter().filter(|p| p.is_file()).collect();
        ui.horizontal_wrapped(|ui| {
            let ready = !busy && !self.a.trim().is_empty() && !self.b.trim().is_empty();
            let btn = egui::Button::new(RichText::new(tr("⚖ Comparar")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(ready, btn).clicked() {
                go = true;
            }
            if ui.button("A ↔ B").on_hover_text(tr("Intercambiar A y B")).clicked() {
                std::mem::swap(&mut self.a, &mut self.b);
                go = true;
            }
            if ui.add_enabled(files.len() >= 2, egui::Button::new(tr("Usar los seleccionados"))).on_hover_text(tr("Los dos archivos seleccionados en la ventana principal")).clicked() {
                self.a = files[0].to_string_lossy().to_string();
                self.b = files[1].to_string_lossy().to_string();
                go = true;
            }
            ui.separator();
            let before = self.opts;
            ui.checkbox(&mut self.opts.ignore_ws, tr("Ignorar espacios"));
            ui.checkbox(&mut self.opts.ignore_case, tr("Ignorar mayúsculas"));
            ui.checkbox(&mut self.opts.show_all, tr("Mostrar todo"));
            if before != self.opts && self.outcome.is_some() {
                go = true;
            }
            if busy {
                ui.spinner();
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        if let Some((left, pk)) = self.pick.take() {
            let mut slot = Some(pk);
            let picked = picker_window(&ctx, "compare_pick", &mut slot);
            if let Some(f) = picked.iter().find(|p| p.is_file()) {
                *(if left { &mut self.a } else { &mut self.b }) = f.to_string_lossy().to_string();
                slot = None;
                // As soon as both files are chosen, compare.
                go = !self.a.trim().is_empty() && !self.b.trim().is_empty();
            }
            self.pick = slot.map(|pk| (left, pk));
        }
        if go && !busy {
            self.start();
        }
        ui.separator();
        match self.outcome.take() {
            None => {
                if !busy {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("⚖").size(40.0).color(theme::p().muted));
                        ui.label(RichText::new(tr("Elige los dos archivos (o arrástralos aquí) y se comparan al momento.")).color(theme::p().muted));
                    });
                }
            }
            Some(Outcome::Failed(e)) => {
                ui.label(RichText::new(e.clone()).color(theme::p().danger));
                self.outcome = Some(Outcome::Failed(e));
            }
            Some(Outcome::Binary(b)) => {
                self.binary_ui(ui, &b);
                self.outcome = Some(Outcome::Binary(b));
            }
            Some(Outcome::Text(t)) => {
                self.text_ui(ui, &t);
                self.outcome = Some(Outcome::Text(t));
            }
        }
    }

    fn binary_ui(&self, ui: &mut egui::Ui, b: &BinaryResult) {
        let same = b.hashes.0 == b.hashes.1;
        ui.label(RichText::new(if same { tr("✔ Los dos archivos son idénticos") } else { tr("✘ Los archivos son distintos") }).strong().size(16.0).color(if same { theme::p().ok } else { theme::p().danger }));
        ui.label(RichText::new(tr("Archivos binarios: se comparan byte a byte.")).small().color(theme::p().muted));
        egui::Grid::new("compare_bin").num_columns(3).spacing([14.0, 4.0]).show(ui, |ui| {
            ui.label("");
            ui.label(RichText::new("A").strong());
            ui.label(RichText::new("B").strong());
            ui.end_row();
            ui.label(tr("Tamaño"));
            ui.label(human_size(b.sizes.0));
            ui.label(human_size(b.sizes.1));
            ui.end_row();
            ui.label("SHA-256");
            ui.label(RichText::new(&b.hashes.0).monospace().small());
            ui.label(RichText::new(&b.hashes.1).monospace().small());
            ui.end_row();
        });
        if let Some(off) = b.first_diff {
            ui.label(trf!("Primera diferencia en el byte {} (0x{:X}) · {} byte(s) distintos", off, off, b.differing));
        }
    }

    fn text_ui(&mut self, ui: &mut egui::Ui, t: &TextResult) {
        let ctx = ui.ctx().clone();
        let blocks: &[usize] = if self.unified { &t.ublocks } else { &t.blocks };
        let total_rows = if self.unified { t.urows.len() } else { t.rows.len() };
        let top_row = (self.offset / ROW_H).round() as usize;
        // The block being looked at: the last one that starts at or above the top of the list (+ a margin).
        let current = blocks.iter().rposition(|&b| b <= top_row + 3).unwrap_or(0);
        let mut jump: Option<usize> = None;
        // F3 / Shift+F3 and Alt+Down / Alt+Up walk through the differences.
        let (next_key, prev_key) = ctx.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, egui::Key::F3) || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowDown), i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3) || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowUp)));
        ui.horizontal_wrapped(|ui| {
            if t.same {
                ui.label(RichText::new(tr("✔ No hay diferencias")).strong().size(16.0).color(theme::p().ok));
            } else {
                let n = blocks.len();
                if ui.button(tr("⬆ Anterior")).on_hover_text("Shift+F3").clicked() || prev_key {
                    jump = Some(if n > 0 { (current + n - 1) % n } else { 0 });
                }
                if ui.button(tr("⬇ Siguiente")).on_hover_text("F3").clicked() || next_key {
                    // From the top of the list the first difference is the next one.
                    let nxt = blocks.iter().position(|&b| b > top_row + 3).unwrap_or(0);
                    jump = Some(nxt);
                }
                ui.label(RichText::new(format!("{}/{}", current + 1, n)).strong());
                ui.label(RichText::new(trf!("+{} líneas · −{} líneas", t.added, t.removed)).color(theme::p().sub));
                ui.separator();
                ui.selectable_value(&mut self.unified, false, tr("Lado a lado"));
                ui.selectable_value(&mut self.unified, true, tr("Unificado"));
            }
        });
        if t.same {
            return;
        }
        if let Some(k) = jump {
            if let Some(&row) = blocks.get(k) {
                self.scroll_to = Some((row.saturating_sub(3)) as f32 * ROW_H);
            }
        }
        // Names above each side.
        let area_h = self.list_h + 22.0;
        let font = FontId::monospace(13.0);
        let (red, green) = (Color32::from_rgba_unmultiplied(220, 70, 60, 40), Color32::from_rgba_unmultiplied(60, 170, 90, 40));
        let (red_s, green_s) = (Color32::from_rgba_unmultiplied(220, 70, 60, 120), Color32::from_rgba_unmultiplied(60, 170, 90, 120));
        let mini_w = 18.0;
        // A window inside the main one grows with its content, so give it a ceiling there.
        let full_w = if crate::tools::embedded(&ctx) { ui.available_width().min(1040.0) } else { ui.available_width() };
        let list_w = full_w - mini_w - 6.0;
        // Both sides stay on screen: a line longer than a side is shown by scrolling the text of
        // both sides together (slider under the list, or Shift + mouse wheel).
        let need = 70.0 + t.max_chars as f32 * 7.9;
        let content_w = list_w - 14.0;
        let half = content_w / 2.0;
        let pane_w = if self.unified { content_w } else { half };
        let max_hx = (need - pane_w).max(0.0);
        self.offset_x = self.offset_x.clamp(0.0, max_hx);
        let hx = self.offset_x;
        let slider_h = if max_hx > 0.0 { 22.0 } else { 0.0 };
        if !self.unified {
            let (nrect, _) = ui.allocate_exact_size(egui::vec2(list_w, 20.0), egui::Sense::hover());
            let painter = ui.painter_at(nrect);
            painter.rect_filled(nrect, 0.0, theme::p().hover);
            for (k, (name, tint)) in [(&t.names.0, Color32::from_rgb(210, 70, 60)), (&t.names.1, Color32::from_rgb(50, 160, 90))].into_iter().enumerate() {
                painter.text(egui::pos2(nrect.left() + k as f32 * half + 8.0, nrect.center().y), egui::Align2::LEFT_CENTER, format!("{}  {}", if k == 0 { "A" } else { "B" }, name), FontId::proportional(12.5), tint);
            }
        }
        let mut area = egui::ScrollArea::vertical().id_salt("compare_rows").max_height(area_h - 22.0 - slider_h).auto_shrink([false, false]);
        if let Some(off) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let mut out = None;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let list = ui.allocate_ui_with_layout(egui::vec2(list_w, area_h - 22.0), egui::Layout::top_down(egui::Align::LEFT), |ui| {
                out = Some(area.show_rows(ui, ROW_H, total_rows, |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(content_w, ROW_H), egui::Sense::hover());
                        let line = |ui: &mut egui::Ui, area: egui::Rect, no: Option<usize>, text: &str, ranges: &[(usize, usize)], bg: Option<Color32>, mark: Color32, sign: &str| {
                            if let Some(bg) = bg {
                                ui.painter().rect_filled(area, 0.0, bg);
                            }
                            if let Some(n) = no {
                                ui.painter().text(egui::pos2(area.left() + 42.0, area.center().y), egui::Align2::RIGHT_CENTER, n.to_string(), FontId::monospace(11.0), theme::p().muted);
                            }
                            if !sign.is_empty() {
                                ui.painter().text(egui::pos2(area.left() + 54.0, area.center().y), egui::Align2::CENTER_CENTER, sign, FontId::monospace(13.0), theme::p().text);
                            }
                            // The text slides with the sideways scroll under a fixed gutter.
                            let clip = egui::Rect::from_min_max(egui::pos2(area.left() + 66.0, area.top()), area.right_bottom());
                            let galley = ui.fonts(|f| f.layout_job(highlighted(text, ranges, font.clone(), theme::p().text, theme::p().text, mark)));
                            let pos = egui::pos2(clip.left() - hx, area.center().y - galley.size().y / 2.0);
                            ui.painter_at(clip).galley(pos, galley, theme::p().text);
                        };
                        let mut long: Option<String> = None;
                        if self.unified {
                            let u = &t.urows[i];
                            match u {
                                URow::Equal(a, s) => line(ui, rect, Some(*a), s, &[], None, Color32::TRANSPARENT, ""),
                                URow::Del(a, s, rg) => line(ui, rect, Some(*a), s, rg, Some(red), red_s, "−"),
                                URow::Ins(b, s, rg) => line(ui, rect, Some(*b), s, rg, Some(green), green_s, "+"),
                                URow::Folded(n) => folded(ui, rect, *n),
                            }
                            if let URow::Equal(_, s) | URow::Del(_, s, _) | URow::Ins(_, s, _) = u {
                                if s.chars().count() > 110 {
                                    long = Some(s.clone());
                                }
                            }
                        } else {
                            let (l, r) = (egui::Rect::from_min_size(rect.min, egui::vec2(half, ROW_H)), egui::Rect::from_min_size(rect.min + egui::vec2(half, 0.0), egui::vec2(rect.width() - half, ROW_H)));
                            match &t.rows[i] {
                                Row::Equal(a, b, s) => {
                                    line(ui, l, Some(*a), s, &[], None, Color32::TRANSPARENT, "");
                                    line(ui, r, Some(*b), s, &[], None, Color32::TRANSPARENT, "");
                                    if s.chars().count() > 55 {
                                        long = Some(s.clone());
                                    }
                                }
                                Row::Del(a, s, rg) => {
                                    line(ui, l, Some(*a), s, rg, Some(red), red_s, "");
                                    ui.painter().rect_filled(r, 0.0, theme::p().hover.gamma_multiply(0.4));
                                    long = (s.chars().count() > 55).then(|| s.clone());
                                }
                                Row::Ins(b, s, rg) => {
                                    ui.painter().rect_filled(l, 0.0, theme::p().hover.gamma_multiply(0.4));
                                    line(ui, r, Some(*b), s, rg, Some(green), green_s, "");
                                    long = (s.chars().count() > 55).then(|| s.clone());
                                }
                                Row::Change(a, sa, ra, b, sb, rb) => {
                                    line(ui, l, Some(*a), sa, ra, Some(red), red_s, "");
                                    line(ui, r, Some(*b), sb, rb, Some(green), green_s, "");
                                    if sa.chars().count() > 55 || sb.chars().count() > 55 {
                                        long = Some(format!("A: {sa}\nB: {sb}"));
                                    }
                                }
                                Row::Folded(n) => folded(ui, rect, *n),
                            }
                            // A thin divider between the two sides.
                            ui.painter().line_segment([egui::pos2(rect.left() + half, rect.top()), egui::pos2(rect.left() + half, rect.bottom())], egui::Stroke::new(1.0_f32, theme::p().border));
                        }
                        if let Some(text) = long {
                            resp.on_hover_text(text);
                        }
                    }
                }));
                if max_hx > 0.0 {
                    ui.spacing_mut().slider_width = (list_w - 20.0).max(100.0);
                    ui.add(egui::Slider::new(&mut self.offset_x, 0.0..=max_hx).show_value(false)).on_hover_text(tr("Desplazar el texto de los dos lados (Mayús + rueda del ratón)"));
                }
            });
            if list.response.hovered() && max_hx > 0.0 {
                let dx = ui.input(|i| i.smooth_scroll_delta.x + if i.modifiers.shift { i.smooth_scroll_delta.y } else { 0.0 });
                self.offset_x = (self.offset_x - dx).clamp(0.0, max_hx);
            }
            // Overview strip: where the differences are in the whole file; click or drag to jump.
            let (mrect, mresp) = ui.allocate_exact_size(egui::vec2(mini_w, area_h - 22.0), egui::Sense::click_and_drag());
            ui.painter().rect_filled(mrect, 3.0, theme::p().hover);
            let n = total_rows.max(1) as f32;
            let kinds: Vec<(usize, Color32)> = if self.unified {
                t.urows.iter().enumerate().filter_map(|(i, r)| match r {
                    URow::Del(..) => Some((i, red_s)),
                    URow::Ins(..) => Some((i, green_s)),
                    _ => None,
                }).collect()
            } else {
                t.rows.iter().enumerate().filter_map(|(i, r)| match r {
                    Row::Del(..) => Some((i, red_s)),
                    Row::Ins(..) => Some((i, green_s)),
                    Row::Change(..) => Some((i, Color32::from_rgba_unmultiplied(230, 150, 40, 170))),
                    _ => None,
                }).collect()
            };
            for (i, c) in kinds {
                let y = mrect.top() + i as f32 / n * mrect.height();
                ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(mrect.left() + 2.0, y), egui::vec2(mrect.width() - 4.0, 2.5)), 1.0, c);
            }
            let (content_h, view_h) = out.as_ref().map(|o| (o.content_size.y, o.inner_rect.height())).unwrap_or((n * ROW_H, 300.0));
            self.viewport_h = view_h;
            if content_h > 0.0 {
                let top = mrect.top() + self.offset / content_h * mrect.height();
                let h = (view_h / content_h * mrect.height()).clamp(8.0, mrect.height());
                ui.painter().rect_stroke(egui::Rect::from_min_size(egui::pos2(mrect.left(), top.min(mrect.bottom() - h)), egui::vec2(mrect.width(), h)), 2.0, egui::Stroke::new(1.5_f32, theme::accent()), egui::StrokeKind::Inside);
            }
            if mresp.clicked() || mresp.dragged() {
                if let Some(pos) = mresp.interact_pointer_pos() {
                    let frac = ((pos.y - mrect.top()) / mrect.height()).clamp(0.0, 1.0);
                    self.scroll_to = Some((frac * content_h - view_h / 2.0).max(0.0));
                }
            }
            mresp.on_hover_text(tr("Mapa de diferencias: haz clic para saltar"));
        });
        if let Some(o) = &out {
            self.offset = o.state.offset.y;
        }
    }
}

fn folded(ui: &mut egui::Ui, rect: egui::Rect, n: usize) {
    ui.painter().rect_filled(rect, 0.0, theme::p().hover);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, trf!("⋯ {} líneas iguales ⋯", n), FontId::proportional(12.0), theme::p().sub);
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: Options = Options { ignore_ws: false, ignore_case: false, show_all: true };

    #[test]
    fn line_diff_pairs_changes_and_marks_words() {
        let r = diff_text("a\nb\nc\nd\n", "a\nB x\nc\nnuevo\nd\n", O);
        assert_eq!((r.added, r.removed, r.same), (2, 1, false));
        assert!(matches!(&r.rows[1], Row::Change(2, a, _, 2, b, _) if a == "b" && b == "B x"));
        assert!(r.rows.iter().any(|x| matches!(x, Row::Ins(4, s, _) if s == "nuevo")));
        assert_eq!(r.blocks.len(), 2);
        let (ra, rb) = inline_ranges("hola mundo", "hola mundial");
        assert!(ra.iter().all(|(s, e)| s < e) && !rb.is_empty());
        assert!(diff_text("x\ny\n", "x\ny\n", O).same);
    }

    #[test]
    fn ignore_options_and_folding() {
        let o = Options { ignore_ws: true, ignore_case: true, show_all: true };
        assert!(diff_text("Hola   mundo\n", "hola mundo\n", o).same);
        assert!(!diff_text("Hola   mundo\n", "hola mundo\n", O).same);
        let big: String = (0..100).map(|i| format!("{i}\n")).collect();
        let changed = big.replace("\n50\n", "\nCINCUENTA\n");
        let r = diff_text(&big, &changed, Options { show_all: false, ..O });
        assert!(r.rows.iter().any(|x| matches!(x, Row::Folded(n) if *n > 40)), "long equal stretches fold");
        assert!(r.rows.len() < 20 && r.blocks.len() == 1);
    }

    #[test]
    fn binary_files_report_first_difference() {
        let d = std::env::temp_dir().join(format!("cmp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.bin"), [0u8, 1, 2, 3, 4]).unwrap();
        fs::write(d.join("b.bin"), [0u8, 1, 9, 3, 4, 5]).unwrap();
        fs::write(d.join("c.bin"), [0u8, 1, 2, 3, 4]).unwrap();
        let Outcome::Binary(b) = compare(&d.join("a.bin"), &d.join("b.bin"), O) else { panic!("binary") };
        assert_eq!((b.first_diff, b.differing, b.sizes), (Some(2), 2, (5, 6)));
        let Outcome::Binary(same) = compare(&d.join("a.bin"), &d.join("c.bin"), O) else { panic!("binary") };
        assert!(same.hashes.0 == same.hashes.1 && same.first_diff.is_none());
        assert!(matches!(compare(&d.join("x"), &d.join("a.bin"), O), Outcome::Failed(_)));
        fs::remove_dir_all(&d).unwrap();
    }
}
