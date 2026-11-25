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

struct TextResult {
    rows: Vec<Row>,
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
    TextResult { rows, blocks, added, removed, same }
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
        (Ok(x), Ok(y)) => Outcome::Text(diff_text(&x, &y, o)),
        (Err(e), _) | (_, Err(e)) => Outcome::Failed(e),
    }
}

pub struct Comparer {
    a: String,
    b: String,
    pick: Option<(bool, Picker)>,
    opts: Options,
    outcome: Option<Outcome>,
    rx: Option<Receiver<Outcome>>,
    current: usize,
    scroll_to: Option<f32>,
    base: PathBuf,
}

impl Comparer {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Comparer {
        let files: Vec<&PathBuf> = targets.iter().filter(|p| p.is_file()).collect();
        Comparer {
            a: files.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            b: files.get(1).map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            pick: None,
            opts: Options { ignore_ws: false, ignore_case: false, show_all: false },
            outcome: None,
            rx: None,
            current: 0,
            scroll_to: None,
            base,
        }
    }

    /// Compares two small files at once, for trying the tool (`--compare`).
    pub fn demo(&mut self, dir: &Path) {
        let (a, b) = (dir.join("comparar_a.txt"), dir.join("comparar_b.txt"));
        let _ = fs::write(&a, "uno\ndos\ntres\ncuatro\ncinco\nseis\nsiete\nocho\nnueve\ndiez\nonce\ndoce\n");
        let _ = fs::write(&b, "uno\ndos\ntres modificado\ncuatro\ncinco\nseis\nsiete\nocho\nnueve\nnuevo renglón\ndiez\nonce\n");
        self.a = a.to_string_lossy().to_string();
        self.b = b.to_string_lossy().to_string();
        self.start();
    }

    fn start(&mut self) {
        let (a, b, o) = (PathBuf::from(self.a.trim()), PathBuf::from(self.b.trim()), self.opts);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(compare(&a, &b, o));
        });
        self.outcome = None;
        self.current = 0;
        self.rx = Some(rx);
    }

    fn path_row(&mut self, ui: &mut egui::Ui, left: bool) {
        let label = if left { tr("Archivo A") } else { tr("Archivo B") };
        ui.horizontal(|ui| {
            ui.add_sized([80.0, 20.0], egui::Label::new(label));
            let text = if left { &mut self.a } else { &mut self.b };
            ui.add(egui::TextEdit::singleline(text).desired_width(ui.available_width() - 120.0));
            if ui.button(tr("📂 Examinar…")).clicked() {
                let cur = if left { &self.a } else { &self.b };
                let start = Path::new(cur.trim()).parent().filter(|p| p.is_dir()).map(Path::to_path_buf).unwrap_or_else(|| self.base.clone());
                self.pick = Some((left, Picker::new(start)));
            }
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, _events: &mut Vec<ToolEvent>, selection: &[PathBuf], _cwd: &Path) {
        if let Some(rx) = &self.rx {
            if let Ok(o) = rx.try_recv() {
                self.outcome = Some(o);
                self.rx = None;
            }
        }
        let busy = self.rx.is_some();
        ui.label(RichText::new(tr("ARCHIVOS A COMPARAR")).small().strong().color(theme::p().sub));
        self.path_row(ui, true);
        self.path_row(ui, false);
        let files: Vec<&PathBuf> = selection.iter().filter(|p| p.is_file()).collect();
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(files.len() >= 2, egui::Button::new(tr("Usar los dos seleccionados"))).clicked() {
                self.a = files[0].to_string_lossy().to_string();
                self.b = files[1].to_string_lossy().to_string();
            }
            if ui.button(tr("⇄ Intercambiar")).clicked() {
                std::mem::swap(&mut self.a, &mut self.b);
            }
            ui.separator();
            let before = self.opts;
            ui.checkbox(&mut self.opts.ignore_ws, tr("Ignorar espacios"));
            ui.checkbox(&mut self.opts.ignore_case, tr("Ignorar mayúsculas"));
            ui.checkbox(&mut self.opts.show_all, tr("Mostrar también lo igual"));
            if before != self.opts && self.outcome.is_some() {
                self.start();
            }
        });
        ui.horizontal(|ui| {
            let ready = !busy && !self.a.trim().is_empty() && !self.b.trim().is_empty();
            let btn = egui::Button::new(RichText::new(tr("⚖ Comparar")).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(ready, btn).clicked() {
                self.start();
            }
            if busy {
                ui.spinner();
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        let ctx = ui.ctx().clone();
        if let Some((left, pk)) = self.pick.take() {
            let mut slot = Some(pk);
            let picked = picker_window(&ctx, "compare_pick", &mut slot);
            if let Some(f) = picked.iter().find(|p| p.is_file()) {
                *(if left { &mut self.a } else { &mut self.b }) = f.to_string_lossy().to_string();
                slot = None;
            }
            self.pick = slot.map(|pk| (left, pk));
        }
        ui.separator();
        match self.outcome.take() {
            None => {}
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
        ui.label(RichText::new(if same { tr("✔ Los dos archivos son idénticos") } else { tr("✘ Los archivos son distintos") }).strong().color(if same { theme::p().ok } else { theme::p().danger }));
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
        ui.horizontal_wrapped(|ui| {
            if t.same {
                ui.label(RichText::new(tr("✔ No hay diferencias")).strong().color(theme::p().ok));
            } else {
                ui.label(RichText::new(trf!("{} bloque(s) de cambios · +{} líneas · −{} líneas", t.blocks.len(), t.added, t.removed)).strong());
                if ui.small_button(tr("⬆ Anterior")).clicked() && !t.blocks.is_empty() {
                    self.current = (self.current + t.blocks.len() - 1) % t.blocks.len();
                    self.scroll_to = Some(t.blocks[self.current] as f32 * 22.0);
                }
                if ui.small_button(tr("⬇ Siguiente")).clicked() && !t.blocks.is_empty() {
                    self.current = (self.current + 1) % t.blocks.len();
                    self.scroll_to = Some(t.blocks[self.current] as f32 * 22.0);
                }
                if !t.blocks.is_empty() {
                    ui.label(RichText::new(format!("{}/{}", self.current + 1, t.blocks.len())).small().color(theme::p().sub));
                }
            }
        });
        if t.same {
            return;
        }
        let font = FontId::monospace(13.0);
        let (red, green) = (Color32::from_rgba_unmultiplied(220, 70, 60, 40), Color32::from_rgba_unmultiplied(60, 170, 90, 40));
        let mut area = egui::ScrollArea::both().id_salt("compare_rows").max_height(crate::tools::grow(ui, 380.0)).auto_shrink([false, false]);
        if let Some(off) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        area.show_rows(ui, 22.0, t.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let half = (ui.available_width() / 2.0).max(300.0);
            for i in range {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(half * 2.0, 22.0), egui::Sense::hover());
                let (l, r) = (egui::Rect::from_min_size(rect.min, egui::vec2(half, 22.0)), egui::Rect::from_min_size(rect.min + egui::vec2(half, 0.0), egui::vec2(half, 22.0)));
                let side = |ui: &mut egui::Ui, area: egui::Rect, no: Option<usize>, text: &str, ranges: &[(usize, usize)], bg: Option<Color32>, mark: Color32| {
                    if let Some(bg) = bg {
                        ui.painter().rect_filled(area, 0.0, bg);
                    }
                    if let Some(n) = no {
                        ui.painter().text(egui::pos2(area.left() + 44.0, area.center().y), egui::Align2::RIGHT_CENTER, n.to_string(), FontId::monospace(11.0), theme::p().muted);
                    }
                    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(egui::pos2(area.left() + 52.0, area.top()), area.right_bottom())).layout(egui::Layout::left_to_right(egui::Align::Center)));
                    inner.add(egui::Label::new(highlighted(text, ranges, font.clone(), theme::p().text, theme::p().text, mark)).truncate());
                };
                match &t.rows[i] {
                    Row::Equal(a, b, s) => {
                        side(ui, l, Some(*a), s, &[], None, Color32::TRANSPARENT);
                        side(ui, r, Some(*b), s, &[], None, Color32::TRANSPARENT);
                    }
                    Row::Del(a, s, rg) => side(ui, l, Some(*a), s, rg, Some(red), Color32::from_rgba_unmultiplied(220, 70, 60, 110)),
                    Row::Ins(b, s, rg) => side(ui, r, Some(*b), s, rg, Some(green), Color32::from_rgba_unmultiplied(60, 170, 90, 110)),
                    Row::Change(a, sa, ra, b, sb, rb) => {
                        side(ui, l, Some(*a), sa, ra, Some(red), Color32::from_rgba_unmultiplied(220, 70, 60, 120));
                        side(ui, r, Some(*b), sb, rb, Some(green), Color32::from_rgba_unmultiplied(60, 170, 90, 120));
                    }
                    Row::Folded(n) => {
                        ui.painter().rect_filled(rect, 0.0, theme::p().hover);
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, trf!("⋯ {} líneas iguales ⋯", n), FontId::proportional(12.0), theme::p().sub);
                    }
                }
            }
        });
    }
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
