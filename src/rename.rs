//! Batch rename: a list of files, a chain of rules (placeholders, regular expressions, case,
//! insert, remove, numbering, clean-up, extension), a live preview with conflicts, undo and presets.

use crate::fsops::human_size;
use crate::i18n::tr;
use crate::theme;
use crate::tools::ToolEvent;
use eframe::egui::{self, Color32, RichText};
use regex::{NoExpand, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Scope {
    Name,
    Ext,
    Full,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum RuleKind {
    Template { pattern: String },
    Replace {
        find: String,
        with: String,
        regex: bool,
        #[serde(default)]
        wildcard: bool,
        case_sensitive: bool,
        all: bool,
    },
    Case { mode: u8 },
    Insert { text: String, pos: usize, from_end: bool },
    Remove { from: usize, count: usize, from_end: bool },
    Number { suffix: bool, sep: String },
    Clean { trim: bool, collapse: bool, spaces: u8, accents: bool, special: bool },
    Ext { mode: u8, text: String },
    PadNumbers { digits: usize },
    RemoveBetween { open: String, close: String },
    Truncate { max: usize, from_end: bool },
    Swap { sep: String, joiner: String },
    Strip { mode: u8 },
    Prefix { text: String },
    Suffix { text: String },
    Counter { start: i64, step: i64, pad: usize, style: u8, pos: u8, at: usize, sep: String, reset: u8, group: String },
    Accumulate { what: u8, inclusive: bool, pos: u8, at: usize, sep: String },
    Variable { name: String, value: String, mode: u8, sep: String },
    Awk {
        script: String,
        fs: String,
        input: u8,
        /// Use the awk installed on the computer instead of the one built in.
        #[serde(default)]
        system: bool,
    },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub on: bool,
    pub scope: Scope,
    pub kind: RuleKind,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Counter {
    pub start: i64,
    pub step: i64,
    pub pad: usize,
    pub per_folder: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Preset {
    rules: Vec<Rule>,
    counter: Counter,
    conflicts: u8,
}

const CASE_MODES: [&str; 5] = ["minúsculas", "MAYÚSCULAS", "Tipo Título", "Tipo oración", "iNVERTIR"];
const SPACE_MODES: [&str; 5] = ["Dejar espacios", "Espacios » _", "Espacios » -", "Espacios » .", "Quitar espacios"];
const REGEX_EXAMPLES: &[(&str, &str, &str)] = &[
    (r"^(\d+)[ ._-]+(.*)$", "$2 ($1)", "Mueve el número del principio al final"),
    (r"^(.*?) - (.*)$", "$2 - $1", "Intercambia lo que hay a cada lado de « - »"),
    (r"(\d{4})(\d{2})(\d{2})", "$1-$2-$3", "20240531 » 2024-05-31"),
    (r"(\d{2})[-.](\d{2})[-.](\d{4})", "$3-$2-$1", "31-05-2024 » 2024-05-31"),
    (r"\s+", " ", "Varios espacios seguidos » uno"),
    (r"[^\w\s.-]", "", "Quita todo lo que no sea letra, número, espacio, punto o guion"),
    (r"\s*\d+$", "", "Quita el número del final"),
    (r"^(?P<serie>.+?)[ ._-]+[Ss](?P<t>\d+)[Ee](?P<e>\d+).*$", "${serie} - T${t}E${e}", "Series: «Serie.S01E02.720p» » «Serie - T01E02»"),
];

const AWK_EXAMPLES: &[(&str, u8, &str)] = &[
    ("{ print toupper($0) }", 0, "Todo en mayúsculas"),
    ("{ printf \"%03d %s\\n\", NR, $0 }", 0, "Numerar 001, 002… en el orden de la lista"),
    ("{ gsub(/[ ]+/, \"_\"); print }", 0, "Espacios a guiones bajos"),
    ("{ n = split($0, p, \"_\"); print p[n] \"_\" p[1] }", 0, "Con partes separadas por _: la última delante de la primera"),
    ("{ print $NF \" \" $1 }", 0, "Última palabra y primera palabra"),
    ("{ print $6 \" \" $1 }", 1, "Fecha de modificación delante (modo datos)"),
    ("{ print ($5 > 1048576 ? \"GRANDE_\" : \"\") $1 }", 1, "Marca los archivos de más de 1 MB (modo datos)"),
    ("{ print $4 \" - \" $1 }", 1, "Nombre de la carpeta delante (modo datos)"),
    ("NR == 1 { first = $0 } { print first \" \" NR }", 0, "Todos con el nombre del primero y su número"),
];

const COUNTER_STYLES: [&str; 4] = ["Números 01 02", "Letras a b c", "Letras A B C", "Romanos I II III"];
const ACCUM_KINDS: [&str; 3] = ["Tamaño acumulado", "Duración acumulada (música y vídeo)", "Suma del primer número del nombre"];
const STRIP_MODES: [&str; 3] = ["Números del principio (01 - , 1. )", "Todos los números", "Marcas de copia: (1), - copia, copy"];
const EXT_MODES: [&str; 5] = ["minúsculas", "MAYÚSCULAS", "Cambiar a", "Quitar extensión", "Añadir extensión"];

impl RuleKind {
    fn title(&self) -> &'static str {
        match self {
            RuleKind::Template { .. } => tr("Nuevo nombre con marcadores"),
            RuleKind::Replace { .. } => tr("Buscar y reemplazar"),
            RuleKind::Case { .. } => tr("Mayúsculas y minúsculas"),
            RuleKind::Insert { .. } => tr("Insertar texto"),
            RuleKind::Remove { .. } => tr("Eliminar caracteres"),
            RuleKind::Number { .. } => tr("Numeración"),
            RuleKind::Clean { .. } => tr("Limpiar nombre"),
            RuleKind::Ext { .. } => tr("Extensión"),
            RuleKind::PadNumbers { .. } => tr("Rellenar números con ceros"),
            RuleKind::RemoveBetween { .. } => tr("Quitar texto entre símbolos"),
            RuleKind::Truncate { .. } => tr("Limitar longitud"),
            RuleKind::Swap { .. } => tr("Intercambiar partes"),
            RuleKind::Strip { .. } => tr("Quitar restos habituales"),
            RuleKind::Prefix { .. } => tr("Añadir al principio"),
            RuleKind::Suffix { .. } => tr("Añadir al final"),
            RuleKind::Counter { .. } => tr("Contador incremental"),
            RuleKind::Accumulate { .. } => tr("Acumulado"),
            RuleKind::Variable { .. } => tr("Guardar en variable"),
            RuleKind::Awk { .. } => tr("Script awk (avanzado)"),
        }
    }
    /// One line under the title: what the rule does, with an example.
    fn help(&self) -> &'static str {
        match self {
            RuleKind::Template { .. } => tr("Escribe el nombre entero con marcadores: «{fecha} {nombre}»."),
            RuleKind::Replace { .. } => tr("Cambia un texto por otro: «IMG_1234» » «Foto 1234»."),
            RuleKind::Case { .. } => tr("Pasa a minúsculas, MAYÚSCULAS o Tipo Título."),
            RuleKind::Insert { .. } => tr("Mete un texto en una posición del nombre."),
            RuleKind::Remove { .. } => tr("Quita caracteres de una posición: «2024_foto» » «foto»."),
            RuleKind::Number { .. } => tr("Añade el contador {n} delante o detrás."),
            RuleKind::Clean { .. } => tr("Arregla espacios, tildes y símbolos raros."),
            RuleKind::Ext { .. } => tr("Cambia la extensión: «.JPG» » «.jpg»."),
            RuleKind::PadNumbers { .. } => tr("Pone ceros delante de los números: «foto 2» » «foto 002»."),
            RuleKind::RemoveBetween { .. } => tr("Quita lo que hay entre dos símbolos: «canción (live)» » «canción»."),
            RuleKind::Truncate { .. } => tr("Corta los nombres demasiado largos."),
            RuleKind::Swap { .. } => tr("Da la vuelta a dos partes: «Artista - Canción» » «Canción - Artista»."),
            RuleKind::Strip { .. } => tr("Quita números del principio o marcas de copia: «foto (1)» » «foto»."),
            RuleKind::Prefix { .. } => tr("Pone un texto delante: «foto» » «Viaje foto»."),
            RuleKind::Suffix { .. } => tr("Pone un texto detrás: «foto» » «foto Viaje»."),
            RuleKind::Counter { .. } => tr("Numera la lista en su orden: «foto» » «01 - foto»."),
            RuleKind::Accumulate { .. } => tr("Pone el total acumulado (tamaño, duración…) de los anteriores."),
            RuleKind::Variable { .. } => tr("No cambia el nombre: guarda un valor que pasa al siguiente archivo."),
            RuleKind::Awk { .. } => tr("Pasa todos los nombres por un script awk, para casos muy especiales."),
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            RuleKind::Template { .. } => "🏷",
            RuleKind::Replace { .. } => "🔍",
            RuleKind::Case { .. } => "🔠",
            RuleKind::Insert { .. } => "➕",
            RuleKind::Remove { .. } => "✂",
            RuleKind::Number { .. } => "#",
            RuleKind::Clean { .. } => "✨",
            RuleKind::Ext { .. } => "📎",
            RuleKind::PadNumbers { .. } => "0",
            RuleKind::RemoveBetween { .. } => "( )",
            RuleKind::Truncate { .. } => "📏",
            RuleKind::Swap { .. } => "🔀",
            RuleKind::Strip { .. } => "🗑",
            RuleKind::Prefix { .. } => "⏮",
            RuleKind::Suffix { .. } => "⏭",
            RuleKind::Counter { .. } => "#",
            RuleKind::Accumulate { .. } => "∑",
            RuleKind::Variable { .. } => "📦",
            RuleKind::Awk { .. } => ">_",
        }
    }
    fn all_defaults() -> Vec<RuleKind> {
        vec![
            RuleKind::Prefix { text: String::new() },
            RuleKind::Suffix { text: String::new() },
            RuleKind::Counter { start: 1, step: 1, pad: 2, style: 0, pos: 0, at: 0, sep: " - ".into(), reset: 0, group: "{carpeta}".into() },
            RuleKind::Template { pattern: "{nombre}".into() },
            RuleKind::Replace { find: String::new(), with: String::new(), regex: false, wildcard: false, case_sensitive: false, all: true },
            RuleKind::Insert { text: String::new(), pos: 0, from_end: false },
            RuleKind::Remove { from: 0, count: 1, from_end: false },
            RuleKind::Case { mode: 2 },
            RuleKind::Clean { trim: true, collapse: true, spaces: 0, accents: false, special: false },
            RuleKind::PadNumbers { digits: 3 },
            RuleKind::RemoveBetween { open: "(".into(), close: ")".into() },
            RuleKind::Truncate { max: 40, from_end: false },
            RuleKind::Swap { sep: " - ".into(), joiner: " - ".into() },
            RuleKind::Strip { mode: 2 },
            RuleKind::Accumulate { what: 1, inclusive: false, pos: 0, at: 0, sep: " - ".into() },
            RuleKind::Variable { name: "x".into(), value: "{nombre}".into(), mode: 1, sep: "+".into() },
            RuleKind::Awk { script: "{ print NR \". \" $0 }".into(), fs: String::new(), input: 0, system: false },
            RuleKind::Ext { mode: 0, text: String::new() },
        ]
    }
    fn uses_scope(&self) -> bool {
        !matches!(self, RuleKind::Number { .. } | RuleKind::Ext { .. } | RuleKind::Variable { .. })
    }
}

#[derive(Clone, PartialEq)]
enum Status {
    Unchanged,
    Ok,
    Conflict(String),
    Invalid(String),
}

struct Item {
    path: PathBuf,
    is_dir: bool,
    on: bool,
    meta: Option<fs::Metadata>,
    new: String,
    status: Status,
    tags: OnceCell<Tags>,
    original: PathBuf,
}

impl Item {
    fn new(path: PathBuf) -> Item {
        let meta = fs::metadata(&path).ok();
        Item { is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false), on: true, new: String::new(), status: Status::Unchanged, meta, original: path.clone(), path, tags: OnceCell::new() }
    }
    fn name(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().to_string()
    }
}

pub struct Renamer {
    items: Vec<Item>,
    rules: Vec<Rule>,
    counter: Counter,
    conflicts: u8,
    rule_errors: Vec<Option<String>>,
    signature: String,
    selected: Option<usize>,
    add_recursive: bool,
    add_mask: String,
    add_folders: bool,
    add_regex: bool,
    /// Rules whose options are folded away (by position).
    folded: HashSet<usize>,
    /// Widths of the two name columns in the preview, once the person drags a divider.
    cols: Option<[f32; 2]>,
    /// The browsers behind «Elegir carpeta…» and «Elegir archivos…».
    folder_browser: Option<crate::dupes::Browser>,
    file_picker: Option<crate::roots::Picker>,
    history: Vec<Batch>,
    preset_name: String,
    undo: Vec<Vec<(PathBuf, PathBuf)>>,
    status: Option<(String, bool)>,
    problems: Vec<String>,
    base: PathBuf,
}

impl Renamer {
    pub fn new(targets: Vec<PathBuf>, base: PathBuf) -> Renamer {
        let mut r = Renamer {
            items: vec![],
            rules: vec![Rule { on: true, scope: Scope::Name, kind: RuleKind::all_defaults().remove(1) }],
            counter: Counter { start: 1, step: 1, pad: 2, per_folder: false },
            conflicts: 0,
            rule_errors: vec![],
            signature: String::new(),
            selected: None,
            add_recursive: false,
            add_mask: String::new(),
            add_folders: false,
            add_regex: false,
            folded: HashSet::new(),
            cols: None,
            folder_browser: None,
            file_picker: None,
            history: load_history(),
            preset_name: String::new(),
            undo: vec![],
            status: None,
            problems: vec![],
            base: base.clone(),
        };
        if targets.is_empty() {
            r.add_folder(&base);
        } else {
            r.add_paths(targets);
        }
        r
    }

    /// Sample rules, for trying the tool (`--rename`).
    pub fn demo(&mut self) {
        let r = |kind| Rule { on: true, scope: Scope::Name, kind };
        self.rules = vec![
            r(RuleKind::Clean { trim: true, collapse: true, spaces: 0, accents: false, special: false }),
            r(RuleKind::Prefix { text: "Viaje ".into() }),
            r(RuleKind::Counter { start: 1, step: 1, pad: 3, style: 0, pos: 1, at: 0, sep: " #".into(), reset: 0, group: "{carpeta}".into() }),
            r(RuleKind::Awk { script: "{ print toupper(substr($0, 1, 1)) substr($0, 2) \" (\" NR \" de \" total \")\" }".into(), fs: String::new(), input: 0, system: false }),
            r(RuleKind::Ext { mode: 0, text: String::new() }),
        ];
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let have: HashSet<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        let mut n = 0;
        for p in paths {
            if !have.contains(&p) && p.exists() {
                self.items.push(Item::new(p));
                n += 1;
            }
        }
        self.signature.clear();
        n
    }

    fn add_folder(&mut self, dir: &Path) -> usize {
        let matcher = match name_matcher(&self.add_mask.replace(',', ";"), self.add_regex) {
            Ok(m) => m,
            Err(e) => {
                self.status = Some((trf!("Expresión no válida: {}", e), true));
                return 0;
            }
        };
        let mut found = vec![];
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir && self.add_recursive {
                    stack.push(e.path());
                }
                if is_dir && !self.add_folders {
                    continue;
                }
                let lname = name.to_lowercase();
                if matcher.as_ref().map(|m| m.is_match(&lname)).unwrap_or(true) {
                    found.push(e.path());
                }
            }
        }
        self.add_paths(found)
    }

    // ---------------- computing ----------------

    fn recompute(&mut self) {
        let mut errors = vec![None; self.rules.len()];
        let regs: Vec<Option<Regex>> = self
            .rules
            .iter()
            .enumerate()
            .map(|(i, r)| match &r.kind {
                RuleKind::Replace { find, regex, wildcard, case_sensitive, .. } if r.on && !find.is_empty() => {
                    let pat = if *regex {
                        find.clone()
                    } else if *wildcard {
                        wildcard_to_regex(find)
                    } else {
                        regex::escape(find)
                    };
                    match RegexBuilder::new(&pat).case_insensitive(!case_sensitive).build() {
                        Ok(re) => Some(re),
                        Err(e) => {
                            errors[i] = Some(e.to_string().lines().last().unwrap_or("").trim().to_string());
                            None
                        }
                    }
                }
                _ => None,
            })
            .collect();
        self.rule_errors = errors;

        let total = self.items.iter().filter(|i| i.on).count();
        let now = SystemTime::now();
        let vars: std::cell::RefCell<HashMap<String, String>> = Default::default();
        let mut states: Vec<RState> = self.rules.iter().map(|_| RState::default()).collect();
        let mut k = 0i64;
        let mut per_folder: HashMap<PathBuf, i64> = HashMap::new();
        let positions: Vec<i64> = self
            .items
            .iter()
            .map(|item| {
                if !item.on {
                    0
                } else if self.counter.per_folder {
                    let c = per_folder.entry(item.path.parent().map(Path::to_path_buf).unwrap_or_default()).or_insert(0);
                    *c += 1;
                    *c - 1
                } else {
                    k += 1;
                    k - 1
                }
            })
            .collect();
        // Each item's (name, extension) as the rules change it.
        let mut work: Vec<(String, String)> = self.items.iter().map(|i| split(&i.name(), i.is_dir)).collect();
        let mut awk_errors: Vec<(usize, String)> = vec![];
        let nrules = self.rules.len();
        let mut seg_start = 0;
        loop {
        // Rules run file by file up to the next awk script, which sees every name at once.
        let seg_end = (seg_start..nrules).find(|&i| self.rules[i].on && matches!(self.rules[i].kind, RuleKind::Awk { .. })).unwrap_or(nrules);
        for (idx, item) in self.items.iter_mut().enumerate() {
            let name = item.path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !item.on {
                continue;
            }
            let pos = positions[idx];
            let (stem0, ext0) = split(&name, item.is_dir);
            let ctx = Ctx {
                n: self.counter.start + self.counter.step * pos,
                pad: self.counter.pad,
                path: &item.path,
                stem: &stem0,
                ext: &ext0,
                meta: item.meta.as_ref(),
                total,
                now,
                seed: idx as u64,
                tags: &item.tags,
                vars: &vars,
            };
            let (mut stem, mut ext) = work[idx].clone();
            for (ri, (r, re)) in self.rules.iter().zip(&regs).enumerate().take(seg_end).skip(seg_start) {
                if !r.on {
                    continue;
                }
                match &r.kind {
                    RuleKind::Variable { name: vname, value, mode, sep } => {
                        let v = expand(value, &ctx, &stem);
                        let mut vars = vars.borrow_mut();
                        let e = vars.entry(vname.trim().to_lowercase()).or_default();
                        *e = match mode {
                            0 => v,
                            1 if e.is_empty() => v,
                            1 => format!("{e}{sep}{v}"),
                            _ => fmt_num(first_number(e).unwrap_or(0.0) + first_number(&v).unwrap_or(0.0)),
                        };
                    }
                    kind @ (RuleKind::Counter { .. } | RuleKind::Accumulate { .. }) => {
                        let st = &mut states[ri];
                        let (value, pos, at, sep) = match kind {
                            RuleKind::Counter { start, step, pad, style, pos, at, sep, reset, group } => {
                                let k = match reset {
                                    1 => {
                                        let c = st.folders.entry(item.path.parent().map(Path::to_path_buf).unwrap_or_default()).or_insert(0);
                                        *c += 1;
                                        *c - 1
                                    }
                                    2 => {
                                        let key = expand(group, &ctx, &stem);
                                        if st.last_key.as_deref() == Some(key.as_str()) {
                                            st.k += 1;
                                        } else {
                                            st.k = 0;
                                            st.last_key = Some(key);
                                        }
                                        st.k
                                    }
                                    _ => {
                                        st.k += 1;
                                        st.k - 1
                                    }
                                };
                                (fmt_counter(start + step * k, *pad, *style), *pos, *at, sep.as_str())
                            }
                            RuleKind::Accumulate { what, inclusive, pos, at, sep } => {
                                let amount = match what {
                                    0 => item.meta.as_ref().map(|m| m.len() as f64).unwrap_or(0.0),
                                    1 => ctx.tags().duration.unwrap_or(0) as f64,
                                    _ => first_number(&stem0).unwrap_or(0.0),
                                };
                                let before = st.total;
                                st.total += amount;
                                let v = if *inclusive { st.total } else { before };
                                let text = match what {
                                    0 => human_size(v as u64),
                                    1 => fmt_duration(v as u64),
                                    _ => fmt_num(v),
                                };
                                (text, *pos, *at, sep.as_str())
                            }
                            _ => unreachable!(),
                        };
                        let put = |s: &str| place(s, &value, pos, at, sep);
                        match r.scope {
                            Scope::Name => stem = put(&stem),
                            Scope::Ext => ext = put(&ext),
                            Scope::Full => {
                                let out = put(&join(&stem, &ext));
                                (stem, ext) = split(&out, item.is_dir);
                            }
                        }
                    }
                    RuleKind::Ext { mode, text } => {
                        if item.is_dir {
                            continue;
                        }
                        let t = text.trim().trim_start_matches('.');
                        ext = match mode {
                            0 => ext.to_lowercase(),
                            1 => ext.to_uppercase(),
                            2 => t.to_string(),
                            3 => String::new(),
                            _ if t.is_empty() => ext,
                            _ if ext.is_empty() => t.to_string(),
                            _ => format!("{ext}.{t}"),
                        };
                    }
                    RuleKind::Number { suffix, sep } => {
                        let num = ctx.number(None);
                        stem = if *suffix { format!("{stem}{sep}{num}") } else { format!("{num}{sep}{stem}") };
                    }
                    kind => {
                        let apply = |s: &str, cur: &str| apply_rule(kind, re.as_ref(), s, &ctx, cur);
                        match r.scope {
                            Scope::Name => stem = apply(&stem, &stem),
                            Scope::Ext => ext = apply(&ext, &stem),
                            Scope::Full => {
                                let full = join(&stem, &ext);
                                let out = apply(&full, &stem);
                                (stem, ext) = split(&out, item.is_dir);
                            }
                        }
                    }
                }
            }
            work[idx] = (stem, ext);
        }
        if seg_end >= nrules {
            break;
        }
        if let RuleKind::Awk { script, fs, input, system } = &self.rules[seg_end].kind {
            let scope = self.rules[seg_end].scope;
            let on: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].on).collect();
            let lines: Vec<String> = on
                .iter()
                .map(|&i| {
                    let it = &self.items[i];
                    let (stem, ext) = &work[i];
                    let part = match scope {
                        Scope::Name => stem.clone(),
                        Scope::Ext => ext.clone(),
                        Scope::Full => join(stem, ext),
                    };
                    let line = if *input == 1 {
                        let (ostem, oext) = split(&it.name(), it.is_dir);
                        let folder = it.path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let size = it.meta.as_ref().map(|m| m.len()).unwrap_or(0);
                        let date = fmt_time(it.meta.as_ref().and_then(|m| m.modified().ok()), "%Y-%m-%d");
                        [part, ostem, oext, folder, size.to_string(), date, it.path.to_string_lossy().to_string()].map(|f| f.replace('\t', " ")).join("\t")
                    } else {
                        part
                    };
                    line.replace(['\n', '\r'], " ")
                })
                .collect();
            let fs = if *input == 1 { "\t".to_string() } else { fs.clone() };
            let result = if *system { run_awk(script, &fs, &lines) } else { run_builtin_awk(script, &fs, &lines) };
            match result {
                Ok(out) if out.len() == lines.len() => {
                    for (&i, new) in on.iter().zip(out) {
                        let is_dir = self.items[i].is_dir;
                        let (stem, ext) = &mut work[i];
                        match scope {
                            Scope::Name => *stem = new,
                            Scope::Ext => *ext = new,
                            Scope::Full => (*stem, *ext) = split(&new, is_dir),
                        }
                    }
                }
                Ok(out) => awk_errors.push((seg_end, trf!("awk devolvió {} líneas para {} archivos: cada archivo necesita exactamente una línea", out.len(), lines.len()))),
                Err(e) => awk_errors.push((seg_end, e)),
            }
        }
        seg_start = seg_end + 1;
        }
        for (ri, e) in awk_errors {
            self.rule_errors[ri] = Some(e);
        }
        for (idx, item) in self.items.iter_mut().enumerate() {
            let name = item.name();
            if item.on {
                item.new = join(&work[idx].0, &work[idx].1);
                item.status = if item.new == name { Status::Unchanged } else { Status::Ok };
            } else {
                item.new = name;
                item.status = Status::Unchanged;
            }
        }
        self.validate();
    }

    fn validate(&mut self) {
        let fold = |s: &str| if cfg!(target_os = "linux") { s.to_string() } else { s.to_lowercase() };
        // Originals that will move away free their names.
        let leaving: HashSet<String> = self
            .items
            .iter()
            .filter(|i| i.status == Status::Ok)
            .map(|i| fold(&i.path.to_string_lossy()))
            .collect();
        let mut taken: HashMap<String, usize> = HashMap::new();
        for it in &self.items {
            let target = it.path.with_file_name(&it.new);
            *taken.entry(fold(&target.to_string_lossy())).or_default() += 1;
        }
        let mut used: HashSet<String> = HashSet::new();
        for i in 0..self.items.len() {
            let it = &self.items[i];
            if it.status == Status::Unchanged {
                used.insert(fold(&it.path.to_string_lossy()));
                continue;
            }
            if let Some(why) = invalid_reason(&it.new) {
                self.items[i].status = Status::Invalid(why);
                continue;
            }
            let target = it.path.with_file_name(&it.new);
            let key = fold(&target.to_string_lossy());
            let same_file = fold(&it.path.to_string_lossy()) == key;
            let exists_outside = target.exists() && !same_file && !leaving.contains(&key);
            let repeated = taken.get(&key).copied().unwrap_or(0) > 1 || used.contains(&key);
            if exists_outside || repeated {
                if self.conflicts == 1 {
                    // Add " (2)", " (3)"… until the name is free.
                    let (stem, ext) = split(&it.new, it.is_dir);
                    let mut k = 2;
                    loop {
                        let cand = join(&format!("{stem} ({k})"), &ext);
                        let ck = fold(&it.path.with_file_name(&cand).to_string_lossy());
                        let busy = used.contains(&ck) || taken.contains_key(&ck) || (it.path.with_file_name(&cand).exists() && !leaving.contains(&ck));
                        if !busy {
                            used.insert(ck);
                            self.items[i].new = cand;
                            break;
                        }
                        k += 1;
                    }
                } else {
                    let why = if exists_outside { tr("ya existe en la carpeta") } else { tr("nombre repetido") };
                    self.items[i].status = Status::Conflict(why.to_string());
                }
            } else {
                used.insert(key);
            }
        }
    }

    fn signature(&self) -> String {
        let mut s = serde_json::to_string(&(&self.rules, &self.counter, self.conflicts)).unwrap_or_default();
        for it in &self.items {
            s.push(if it.on { '1' } else { '0' });
            s.push_str(&it.path.to_string_lossy());
        }
        s
    }

    // ---------------- applying ----------------

    fn apply(&mut self, events: &mut Vec<ToolEvent>) {
        let plan: Vec<(usize, PathBuf, PathBuf)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.status == Status::Ok)
            .map(|(k, i)| (k, i.path.clone(), i.path.with_file_name(&i.new)))
            .collect();
        let pairs: Vec<(PathBuf, PathBuf)> = plan.iter().map(|(_, a, b)| (a.clone(), b.clone())).collect();
        let (done, errs) = two_phase(&pairs);
        for (k, from, to) in &plan {
            if done.contains(&(from.clone(), to.clone())) {
                self.items[*k].path = to.clone();
                self.items[*k].meta = fs::metadata(to).ok();
            }
        }
        if !done.is_empty() {
            self.undo.push(done.iter().map(|(a, b)| (b.clone(), a.clone())).collect());
            let now: chrono::DateTime<chrono::Local> = SystemTime::now().into();
            self.history.push(Batch { when: now.format("%d/%m/%Y %H:%M:%S").to_string(), pairs: done.clone(), restored: false });
            save_history(&self.history);
        }
        self.problems = errs;
        self.status = Some((trf!("{} elemento(s) renombrados", done.len()), !self.problems.is_empty()));
        self.signature.clear();
        events.push(ToolEvent::Reload);
    }

    fn undo_last(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(batch) = self.undo.pop() else { return };
        if let Some(h) = self.history.iter_mut().rev().find(|h| !h.restored) {
            h.restored = true;
            save_history(&self.history);
        }
        let (done, errs) = two_phase(&batch);
        for (from, to) in &done {
            if let Some(it) = self.items.iter_mut().find(|i| &i.path == from) {
                it.path = to.clone();
                it.meta = fs::metadata(to).ok();
            }
        }
        self.problems = errs;
        self.status = Some((trf!("Deshecho: {} elemento(s) recuperan su nombre", done.len()), false));
        self.signature.clear();
        events.push(ToolEvent::Reload);
    }

    /// Puts back the names a past rename changed (newest batches first when several are restored).
    fn restore_batch(&mut self, idx: usize, events: &mut Vec<ToolEvent>) {
        let Some(b) = self.history.get(idx) else { return };
        let pairs: Vec<(PathBuf, PathBuf)> = b.pairs.iter().filter(|(_, new)| new.exists()).map(|(old, new)| (new.clone(), old.clone())).collect();
        let missing = b.pairs.len() - pairs.len();
        let (done, mut errs) = two_phase(&pairs);
        if missing > 0 {
            errs.push(trf!("{} archivo(s) ya no estaban donde se dejaron", missing));
        }
        for (from, to) in &done {
            if let Some(it) = self.items.iter_mut().find(|i| &i.path == from) {
                it.path = to.clone();
                it.meta = fs::metadata(to).ok();
            }
        }
        self.history[idx].restored = true;
        save_history(&self.history);
        self.undo.clear();
        self.problems = errs;
        self.status = Some((trf!("Restaurado: {} elemento(s) recuperan su nombre", done.len()), !self.problems.is_empty()));
        self.signature.clear();
        events.push(ToolEvent::Reload);
    }

    /// Gives every item in the list back the name it had when it was added.
    fn restore_originals(&mut self, events: &mut Vec<ToolEvent>) {
        let pairs: Vec<(PathBuf, PathBuf)> = self.items.iter().filter(|i| i.path != i.original && i.path.exists()).map(|i| (i.path.clone(), i.original.clone())).collect();
        let (done, errs) = two_phase(&pairs);
        for (from, to) in &done {
            if let Some(it) = self.items.iter_mut().find(|i| &i.path == from) {
                it.path = to.clone();
                it.meta = fs::metadata(to).ok();
            }
        }
        if !done.is_empty() {
            let now: chrono::DateTime<chrono::Local> = SystemTime::now().into();
            self.history.push(Batch { when: now.format("%d/%m/%Y %H:%M:%S").to_string(), pairs: done.clone(), restored: false });
            save_history(&self.history);
        }
        self.undo.clear();
        self.problems = errs;
        self.status = Some((trf!("Restaurado: {} elemento(s) recuperan su nombre", done.len()), !self.problems.is_empty()));
        self.signature.clear();
        events.push(ToolEvent::Reload);
    }

    // ---------------- presets ----------------

    fn presets_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("renombrado"))
    }

    fn preset_list() -> Vec<String> {
        let mut v: Vec<String> = Self::presets_dir()
            .and_then(|d| fs::read_dir(d).ok())
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_string)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    fn save_preset(&mut self) {
        let name = self.preset_name.trim().replace(['/', '\\'], "-");
        let Some(dir) = Self::presets_dir() else { return };
        let p = Preset { rules: self.rules.clone(), counter: self.counter.clone(), conflicts: self.conflicts };
        let r = fs::create_dir_all(&dir).and_then(|_| fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&p).unwrap_or_default()));
        self.status = Some(match r {
            Ok(()) => (trf!("Preajuste «{}» guardado", name), false),
            Err(e) => (trf!("No se pudo guardar: {}", e), true),
        });
    }

    fn load_preset(&mut self, name: &str) {
        let Some(dir) = Self::presets_dir() else { return };
        match fs::read_to_string(dir.join(format!("{name}.json"))).ok().and_then(|s| serde_json::from_str::<Preset>(&s).ok()) {
            Some(p) => {
                self.rules = p.rules;
                self.counter = p.counter;
                self.conflicts = p.conflicts;
                self.preset_name = name.to_string();
                self.status = Some((trf!("Preajuste «{}» cargado", name), false));
            }
            None => self.status = Some((tr("No se pudo leer el preajuste").to_string(), true)),
        }
    }

    fn load_builtin(&mut self, k: usize) {
        let mut all = builtin_presets();
        let (name, _, p) = all.swap_remove(k);
        self.rules = p.rules;
        self.counter = p.counter;
        self.conflicts = p.conflicts;
        self.status = Some((trf!("Preajuste «{}» cargado", tr(name)), false));
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            let n = self.add_paths(dropped);
            self.status = Some((trf!("{} elemento(s) añadidos", n), false));
        }
        let sig = self.signature();
        if sig != self.signature {
            self.recompute();
            self.signature = sig;
        }

        self.list_toolbar(ui, selection, cwd);
        // The browsers: a folder adds what is inside it (with the options of the toolbar); files are added as they are.
        let ctx = ui.ctx().clone();
        for dir in crate::roots::folder_window(&ctx, "rename_folder", &mut self.folder_browser, false) {
            let n = self.add_folder(&dir);
            self.status = Some((trf!("{} elemento(s) añadidos", n), false));
        }
        for p in crate::roots::picker_window(&ctx, "rename_files", &mut self.file_picker) {
            let n = if p.is_dir() { self.add_folder(&p) } else { self.add_paths([p]) };
            self.status = Some((trf!("{} elemento(s) añadidos", n), false));
        }
        ui.separator();
        // The rules and the preview fill the window's height (and scroll inside it).
        let h = (ui.available_height() - 52.0).clamp(260.0, 2000.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(470.0, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_min_size(egui::vec2(470.0, h));
                self.rules_panel(ui, events);
            });
            ui.add_space(10.0);
            ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), h), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_min_height(h);
                self.preview(ui);
            });
        });
        ui.separator();
        self.footer(ui, events);
    }

    fn list_toolbar(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        let row_label = |ui: &mut egui::Ui, t: &str| {
            crate::checksum::cell(ui, 110.0, 22.0, |ui| ui.label(RichText::new(t).small().strong().color(theme::p().sub)));
        };
        // 1. Adding: from the main window, a folder or chosen files.
        ui.horizontal_wrapped(|ui| {
            row_label(ui, tr("AÑADIR"));
            if ui
                .add_enabled(!selection.is_empty(), egui::Button::new(trf!("➕ Seleccionados ({})", selection.len())))
                .on_hover_text(tr("Selecciona archivos en la ventana principal (puedes cambiar de carpeta) y pulsa aquí para sumarlos a la lista."))
                .clicked()
            {
                let n = self.add_paths(selection.to_vec());
                self.status = Some((trf!("{} elemento(s) añadidos", n), false));
            }
            if ui.button(tr("➕ Carpeta actual")).on_hover_text(cwd.to_string_lossy()).clicked() {
                let n = self.add_folder(cwd);
                self.status = Some((trf!("{} elemento(s) añadidos", n), false));
            }
            let start = self.items.last().map(|i| i.path.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.to_path_buf())).unwrap_or_else(|| cwd.to_path_buf());
            if ui.button(tr("📂 Elegir carpeta…")).on_hover_text(tr("Examinar y añadir el contenido de una carpeta (con las opciones de al lado)")).clicked() {
                self.folder_browser = Some(crate::dupes::Browser::new(start.clone(), 0));
            }
            if ui.button(tr("📂 Elegir archivos…")).on_hover_text(tr("Examinar y marcar archivos o carpetas concretos")).clicked() {
                self.file_picker = Some(crate::roots::Picker::new(start));
            }
        });
        // 2. One filter for both jobs: it decides what a folder brings into the list, and the
        // buttons beside it mark which of the files already listed get renamed.
        let m = name_matcher(&self.add_mask.replace(',', ";"), self.add_regex);
        let ok = matches!(m, Ok(Some(_)));
        ui.horizontal_wrapped(|ui| {
            row_label(ui, tr("FILTRO"));
            ui.add(egui::TextEdit::singleline(&mut self.add_mask).hint_text(if self.add_regex { "\\.(jpe?g|png)$" } else { "*.jpg, *.png, foto*" }).desired_width(170.0).font(egui::TextStyle::Monospace))
                .on_hover_text(format!("{}\n{}", tr("Puedes poner varios separados por comas, por ejemplo: *.jpg, *.png, foto*"), tr("Al añadir una carpeta solo entra lo que coincide. Con «Solo estos» o «Excluir estos» se aplica también a lo que ya está en la lista.")));
            if !self.add_regex {
                ui.label(RichText::new(tr("varios: separados por comas")).small().color(theme::p().muted));
            }
            ui.checkbox(&mut self.add_regex, tr("Expresión regular")).on_hover_text(tr("El filtro es una expresión regular en vez de comodines"));
            ui.checkbox(&mut self.add_recursive, tr("Incluir subcarpetas"));
            ui.checkbox(&mut self.add_folders, tr("Añadir también las carpetas"));
        });
        // 3. The list: order it and choose which of its items get renamed.
        ui.horizontal_wrapped(|ui| {
            row_label(ui, tr("LISTA"));
            ui.menu_button(tr("Ordenar ⏷"), |ui| {
                let mut sort = |ui: &mut egui::Ui, label: &str, f: &dyn Fn(&Item, &Item) -> std::cmp::Ordering| {
                    if ui.button(label).clicked() {
                        self.items.sort_by(|a, b| f(a, b));
                        self.selected = None;
                        ui.close_menu();
                    }
                };
                sort(ui, tr("Por nombre (natural)"), &|a, b| natural_cmp(&a.name(), &b.name()));
                sort(ui, tr("Por fecha de modificación"), &|a, b| mtime(a).cmp(&mtime(b)));
                sort(ui, tr("Por tamaño"), &|a, b| size(a).cmp(&size(b)));
                sort(ui, tr("Por extensión"), &|a, b| split(&a.name(), a.is_dir).1.to_lowercase().cmp(&split(&b.name(), b.is_dir).1.to_lowercase()).then(natural_cmp(&a.name(), &b.name())));
                sort(ui, tr("Por carpeta"), &|a, b| a.path.cmp(&b.path));
                if ui.button(tr("Invertir orden")).clicked() {
                    self.items.reverse();
                    ui.close_menu();
                }
            });
            let sel = self.selected;
            if ui.add_enabled(sel.map(|s| s > 0).unwrap_or(false), egui::Button::new("⬆")).on_hover_text(tr("Subir el elemento elegido")).clicked() {
                let s = sel.unwrap();
                self.items.swap(s, s - 1);
                self.selected = Some(s - 1);
            }
            if ui.add_enabled(sel.map(|s| s + 1 < self.items.len()).unwrap_or(false), egui::Button::new("⬇")).on_hover_text(tr("Bajar el elemento elegido")).clicked() {
                let s = sel.unwrap();
                self.items.swap(s, s + 1);
                self.selected = Some(s + 1);
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(tr("Quitar"))).on_hover_text(tr("Quitar de la lista (no borra el archivo)")).clicked() {
                self.items.remove(sel.unwrap());
                self.selected = None;
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
                self.selected = None;
            }
        });
        ui.horizontal_wrapped(|ui| {
            row_label(ui, tr("RENOMBRAR"));
            // 0: only the matches, 1: unmark the matches, 2: everything but the matches.
            let mut set = |how: u8| {
                if let Ok(Some(re)) = &m {
                    for it in &mut self.items {
                        let hit = re.is_match(&it.name());
                        it.on = match how {
                            0 => hit,
                            1 => it.on && !hit,
                            _ => !hit,
                        };
                    }
                }
            };
            if ui.add_enabled(ok, egui::Button::new(tr("Solo estos"))).on_hover_text(tr("De la lista, renombra solo los que coinciden con el filtro")).clicked() {
                set(0);
            }
            if ui.add_enabled(ok, egui::Button::new(tr("Excluir estos"))).on_hover_text(tr("Desmarca los que coinciden con el filtro; el resto se queda como está")).clicked() {
                set(1);
            }
            if ui.add_enabled(ok, egui::Button::new(tr("Todos menos estos"))).on_hover_text(tr("Marca todos los de la lista excepto los que coinciden con el filtro")).clicked() {
                set(2);
            }
            if ui.button(tr("Marcar todos")).clicked() {
                for it in &mut self.items {
                    it.on = true;
                }
            }
            if ui.button(tr("Ninguno")).on_hover_text(tr("Desmarca todos: no se renombra nada hasta que marques alguno")).clicked() {
                for it in &mut self.items {
                    it.on = false;
                }
            }
            if let Err(e) = &m {
                ui.label(RichText::new(trf!("Expresión no válida: {}", e)).small().color(theme::p().danger));
            } else if ok {
                let n = self.items.iter().filter(|i| matches!(&m, Ok(Some(re)) if re.is_match(&i.name()))).count();
                ui.label(RichText::new(trf!("{} coinciden", n)).small().color(theme::p().sub));
            }
        });
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let hint = if hovering { tr("Suelta para añadir los archivos") } else { tr("También puedes arrastrar archivos y carpetas a esta ventana.") };
        ui.label(RichText::new(hint).small().color(if hovering { theme::accent() } else { theme::p().muted }));
    }

    fn rules_panel(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("REGLAS (se aplican en orden)")).small().strong().color(theme::p().sub));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(tr("✨ Rápidos"), |ui| {
                    ui.label(RichText::new(tr("Sustituyen las reglas actuales")).small().color(theme::p().muted));
                    for (k, (name, desc, _)) in builtin_presets().iter().enumerate() {
                        if ui.button(tr(name)).on_hover_text(tr(desc)).clicked() {
                            self.load_builtin(k);
                            ui.close_menu();
                        }
                    }
                })
                .response
                .on_hover_text(tr("Preajustes listos para usar"));
                ui.menu_button(tr("➕ Añadir regla"), |ui| {
                    ui.set_min_width(260.0);
                    let all = RuleKind::all_defaults();
                    // Grouped by what they do; the order inside each group is the most used first.
                    for (group, idx) in [("AÑADIR", &[3usize, 0, 1, 2, 5][..]), ("CAMBIAR", &[4, 7, 17, 12, 9][..]), ("QUITAR Y LIMPIAR", &[8, 6, 10, 13, 11][..]), ("AVANZADO", &[14, 15, 16][..])] {
                        ui.label(RichText::new(tr(group)).small().strong().color(theme::p().sub));
                        for &n in idx {
                            let k = &all[n];
                            if ui.button(format!("{}  {}", k.icon(), k.title())).on_hover_text(k.help()).clicked() {
                                self.rules.push(Rule { on: true, scope: Scope::Name, kind: k.clone() });
                                ui.close_menu();
                            }
                        }
                        ui.add_space(4.0);
                    }
                });
            });
        });
        egui::ScrollArea::vertical().id_salt("reglas").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
            let mut action: Option<(usize, i8)> = None; // -1 up, 1 down, 0 delete
            let n = self.rules.len();
            for i in 0..n {
                let err = self.rule_errors.get(i).cloned().flatten();
                let rule = &mut self.rules[i];
                egui::Frame::group(ui.style()).fill(theme::p().base).inner_margin(6).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let folded = self.folded.contains(&i);
                    ui.horizontal(|ui| {
                        if ui.small_button(if folded { "⏵" } else { "⏷" }).on_hover_text(if folded { tr("Mostrar las opciones") } else { tr("Plegar") }).clicked() {
                            action = Some((i, 3));
                        }
                        ui.checkbox(&mut rule.on, "").on_hover_text(tr("Activar o desactivar esta regla sin quitarla"));
                        let title = ui.add(egui::Label::new(RichText::new(format!("{}. {}  {}", i + 1, rule.kind.icon(), rule.kind.title())).strong()).sense(egui::Sense::click()));
                        if title.on_hover_text(rule.kind.help()).clicked() {
                            action = Some((i, 3));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("🗑").on_hover_text(tr("Quitar regla")).clicked() {
                                action = Some((i, 0));
                            }
                            if ui.small_button("🗐").on_hover_text(tr("Duplicar regla")).clicked() {
                                action = Some((i, 2));
                            }
                            if ui.add_enabled(i + 1 < n, egui::Button::new("⬇").small()).on_hover_text(tr("Bajar")).clicked() {
                                action = Some((i, 1));
                            }
                            if ui.add_enabled(i > 0, egui::Button::new("⬆").small()).on_hover_text(tr("Subir")).clicked() {
                                action = Some((i, -1));
                            }
                        });
                    });
                    if !folded {
                        ui.label(RichText::new(rule.kind.help()).small().italics().color(theme::p().muted));
                        ui.add_space(2.0);
                        ui.add_enabled_ui(rule.on, |ui| {
                            // Every rule that changes the name starts with the same «Aplicar a» line.
                            if rule.kind.uses_scope() {
                                field(ui, tr("Aplicar a"), |ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.selectable_value(&mut rule.scope, Scope::Name, tr("Nombre"));
                                    ui.selectable_value(&mut rule.scope, Scope::Ext, tr("Extensión"));
                                    ui.selectable_value(&mut rule.scope, Scope::Full, tr("Todo"));
                                })
                            }
                            rule_editor(ui, i, &mut rule.kind)
                        });
                    }
                    if let Some(e) = err {
                        let msg = if matches!(rule.kind, RuleKind::Awk { .. }) { trf!("awk: {}", e) } else { trf!("Expresión no válida: {}", e) };
                        ui.label(RichText::new(msg).small().color(theme::p().danger));
                    }
                });
                ui.add_space(2.0);
            }
            if let Some((i, d)) = action {
                // Folding follows the rules when they move.
                let shift = |set: &HashSet<usize>, f: &dyn Fn(usize) -> Option<usize>| set.iter().filter_map(|&k| f(k)).collect::<HashSet<usize>>();
                match d {
                    0 => {
                        self.rules.remove(i);
                        self.folded = shift(&self.folded, &|k| if k == i { None } else if k > i { Some(k - 1) } else { Some(k) });
                    }
                    2 => {
                        let copy = self.rules[i].clone();
                        self.rules.insert(i + 1, copy);
                        self.folded = shift(&self.folded, &|k| Some(if k > i { k + 1 } else { k }));
                    }
                    3 => {
                        if !self.folded.remove(&i) {
                            self.folded.insert(i);
                        }
                    }
                    -1 | 1 => {
                        let j = if d == -1 { i - 1 } else { i + 1 };
                        self.rules.swap(i, j);
                        self.folded = shift(&self.folded, &|k| Some(if k == i { j } else if k == j { i } else { k }));
                    }
                    _ => {}
                }
            }
            if self.rules.is_empty() {
                ui.label(RichText::new(tr("Añade una regla para empezar.")).color(theme::p().muted));
            }

            ui.add_space(6.0);
            ui.collapsing(tr("🔢 Contador ({n})"), |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr("Empieza en"));
                    ui.add(egui::DragValue::new(&mut self.counter.start));
                    ui.label(tr("paso"));
                    ui.add(egui::DragValue::new(&mut self.counter.step));
                    ui.label(tr("cifras"));
                    ui.add(egui::DragValue::new(&mut self.counter.pad).range(1..=10));
                });
                ui.checkbox(&mut self.counter.per_folder, tr("Reiniciar en cada carpeta"));
            });
            ui.collapsing(tr("ℹ Marcadores disponibles"), |ui| {
                ui.label(RichText::new(tr("Se usan en «Nuevo nombre», «Insertar texto» y en el reemplazo. Haz clic para copiar.")).small().color(theme::p().muted));
                egui::Grid::new("marcadores").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
                    for (code, desc) in PLACEHOLDERS {
                        if ui.link(RichText::new(*code).monospace()).clicked() {
                            events.push(ToolEvent::Clipboard(code.to_string()));
                        }
                        ui.label(RichText::new(tr(desc)).small());
                        ui.end_row();
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(tr("Con expresiones regulares, usa $1, $2… o ${nombre_grupo} en el reemplazo.")).small().color(theme::p().muted));
            });
            ui.collapsing(tr("⚙ Opciones avanzadas"), |ui| {
                ui.label(tr("Si dos nombres chocan o ya existen:"));
                ui.selectable_value(&mut self.conflicts, 0, tr("No renombrar esos elementos"));
                ui.selectable_value(&mut self.conflicts, 1, tr("Añadir (2), (3)… automáticamente"));
            });
            ui.collapsing(tr("💾 Preajustes"), |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text(tr("nombre del preajuste")).desired_width(160.0));
                    if ui.add_enabled(!self.preset_name.trim().is_empty(), egui::Button::new(tr("Guardar"))).clicked() {
                        self.save_preset();
                    }
                });
                ui.label(RichText::new(tr("Incluidos")).small().strong());
                for (k, (name, desc, _)) in builtin_presets().iter().enumerate() {
                    if ui.link(tr(name)).on_hover_text(tr(desc)).clicked() {
                        self.load_builtin(k);
                    }
                }
                ui.label(RichText::new(tr("Tuyos")).small().strong());
                let list = Self::preset_list();
                if list.is_empty() {
                    ui.label(RichText::new(tr("Aún no hay preajustes guardados.")).small().color(theme::p().muted));
                }
                for name in list {
                    ui.horizontal(|ui| {
                        if ui.link(&name).on_hover_text(tr("Cargar estas reglas")).clicked() {
                            self.load_preset(&name);
                        }
                        if ui.small_button("🗑").on_hover_text(tr("Borrar preajuste")).clicked() {
                            if let Some(d) = Self::presets_dir() {
                                let _ = fs::remove_file(d.join(format!("{name}.json")));
                            }
                        }
                    });
                }
            });
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let changes = self.items.iter().filter(|i| i.status == Status::Ok).count();
        let bad = self.items.iter().filter(|i| matches!(i.status, Status::Conflict(_) | Status::Invalid(_))).count();
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("VISTA PREVIA")).small().strong().color(theme::p().sub));
            ui.label(RichText::new(trf!("{} en la lista · {} cambian", self.items.len(), changes)).small());
            if bad > 0 {
                ui.label(RichText::new(trf!("· {} con problemas", bad)).small().color(theme::p().danger));
            }
        });
        let w = ui.available_width();
        // Three columns, one line per file. By default each name column fits the longest name but
        // never takes more than half the panel, so the old and new names sit side by side; the
        // dividers in the header drag to resize (double-click fits the names again).
        let longest = self.items.iter().map(|i| i.name().chars().count().max(i.new.chars().count())).max().unwrap_or(0);
        let status_w = 170.0;
        let fit = ((longest as f32) * 7.6 + 30.0).clamp(140.0, ((w - 30.0 - status_w) / 2.0).max(140.0));
        let [c0, c1] = self.cols.unwrap_or([fit, fit]);
        let list_h = (ui.available_height() - 24.0).max(120.0);
        let row_h = 24.0;
        let mut toggles = vec![];
        let mut clicked = None;
        let mut widths = [c0, c1];
        let mut reset = false;
        egui::ScrollArea::horizontal().id_salt("vista_ren_h").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(28.0);
                for (k, title) in [tr("Nombre actual"), tr("Nombre nuevo")].into_iter().enumerate() {
                    crate::checksum::cell(ui, widths[k] - 8.0, 18.0, |ui| ui.label(RichText::new(title).small().color(theme::p().sub)));
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(8.0, 18.0), egui::Sense::click_and_drag());
                    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text(tr("Arrastra para cambiar el ancho; doble clic para ajustarlo a los nombres"));
                    let c = if resp.hovered() || resp.dragged() { theme::accent() } else { theme::p().muted.gamma_multiply(0.5) };
                    ui.painter().vline(rect.center().x, rect.y_range(), egui::Stroke::new(1.5_f32, c));
                    if resp.dragged() {
                        widths[k] = (widths[k] + resp.drag_delta().x).clamp(80.0, 2000.0);
                    }
                    if resp.double_clicked() {
                        reset = true;
                    }
                }
                ui.label(RichText::new(tr("Estado")).small().color(theme::p().sub));
            });
            let [c0, c1] = widths;
            egui::ScrollArea::vertical().id_salt("vista_ren").max_height(list_h).auto_shrink([false, false]).show_rows(ui, row_h, self.items.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for i in range {
                    let it = &self.items[i];
                    let selected = self.selected == Some(i);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0 + c0 + c1 + status_w, row_h), egui::Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 4.0, theme::accent().gamma_multiply(0.25));
                    } else if i % 2 == 1 {
                        ui.painter().rect_filled(rect, 0.0, theme::p().stripe);
                    }
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                    let mut on = it.on;
                    if child.checkbox(&mut on, "").changed() {
                        toggles.push(i);
                    }
                    let icon = if it.is_dir { "📁 " } else { "" };
                    let old_col = if it.on { theme::p().text } else { theme::p().muted };
                    let (new_col, st) = match &it.status {
                        Status::Unchanged => (theme::p().muted, if it.on { tr("sin cambios").to_string() } else { tr("excluido").to_string() }),
                        Status::Ok => (theme::p().ok, "✔".to_string()),
                        Status::Conflict(w) => (theme::p().danger, format!("⚠ {w}")),
                        Status::Invalid(w) => (theme::p().danger, format!("⛔ {w}")),
                    };
                    crate::checksum::cell(&mut child, c0 - 4.0, row_h, |ui| ui.add(egui::Label::new(RichText::new(format!("{icon}{}", it.name())).color(old_col)).truncate()).on_hover_text(it.path.to_string_lossy()));
                    crate::checksum::cell(&mut child, c1, row_h, |ui| ui.add(egui::Label::new(RichText::new(&it.new).color(new_col)).truncate()));
                    child.add(egui::Label::new(RichText::new(st).small().color(new_col)).truncate());
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
        if reset {
            self.cols = None;
        } else if widths != [c0, c1] {
            self.cols = Some(widths);
        }
        for i in toggles {
            self.items[i].on = !self.items[i].on;
        }
        if let Some(i) = clicked {
            self.selected = if self.selected == Some(i) { None } else { Some(i) };
        }
        if self.items.is_empty() {
            ui.label(RichText::new(tr("La lista está vacía: añade archivos con los botones de arriba o arrastrándolos.")).color(theme::p().muted));
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>) {
        let changes = self.items.iter().filter(|i| i.status == Status::Ok).count();
        let regex_err = self.rule_errors.iter().any(|e| e.is_some());
        ui.horizontal(|ui| {
            let label = trf!("✏ Renombrar {} elemento(s)", changes);
            let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(changes > 0 && !regex_err, btn).clicked() {
                self.apply(events);
            }
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new(tr("Deshacer"))).on_hover_text(tr("Devuelve el nombre anterior a lo último que se renombró")).clicked() {
                self.undo_last(events);
            }
            let changed = self.items.iter().filter(|i| i.path != i.original).count();
            if ui
                .add_enabled(changed > 0, egui::Button::new(trf!("Restaurar originales ({})", changed)))
                .on_hover_text(tr("Devuelve a cada archivo de la lista el nombre que tenía cuando lo añadiste, aunque hayas renombrado varias veces"))
                .clicked()
            {
                self.restore_originals(events);
            }
            let mut restore = None;
            ui.menu_button(trf!("Historial ({})", self.history.len()), |ui| {
                ui.label(RichText::new(tr("Se guarda aunque cierres el programa. Restaurar devuelve los nombres de antes de ese renombrado.")).small().color(theme::p().muted));
                ui.separator();
                if self.history.is_empty() {
                    ui.label(tr("Todavía no se ha renombrado nada."));
                }
                egui::ScrollArea::vertical().max_height(crate::tools::grow(ui, 320.0)).show(ui, |ui| {
                    for (k, b) in self.history.iter().enumerate().rev() {
                        ui.horizontal(|ui| {
                            let first = b.pairs.first().map(|(o, n)| format!("{} » {}", o.file_name().unwrap_or_default().to_string_lossy(), n.file_name().unwrap_or_default().to_string_lossy())).unwrap_or_default();
                            ui.vertical(|ui| {
                                ui.label(RichText::new(trf!("{} · {} elemento(s)", b.when, b.pairs.len())).strong());
                                ui.label(RichText::new(first).small().color(theme::p().sub));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if b.restored {
                                    ui.label(RichText::new(tr("restaurado")).small().color(theme::p().muted));
                                } else if ui.button(tr("Restaurar")).clicked() {
                                    restore = Some(k);
                                    ui.close_menu();
                                }
                            });
                        });
                        ui.separator();
                    }
                });
                if !self.history.is_empty() && ui.small_button(tr("Borrar historial")).clicked() {
                    self.history.clear();
                    save_history(&self.history);
                }
            });
            if let Some(k) = restore {
                self.restore_batch(k, events);
            }
            if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).color(if *err { theme::p().danger } else { theme::p().ok }));
            }
        });
        if !self.problems.is_empty() {
            ui.collapsing(trf!("⚠ {} problema(s)", self.problems.len()), |ui| {
                for p in &self.problems {
                    ui.label(RichText::new(p).small().color(theme::p().danger));
                }
            });
        }
        let _ = &self.base;
    }
}

// ---- the same layout for every rule: a label column on the left, its controls on the right ----

use crate::ui_kit::{bool_choice, choice, field, note, LABEL_W};

/// A text box for text with placeholders, and buttons that add the usual ones.
fn text_with_codes(ui: &mut egui::Ui, text: &mut String, hint: &str, codes: &[&str]) {
    field(ui, tr("Texto"), |ui| {
        ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
    });
    ui.horizontal_wrapped(|ui| {
        crate::checksum::cell(ui, LABEL_W, 18.0, |ui| ui.label(RichText::new(tr("insertar")).small().color(theme::p().muted)));
        ui.spacing_mut().item_spacing.x = 4.0;
        for code in codes {
            if ui.small_button(code.trim()).on_hover_text(tr("Añadir al final")).clicked() {
                text.push_str(code);
            }
        }
    });
}

/// Where a counter or a total goes: start, end or a position, with its separator.
fn placement_ui(ui: &mut egui::Ui, pos: &mut u8, at: &mut usize, sep: &mut String) {
    field(ui, tr("Dónde"), |ui| {
        choice(ui, pos, &["Al principio", "Al final", "En la posición"]);
        if *pos == 2 {
            ui.add(egui::DragValue::new(at).range(0..=255));
        }
    });
    if *pos != 2 {
        field(ui, tr("Separador"), |ui| {
            ui.add(egui::TextEdit::singleline(sep).desired_width(60.0).font(egui::TextStyle::Monospace));
        });
    }
}

fn rule_editor(ui: &mut egui::Ui, i: usize, kind: &mut RuleKind) {
    match kind {
        RuleKind::Template { pattern } => {
            text_with_codes(ui, pattern, "{nombre} - {n}", &["{nombre}", "{n}", "{fecha}", "{carpeta}", "{ext}"]);
        }
        RuleKind::Prefix { text } | RuleKind::Suffix { text } => {
            text_with_codes(ui, text, tr("texto o marcadores"), &["{fecha} ", "{carpeta} - ", "{n} ", "{captura} "]);
        }
        RuleKind::Replace { find, with, regex, wildcard, case_sensitive, all } => {
            let mut mode: u8 = if *regex { 2 } else if *wildcard { 1 } else { 0 };
            field(ui, tr("Tipo"), |ui| choice(ui, &mut mode, &["Texto", "Comodines * ?", "Expresión regular"]));
            *regex = mode == 2;
            *wildcard = mode == 1;
            let (hf, hw) = match mode {
                1 => ("IMG_*", "Foto *"),
                2 => ("(\\d+)_(.*)", "$2 - $1"),
                _ => ("", ""),
            };
            for (label, text, hint) in [(tr("Buscar"), &mut *find, hf), (tr("Reemplazar por"), &mut *with, hw)] {
                field(ui, label, |ui| {
                    ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace).hint_text(hint));
                });
            }
            field(ui, tr("Opciones"), |ui| {
                ui.checkbox(case_sensitive, tr("Distinguir mayúsculas"));
                if mode != 1 {
                    ui.checkbox(all, tr("Todas las veces"));
                }
            });
            match mode {
                1 => note(ui, tr("* es cualquier texto y ? un carácter; el patrón abarca el nombre entero. En el reemplazo, cada * o ? pone lo que encontró su comodín: «IMG_*» » «Foto *».")),
                2 => {
                    note(ui, tr("Grupos con ( ) y en el reemplazo $1, $2… o ${nombre}."));
                    ui.collapsing(tr("Ejemplos"), |ui| {
                        for (f, w, d) in REGEX_EXAMPLES {
                            if ui.link(RichText::new(format!("{f}  »  {w}")).monospace().small()).on_hover_text(tr("Usar este ejemplo")).clicked() {
                                *find = f.to_string();
                                *with = w.to_string();
                            }
                            note(ui, tr(d));
                        }
                    });
                }
                _ => {}
            }
        }
        RuleKind::Case { mode } => {
            field(ui, tr("Convertir a"), |ui| choice(ui, mode, &CASE_MODES));
        }
        RuleKind::Insert { text, pos, from_end } => {
            field(ui, tr("Texto"), |ui| {
                ui.add(egui::TextEdit::singleline(text).hint_text(tr("texto o marcadores")).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
            });
            field(ui, tr("Posición"), |ui| {
                ui.add(egui::DragValue::new(pos).range(0..=999));
                bool_choice(ui, from_end, "desde el principio", "desde el final");
            });
        }
        RuleKind::Remove { from, count, from_end } => {
            field(ui, tr("Cuántos"), |ui| {
                ui.add(egui::DragValue::new(count).range(0..=999).suffix(tr(" caracteres")));
            });
            field(ui, tr("Desde"), |ui| {
                ui.add(egui::DragValue::new(from).range(0..=999));
                bool_choice(ui, from_end, "contando desde el principio", "contando desde el final");
            });
        }
        RuleKind::Number { suffix, sep } => {
            field(ui, tr("Dónde"), |ui| bool_choice(ui, suffix, "Delante", "Detrás"));
            field(ui, tr("Separador"), |ui| {
                ui.add(egui::TextEdit::singleline(sep).desired_width(60.0).font(egui::TextStyle::Monospace));
            });
            note(ui, tr("Inicio, paso y cifras se ajustan en «Contador»."));
        }
        RuleKind::Clean { trim, collapse, spaces, accents, special } => {
            field(ui, tr("Espacios"), |ui| {
                ui.checkbox(trim, tr("Quitar al inicio y final"));
                ui.checkbox(collapse, tr("Unir repetidos"));
            });
            field(ui, tr("Cambiar"), |ui| {
                egui::ComboBox::from_id_salt(("espacios", i)).width(170.0).selected_text(tr(SPACE_MODES[*spaces as usize])).show_ui(ui, |ui| {
                    for (k, l) in SPACE_MODES.iter().enumerate() {
                        ui.selectable_value(spaces, k as u8, tr(l));
                    }
                });
            });
            field(ui, tr("Quitar"), |ui| {
                ui.checkbox(accents, tr("Tildes (á » a)"));
                ui.checkbox(special, tr("Símbolos raros"));
            });
        }
        RuleKind::Ext { mode, text } => {
            field(ui, tr("Extensión"), |ui| choice(ui, mode, &EXT_MODES));
            if *mode == 2 || *mode == 4 {
                field(ui, tr("Nueva"), |ui| {
                    ui.add(egui::TextEdit::singleline(text).hint_text("jpg").desired_width(100.0).font(egui::TextStyle::Monospace));
                });
            }
        }
        RuleKind::PadNumbers { digits } => {
            field(ui, tr("Cifras"), |ui| {
                ui.add(egui::DragValue::new(digits).range(1..=10));
                note(ui, tr("como mínimo en cada número"));
            });
        }
        RuleKind::RemoveBetween { open, close } => {
            field(ui, tr("Desde / hasta"), |ui| {
                ui.add(egui::TextEdit::singleline(open).desired_width(36.0).font(egui::TextStyle::Monospace));
                ui.add(egui::TextEdit::singleline(close).desired_width(36.0).font(egui::TextStyle::Monospace));
                ui.spacing_mut().item_spacing.x = 4.0;
                for (a, b) in [("(", ")"), ("[", "]"), ("{", "}")] {
                    if ui.small_button(format!("{a} {b}")).clicked() {
                        *open = a.into();
                        *close = b.into();
                    }
                }
            });
        }
        RuleKind::Truncate { max, from_end } => {
            field(ui, tr("Como mucho"), |ui| {
                ui.add(egui::DragValue::new(max).range(1..=255).suffix(tr(" caracteres")));
            });
            field(ui, tr("Conservar"), |ui| bool_choice(ui, from_end, "el principio", "el final"));
        }
        RuleKind::Swap { sep, joiner } => {
            field(ui, tr("Separar por"), |ui| {
                ui.add(egui::TextEdit::singleline(sep).desired_width(60.0).font(egui::TextStyle::Monospace));
            });
            field(ui, tr("Unir con"), |ui| {
                ui.add(egui::TextEdit::singleline(joiner).desired_width(60.0).font(egui::TextStyle::Monospace));
            });
        }
        RuleKind::Strip { mode } => {
            field(ui, tr("Quitar"), |ui| {
                egui::ComboBox::from_id_salt(("restos", i)).width(260.0).selected_text(tr(STRIP_MODES[*mode as usize])).show_ui(ui, |ui| {
                    for (k, l) in STRIP_MODES.iter().enumerate() {
                        ui.selectable_value(mode, k as u8, tr(l));
                    }
                });
            });
        }
        RuleKind::Counter { start, step, pad, style, pos, at, sep, reset, group } => {
            field(ui, tr("Números"), |ui| {
                ui.label(tr("desde"));
                ui.add(egui::DragValue::new(start));
                ui.label(tr("paso"));
                ui.add(egui::DragValue::new(step));
                ui.label(tr("cifras"));
                ui.add(egui::DragValue::new(pad).range(1..=10));
            });
            field(ui, tr("Estilo"), |ui| {
                egui::ComboBox::from_id_salt(("estilo", i)).width(150.0).selected_text(tr(COUNTER_STYLES[*style as usize])).show_ui(ui, |ui| {
                    for (k, l) in COUNTER_STYLES.iter().enumerate() {
                        ui.selectable_value(style, k as u8, tr(l));
                    }
                });
            });
            placement_ui(ui, pos, at, sep);
            field(ui, tr("Reiniciar"), |ui| choice(ui, reset, &["nunca", "en cada carpeta", "cuando cambia"]));
            if *reset == 2 {
                field(ui, tr("Cambia"), |ui| {
                    ui.add(egui::TextEdit::singleline(group).hint_text("{carpeta}").desired_width(f32::INFINITY).font(egui::TextStyle::Monospace))
                        .on_hover_text(tr("Vuelve a empezar cada vez que este texto cambia, p. ej. {captura} para numerar las fotos de cada día o {album} para cada disco. Ordena la lista antes."));
                });
            }
        }
        RuleKind::Accumulate { what, inclusive, pos, at, sep } => {
            field(ui, tr("Qué sumar"), |ui| {
                egui::ComboBox::from_id_salt(("acumulado", i)).width(260.0).selected_text(tr(ACCUM_KINDS[*what as usize])).show_ui(ui, |ui| {
                    for (k, l) in ACCUM_KINDS.iter().enumerate() {
                        ui.selectable_value(what, k as u8, tr(l));
                    }
                });
            });
            field(ui, tr("Contar"), |ui| bool_choice(ui, inclusive, "lo anterior", "hasta él incluido"))
                ;
            placement_ui(ui, pos, at, sep);
        }
        RuleKind::Variable { name, value, mode, sep } => {
            field(ui, tr("Variable"), |ui| {
                ui.add(egui::TextEdit::singleline(name).desired_width(80.0).font(egui::TextStyle::Monospace));
            });
            field(ui, tr("Valor"), |ui| {
                ui.add(egui::TextEdit::singleline(value).hint_text("{nombre:1:3}").desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
            });
            field(ui, tr("Modo"), |ui| choice(ui, mode, &["Asignar", "Acumular texto", "Sumar números"]));
            if *mode == 1 {
                field(ui, tr("Separador"), |ui| {
                    ui.add(egui::TextEdit::singleline(sep).desired_width(60.0).font(egui::TextStyle::Monospace));
                });
            }
            note(ui, &trf!("Úsala en reglas posteriores con {}.", format!("{{var:{}}}", name.trim())));
        }
        RuleKind::Awk { script, fs, input, system } => {
            ui.add(egui::TextEdit::multiline(script).font(egui::TextStyle::Monospace).desired_rows(3).desired_width(f32::INFINITY).hint_text("{ print toupper($0) }"));
            field(ui, tr("Entrada"), |ui| {
                choice(ui, input, &["Solo el nombre", "Nombre y datos"]);
            });
            if *input == 0 {
                field(ui, tr("Separador -F"), |ui| {
                    ui.add(egui::TextEdit::singleline(fs).desired_width(60.0).font(egui::TextStyle::Monospace)).on_hover_text(tr("Vacío: espacios, como awk por defecto"));
                });
            } else {
                note(ui, tr("Campos separados por tabulador: $1 nombre actual, $2 nombre original, $3 extensión original, $4 carpeta, $5 tamaño en bytes, $6 fecha de modificación (AAAA-MM-DD), $7 ruta completa."));
            }
            field(ui, tr("Motor"), |ui| {
                let found = awk_program().is_some();
                if !found {
                    *system = false;
                }
                ui.add_enabled(found, egui::Checkbox::new(system, tr("Usar el awk instalado en el sistema")))
                    .on_hover_text(tr("Útil para extensiones de gawk. Si no, se usa el awk incluido en el programa, que funciona igual en Windows, Mac y Linux sin instalar nada."))
                    .on_disabled_hover_text(tr("No hay ningún awk instalado; se usa el incluido en el programa."));
            });
            ui.collapsing(tr("Ejemplos"), |ui| {
                for (sc, mode, d) in AWK_EXAMPLES {
                    if ui.link(RichText::new(*sc).monospace().small()).on_hover_text(tr("Usar este ejemplo")).clicked() {
                        *script = sc.to_string();
                        *input = *mode;
                        fs.clear();
                    }
                    note(ui, tr(d));
                }
            });
        }
    }
}

const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{nombre}", "Nombre original sin extensión"),
    ("{nombre:1:5}", "Parte del nombre: desde el carácter 1, 5 caracteres (-3 cuenta desde el final)"),
    ("{actual}", "Nombre tal como va tras las reglas anteriores"),
    ("{ext}", "Extensión original"),
    ("{n}", "Contador (ver «Contador»); {n:4} con 4 cifras"),
    ("{carpeta}", "Nombre de la carpeta que lo contiene"),
    ("{fecha}", "Fecha de modificación (2024-05-31)"),
    ("{fecha:%d-%m-%Y %Hh%M}", "Fecha con el formato que quieras"),
    ("{creado}", "Fecha de creación (si el sistema la guarda)"),
    ("{hoy}", "Fecha de hoy (admite formato como {fecha})"),
    ("{tamaño}", "Tamaño legible (2.4 MB)"),
    ("{bytes}", "Tamaño en bytes"),
    ("{total}", "Cuántos elementos se renombran"),
    ("{aleatorio:6}", "Letras y números al azar"),
    ("{captura}", "Fotos: fecha en que se hizo (EXIF); admite formato"),
    ("{camara}", "Fotos: modelo de cámara o móvil"),
    ("{ancho}  {alto}", "Imágenes: tamaño en píxeles"),
    ("{artista}", "Música: artista"),
    ("{album}", "Música: álbum"),
    ("{titulo}", "Música: título de la canción"),
    ("{pista}", "Música: número de pista (2 cifras); {pista:3}"),
    ("{año}", "Música: año"),
    ("{genero}", "Música: género"),
    ("{duracion}", "Música y vídeo: duración (03m12s)"),
    ("{var:x}", "Valor guardado con la regla «Guardar en variable»"),
];

#[derive(Clone, Serialize, Deserialize)]
struct Batch {
    when: String,
    /// (name before, name after)
    pairs: Vec<(PathBuf, PathBuf)>,
    restored: bool,
}

fn history_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("utilidades-de-archivos").join("historial-renombrado.json"))
}

fn load_history() -> Vec<Batch> {
    if cfg!(test) {
        return vec![];
    }
    history_file().and_then(|f| fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_history(h: &[Batch]) {
    if cfg!(test) {
        return;
    }
    let Some(f) = history_file() else { return };
    if let Some(d) = f.parent() {
        let _ = fs::create_dir_all(d);
    }
    let keep = &h[h.len().saturating_sub(100)..];
    let _ = fs::write(f, serde_json::to_string(keep).unwrap_or_default());
}

fn builtin_presets() -> Vec<(&'static str, &'static str, Preset)> {
    let r = |kind: RuleKind| Rule { on: true, scope: Scope::Name, kind };
    let counter = Counter { start: 1, step: 1, pad: 2, per_folder: false };
    let p = |rules: Vec<Rule>, conflicts: u8| Preset { rules, counter: counter.clone(), conflicts };
    let tidy = || RuleKind::Clean { trim: true, collapse: true, spaces: 0, accents: false, special: false };
    let lower_ext = || RuleKind::Ext { mode: 0, text: String::new() };
    vec![
        ("📷 Fotos por fecha de captura", "2024-05-31_183005.jpg, con la fecha en que se hizo la foto (o la de modificación si no la tiene).", p(vec![r(RuleKind::Template { pattern: "{captura:%Y-%m-%d_%H%M%S}".into() }), r(lower_ext())], 1)),
        ("📷 Fotos: fecha y cámara", "2024-05-31 Pixel 8 01.jpg", p(vec![r(RuleKind::Template { pattern: "{captura} {camara} {n}".into() }), r(tidy()), r(lower_ext())], 1)),
        ("🎵 Música: pista - artista - título", "03 - Artista - Canción.mp3, con las etiquetas del archivo.", p(vec![r(RuleKind::Template { pattern: "{pista} - {artista} - {titulo}".into() }), r(tidy())], 1)),
        ("🎵 Música: artista - título", "Artista - Canción.mp3", p(vec![r(RuleKind::Template { pattern: "{artista} - {titulo}".into() }), r(tidy())], 1)),
        ("🌐 Nombres seguros para web", "sin tildes ni símbolos, en minúsculas y con guiones: mi-foto-de-verano.jpg", p(vec![r(RuleKind::Clean { trim: true, collapse: true, spaces: 2, accents: true, special: true }), r(RuleKind::Case { mode: 0 }), r(lower_ext())], 1)),
        ("# Numerar con el nombre de la carpeta", "Vacaciones_01.jpg, Vacaciones_02.jpg…", p(vec![r(RuleKind::Template { pattern: "{carpeta}_{n}".into() })], 0)),
        ("# Numerar manteniendo el nombre", "01 - nombre original.ext", p(vec![r(RuleKind::Counter { start: 1, step: 1, pad: 2, style: 0, pos: 0, at: 0, sep: " - ".into(), reset: 0, group: String::new() })], 0)),
        ("# Numerar las fotos de cada día", "2024-05-31 01.jpg, 2024-05-31 02.jpg, 2024-06-01 01.jpg…", p(vec![r(RuleKind::Template { pattern: "{captura}".into() }), r(RuleKind::Counter { start: 1, step: 1, pad: 2, style: 0, pos: 1, at: 0, sep: " ".into(), reset: 2, group: "{captura}".into() }), r(lower_ext())], 1)),
        ("∑ Mezcla: minuto de inicio de cada pista", "00m00s - pista1.mp3, 03m12s - pista2.mp3…", p(vec![r(RuleKind::Accumulate { what: 1, inclusive: false, pos: 0, at: 0, sep: " - ".into() })], 0)),
        ("📅 Fecha de modificación delante", "2024-05-31 - nombre original.ext", p(vec![r(RuleKind::Template { pattern: "{fecha} - {actual}".into() })], 1)),
        ("0 Rellenar números para ordenar bien", "foto 2 » foto 002, para que 10 no vaya antes que 2.", p(vec![r(RuleKind::PadNumbers { digits: 3 })], 0)),
        ("🗑 Quitar marcas de copia", "informe (1).pdf, informe - copia.pdf » informe.pdf", p(vec![r(RuleKind::Strip { mode: 2 })], 1)),
        ("🗑 Quitar números del principio", "01 - canción.mp3 » canción.mp3", p(vec![r(RuleKind::Strip { mode: 0 })], 1)),
        ("( ) Quitar texto entre paréntesis y corchetes", "video (1080p) [final].mp4 » video.mp4", p(vec![r(RuleKind::RemoveBetween { open: "(".into(), close: ")".into() }), r(RuleKind::RemoveBetween { open: "[".into(), close: "]".into() })], 1)),
        ("Aa Guiones bajos a espacios y tipo título", "mi_archivo_de_texto.txt » Mi Archivo De Texto.txt", p(vec![r(RuleKind::Replace { find: "_".into(), with: " ".into(), regex: false, wildcard: false, case_sensitive: false, all: true }), r(tidy()), r(RuleKind::Case { mode: 2 })], 0)),
        ("📐 Añadir resolución a imágenes", "foto.jpg » foto (4000x3000).jpg", p(vec![r(RuleKind::Template { pattern: "{actual} ({ancho}x{alto})".into() })], 0)),
        ("🔤 Extensiones en minúsculas", "FOTO.JPG » FOTO.jpg", p(vec![r(lower_ext())], 0)),
    ]
}

#[derive(Clone)]
struct Ctx<'a> {
    n: i64,
    pad: usize,
    path: &'a Path,
    stem: &'a str,
    ext: &'a str,
    meta: Option<&'a fs::Metadata>,
    total: usize,
    now: SystemTime,
    seed: u64,
    tags: &'a OnceCell<Tags>,
    vars: &'a std::cell::RefCell<HashMap<String, String>>,
}

impl Ctx<'_> {
    fn tags(&self) -> &Tags {
        self.tags.get_or_init(|| read_tags(self.path))
    }
    fn number(&self, pad: Option<usize>) -> String {
        let pad = pad.unwrap_or(self.pad);
        if self.n < 0 {
            format!("-{:0pad$}", -self.n)
        } else {
            format!("{:0pad$}", self.n)
        }
    }
}

fn fmt_time(t: Option<SystemTime>, fmt: &str) -> String {
    let Some(t) = t else { return String::new() };
    let dt: chrono::DateTime<chrono::Local> = t.into();
    let fmt = if fmt.is_empty() { "%Y-%m-%d" } else { fmt };
    let mut s = String::new();
    if write!(s, "{}", dt.format(fmt)).is_err() {
        return "?".into();
    }
    s
}

fn substring(s: &str, args: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut it = args.split(':');
    let start: i64 = it.next().and_then(|a| a.trim().parse().ok()).unwrap_or(1);
    let len: Option<usize> = it.next().and_then(|a| a.trim().parse().ok());
    let n = chars.len() as i64;
    let from = if start < 0 { (n + start).max(0) } else { (start - 1).clamp(0, n) } as usize;
    let to = len.map(|l| (from + l).min(chars.len())).unwrap_or(chars.len());
    chars[from..to.max(from)].iter().collect()
}

/// Replaces `{placeholders}`; unknown ones stay as written.
fn expand(pattern: &str, ctx: &Ctx, cur: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let token = &after[..close];
        let (name, args) = match token.split_once(':') {
            Some((n, a)) => (n.trim().to_lowercase(), Some(a)),
            None => (token.trim().to_lowercase(), None),
        };
        let value = match name.as_str() {
            "nombre" | "name" => Some(args.map(|a| substring(ctx.stem, a)).unwrap_or_else(|| ctx.stem.to_string())),
            "actual" | "current" => Some(args.map(|a| substring(cur, a)).unwrap_or_else(|| cur.to_string())),
            "ext" => Some(ctx.ext.to_string()),
            "n" | "num" => Some(ctx.number(args.and_then(|a| a.trim().parse().ok()))),
            "carpeta" | "folder" => Some(ctx.path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            "fecha" | "date" => Some(fmt_time(ctx.meta.and_then(|m| m.modified().ok()), args.unwrap_or(""))),
            "creado" | "created" => Some(fmt_time(ctx.meta.and_then(|m| m.created().ok()).or_else(|| ctx.meta.and_then(|m| m.modified().ok())), args.unwrap_or(""))),
            "hoy" | "today" => Some(fmt_time(Some(ctx.now), args.unwrap_or(""))),
            "tamaño" | "tamano" | "size" => Some(ctx.meta.map(|m| human_size(m.len())).unwrap_or_default()),
            "bytes" => Some(ctx.meta.map(|m| m.len().to_string()).unwrap_or_default()),
            "total" => Some(ctx.total.to_string()),
            "captura" | "taken" => Some(match self_taken(ctx) {
                Some(dt) => fmt_naive(dt, args.unwrap_or("")),
                None => fmt_time(ctx.meta.and_then(|m| m.modified().ok()), args.unwrap_or("")),
            }),
            "camara" | "cámara" | "camera" => Some(safe(ctx.tags().camera.clone())),
            "ancho" | "width" => Some(ctx.tags().size.map(|s| s.0.to_string()).unwrap_or_default()),
            "alto" | "height" => Some(ctx.tags().size.map(|s| s.1.to_string()).unwrap_or_default()),
            "artista" | "artist" => Some(safe(ctx.tags().artist.clone())),
            "album" | "álbum" => Some(safe(ctx.tags().album.clone())),
            "titulo" | "título" | "title" => Some(safe(ctx.tags().title.clone())),
            "genero" | "género" | "genre" => Some(safe(ctx.tags().genre.clone())),
            "año" | "ano" | "year" => Some(ctx.tags().year.map(|y| y.to_string()).unwrap_or_default()),
            "pista" | "track" => {
                let pad = args.and_then(|a| a.trim().parse().ok()).unwrap_or(2usize);
                Some(ctx.tags().track.map(|t| format!("{t:0pad$}")).unwrap_or_default())
            }
            "var" => Some(args.map(|a| ctx.vars.borrow().get(&a.trim().to_lowercase()).cloned().unwrap_or_default()).unwrap_or_default()),
            "duracion" | "duración" | "duration" => Some(ctx.tags().duration.map(fmt_duration).unwrap_or_default()),
            "aleatorio" | "random" => {
                let len = args.and_then(|a| a.trim().parse().ok()).unwrap_or(6usize).min(64);
                let mut state = ctx.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03 ^ (ctx.stem.len() as u64) << 32;
                const ABC: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                Some(
                    (0..len)
                        .map(|_| {
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            ABC[(state % ABC.len() as u64) as usize] as char
                        })
                        .collect(),
                )
            }
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn apply_rule(kind: &RuleKind, re: Option<&Regex>, s: &str, ctx: &Ctx, cur: &str) -> String {
    match kind {
        RuleKind::Template { pattern } => expand(pattern, ctx, cur),
        RuleKind::Replace { regex, wildcard, all, with, .. } => {
            let Some(re) = re else { return s.to_string() };
            let rep = expand(with, ctx, cur);
            let rep = if *wildcard && !*regex { wildcard_replacement(&rep) } else { rep };
            match (*regex || *wildcard, *all) {
                (true, true) => re.replace_all(s, rep.as_str()).into_owned(),
                (true, false) => re.replace(s, rep.as_str()).into_owned(),
                (false, true) => re.replace_all(s, NoExpand(&rep)).into_owned(),
                (false, false) => re.replace(s, NoExpand(&rep)).into_owned(),
            }
        }
        RuleKind::Case { mode } => match mode {
            0 => s.to_lowercase(),
            1 => s.to_uppercase(),
            2 => title_case(s),
            3 => {
                let lower = s.to_lowercase();
                let mut c = lower.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().chain(c).collect(),
                    None => String::new(),
                }
            }
            _ => s.chars().map(|c| if c.is_uppercase() { c.to_lowercase().collect::<String>() } else { c.to_uppercase().collect() }).collect(),
        },
        RuleKind::Insert { text, pos, from_end } => {
            let chars: Vec<char> = s.chars().collect();
            let at = if *from_end { chars.len().saturating_sub(*pos) } else { (*pos).min(chars.len()) };
            let ins = expand(text, ctx, cur);
            chars[..at].iter().collect::<String>() + &ins + &chars[at..].iter().collect::<String>()
        }
        RuleKind::Remove { from, count, from_end } => {
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len();
            let (a, b) = if *from_end {
                let end = n.saturating_sub(*from);
                (end.saturating_sub(*count), end)
            } else {
                let a = (*from).min(n);
                (a, (a + count).min(n))
            };
            chars[..a].iter().chain(chars[b..].iter()).collect()
        }
        RuleKind::Clean { trim, collapse, spaces, accents, special } => {
            let mut t = s.to_string();
            if *accents {
                use unicode_normalization::UnicodeNormalization;
                t = t.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).collect();
            }
            if *special {
                t = t.chars().filter(|c| c.is_alphanumeric() || " -_.()[]".contains(*c)).collect();
            }
            if *collapse {
                let mut out = String::with_capacity(t.len());
                let mut prev_space = false;
                for c in t.chars() {
                    let sp = c == ' ';
                    if !(sp && prev_space) {
                        out.push(c);
                    }
                    prev_space = sp;
                }
                t = out;
            }
            if *trim {
                t = t.trim().to_string();
            }
            match spaces {
                1 => t.replace(' ', "_"),
                2 => t.replace(' ', "-"),
                3 => t.replace(' ', "."),
                4 => t.replace(' ', ""),
                _ => t,
            }
        }
        RuleKind::PadNumbers { digits } => {
            static NUM: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            let re = NUM.get_or_init(|| Regex::new(r"\d+").unwrap());
            let d = *digits;
            re.replace_all(s, |c: &regex::Captures| format!("{:0>d$}", &c[0])).into_owned()
        }
        RuleKind::RemoveBetween { open, close } => {
            if open.is_empty() || close.is_empty() {
                return s.to_string();
            }
            let mut out = s.to_string();
            let mut from = 0;
            while let Some(a) = out[from..].find(open.as_str()).map(|i| i + from) {
                match out[a + open.len()..].find(close.as_str()) {
                    Some(b) => out.replace_range(a..a + open.len() + b + close.len(), ""),
                    None => break,
                }
                from = a;
            }
            tidy_spaces(&out)
        }
        RuleKind::Truncate { max, from_end } => {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() <= *max {
                s.to_string()
            } else if *from_end {
                chars[chars.len() - max..].iter().collect::<String>().trim().to_string()
            } else {
                chars[..*max].iter().collect::<String>().trim().to_string()
            }
        }
        RuleKind::Swap { sep, joiner } => match (!sep.is_empty()).then(|| s.split_once(sep.as_str())).flatten() {
            Some((a, b)) => format!("{}{}{}", b.trim(), joiner, a.trim()),
            None => s.to_string(),
        },
        RuleKind::Strip { mode } => {
            static LEAD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            static COPY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            match mode {
                0 => LEAD.get_or_init(|| Regex::new(r"^\s*\d+\s*[-._)\]]*\s*").unwrap()).replace(s, "").into_owned(),
                1 => tidy_spaces(&s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>()),
                _ => COPY
                    .get_or_init(|| Regex::new(r"(?i)(\s*\(\d+\)|\s*[-_]\s*(copia|copy|kopie|copie|cópia)(\s*\(?\d+\)?)?|\s+(copia|copy|kopie|copie|cópia)(\s*\(?\d+\)?)?)+$").unwrap())
                    .replace(s, "")
                    .into_owned(),
            }
        }
        RuleKind::Prefix { text } => expand(text, ctx, cur) + s,
        RuleKind::Suffix { text } => s.to_string() + &expand(text, ctx, cur),
        RuleKind::Number { .. } | RuleKind::Ext { .. } | RuleKind::Counter { .. } | RuleKind::Accumulate { .. } | RuleKind::Variable { .. } | RuleKind::Awk { .. } => s.to_string(),
    }
}

/// A wildcard pattern (`*` any text, `?` one character) as a regular expression over the whole name.
fn wildcard_to_regex(p: &str) -> String {
    let mut out = String::from("^");
    for c in p.chars() {
        match c {
            '*' => out.push_str("(.*)"),
            '?' => out.push_str("(.)"),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

/// In a wildcard replacement each `*` or `?` takes what its wildcard matched, in order.
fn wildcard_replacement(w: &str) -> String {
    let mut out = String::new();
    let mut k = 0;
    for c in w.chars() {
        match c {
            '*' | '?' => {
                k += 1;
                out.push_str(&format!("${{{k}}}"));
            }
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out
}

/// Names matching wildcards (several separated by «;») or a regular expression; case doesn't matter.
pub fn name_matcher(text: &str, regex_mode: bool) -> Result<Option<Regex>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let pat = if regex_mode {
        text.to_string()
    } else {
        text.split(';').map(str::trim).filter(|p| !p.is_empty()).map(wildcard_to_regex).collect::<Vec<_>>().join("|")
    };
    RegexBuilder::new(&pat).case_insensitive(true).build().map(Some).map_err(|e| e.to_string().lines().last().unwrap_or("").trim().to_string())
}

fn awk_program() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        ["awk", "gawk", "mawk", "busybox"].into_iter().find(|p| {
            let mut c = std::process::Command::new(p);
            if *p == "busybox" {
                c.arg("awk");
            }
            c.arg("BEGIN{}").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
        })
    })
}

/// Pipes the lines through awk with the script, as `printf lines | awk -F fs 'script'` would,
/// and returns its output lines. Stops it after a few seconds.
/// Runs the script with the awk interpreter built into the program (POSIX awk, no gawk
/// extensions such as `\\d`), on its own thread so a script that never ends can't freeze the window.
fn run_builtin_awk(script: &str, fs: &str, lines: &[String]) -> Result<Vec<String>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    if script.trim().is_empty() {
        return Ok(lines.to_vec());
    }
    if RUNNING.load(Ordering::SeqCst) >= 2 {
        return Err(tr("Un script anterior sigue en marcha (quizá un bucle sin fin). Corrige el script; si continúa, cierra y abre el programa.").to_string());
    }
    let (script, fs, n) = (script.to_string(), fs.to_string(), lines.len());
    let mut input = lines.join("\n");
    input.push('\n');
    let (tx, rx) = std::sync::mpsc::channel();
    RUNNING.fetch_add(1, Ordering::SeqCst);
    std::thread::spawn(move || {
        let run = || -> Result<String, eawk::Error> {
            let mut opts = eawk::Options::default();
            if !fs.is_empty() {
                opts.fs = Some(fs);
            }
            opts.assignments.push(("total".into(), n.to_string()));
            let mut e = eawk::Engine::with_options(&script, opts)?;
            e.exec_captured(&input)
        };
        let r = run().map_err(|e| e.to_string().trim_start_matches("eawk: ").to_string());
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        let _ = tx.send(r);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(out)) => {
            let mut v: Vec<String> = out.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
            if v.last().map(|l| l.is_empty()).unwrap_or(false) {
                v.pop();
            }
            Ok(v)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(tr("El script tardó demasiado (más de 3 segundos) y se detuvo.").to_string()),
    }
}

fn run_awk(script: &str, fs: &str, lines: &[String]) -> Result<Vec<String>, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    if script.trim().is_empty() {
        return Ok(lines.to_vec());
    }
    let Some(prog) = awk_program() else { return Err(tr("No se encontró awk en este equipo.").to_string()) };
    let mut cmd = Command::new(prog);
    if prog == "busybox" {
        cmd.arg("awk");
    }
    if !fs.is_empty() {
        cmd.arg("-F").arg(fs);
    }
    cmd.arg("-v").arg(format!("total={}", lines.len())).arg("--").arg(script);
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut input = lines.join("\n");
    input.push('\n');
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let err_reader = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        err
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > std::time::Duration::from_secs(3) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(tr("El script tardó demasiado (más de 3 segundos) y se detuvo.").to_string());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        let msg = err.lines().filter(|l| !l.trim().is_empty()).take(3).collect::<Vec<_>>().join(" · ");
        return Err(if msg.is_empty() { trf!("awk terminó con error ({})", status) } else { msg });
    }
    let mut v: Vec<String> = out.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
    if v.last().map(|l| l.is_empty()).unwrap_or(false) {
        v.pop();
    }
    Ok(v)
}

/// What a counter or accumulator remembers from one file to the next.
#[derive(Default)]
struct RState {
    k: i64,
    last_key: Option<String>,
    folders: HashMap<PathBuf, i64>,
    total: f64,
}

fn place(s: &str, value: &str, pos: u8, at: usize, sep: &str) -> String {
    match pos {
        0 => format!("{value}{sep}{s}"),
        1 => format!("{s}{sep}{value}"),
        _ => {
            let chars: Vec<char> = s.chars().collect();
            let at = at.min(chars.len());
            chars[..at].iter().collect::<String>() + value + &chars[at..].iter().collect::<String>()
        }
    }
}

fn fmt_counter(n: i64, pad: usize, style: u8) -> String {
    match style {
        1 | 2 if n > 0 => {
            let mut v = n;
            let mut out = Vec::new();
            while v > 0 {
                v -= 1;
                out.push((b'a' + (v % 26) as u8) as char);
                v /= 26;
            }
            let s: String = out.into_iter().rev().collect();
            if style == 2 { s.to_uppercase() } else { s }
        }
        3 if (1..4000).contains(&n) => {
            const R: [(i64, &str); 13] = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")];
            let mut v = n;
            let mut s = String::new();
            for (k, r) in R {
                while v >= k {
                    s.push_str(r);
                    v -= k;
                }
            }
            s
        }
        _ if n < 0 => format!("-{:0pad$}", -n),
        _ => format!("{n:0pad$}"),
    }
}

fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m:02}m{s:02}s")
    }
}

fn first_number(s: &str) -> Option<f64> {
    static NUM: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    NUM.get_or_init(|| Regex::new(r"-?\d+(?:[.,]\d+)?").unwrap()).find(s).and_then(|m| m.as_str().replace(',', ".").parse().ok())
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

fn tidy_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tag text made safe for a file name.
fn safe(v: Option<String>) -> String {
    v.unwrap_or_default().replace(['/', '\\'], "-").trim().to_string()
}

fn self_taken(ctx: &Ctx) -> Option<chrono::NaiveDateTime> {
    ctx.tags().taken
}

fn fmt_naive(dt: chrono::NaiveDateTime, fmt: &str) -> String {
    let fmt = if fmt.is_empty() { "%Y-%m-%d" } else { fmt };
    let mut s = String::new();
    if write!(s, "{}", dt.format(fmt)).is_err() {
        return "?".into();
    }
    s
}

/// What photos and music files say about themselves, read only when a rule asks for it.
#[derive(Default)]
pub struct Tags {
    pub(crate) taken: Option<chrono::NaiveDateTime>,
    pub(crate) camera: Option<String>,
    size: Option<(usize, usize)>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    title: Option<String>,
    pub(crate) genre: Option<String>,
    track: Option<u32>,
    pub(crate) year: Option<u32>,
    duration: Option<u64>,
}

pub(crate) fn read_tags(path: &Path) -> Tags {
    let mut t = Tags::default();
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "jpg" | "jpeg" | "tif" | "tiff" | "heic" | "heif" | "png" | "webp" | "dng" | "cr2" | "nef" | "arw" | "orf" | "rw2" | "avif" | "gif" | "bmp") {
        if let Ok(f) = fs::File::open(path) {
            let mut r = std::io::BufReader::new(f);
            if let Ok(ex) = exif::Reader::new().read_from_container(&mut r) {
                let ascii = |tag: exif::Tag| {
                    ex.get_field(tag, exif::In::PRIMARY)
                        .and_then(|f| if let exif::Value::Ascii(v) = &f.value { v.first().map(|b| String::from_utf8_lossy(b).trim_matches(char::from(0)).trim().to_string()) } else { None })
                        .filter(|s| !s.is_empty())
                };
                for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime] {
                    let dt = ascii(tag).and_then(|s| exif::DateTime::from_ascii(s.as_bytes()).ok());
                    if let Some(d) = dt {
                        t.taken = chrono::NaiveDate::from_ymd_opt(d.year as i32, d.month as u32, d.day as u32).and_then(|x| x.and_hms_opt(d.hour as u32, d.minute as u32, d.second as u32));
                        if t.taken.is_some() {
                            break;
                        }
                    }
                }
                t.camera = match (ascii(exif::Tag::Make), ascii(exif::Tag::Model)) {
                    (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.split_whitespace().next().unwrap_or("").to_lowercase()) => Some(md),
                    (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
                    (a, b) => b.or(a),
                };
            }
        }
        if let Ok(sz) = imagesize::size(path) {
            t.size = Some((sz.width, sz.height));
        }
    }
    if matches!(ext.as_str(), "mp3" | "flac" | "ogg" | "opus" | "m4a" | "mp4" | "aac" | "wav" | "aiff" | "aif" | "wma" | "ape" | "wv" | "mpc" | "spx") {
        use lofty::prelude::*;
        if let Ok(tf) = lofty::read_from_path(path) {
            if let Some(tag) = tf.primary_tag().or_else(|| tf.first_tag()) {
                t.artist = tag.artist().map(|s| s.to_string());
                t.album = tag.album().map(|s| s.to_string());
                t.title = tag.title().map(|s| s.to_string());
                t.genre = tag.genre().map(|s| s.to_string());
                t.track = tag.track();
                t.year = tag.year();
            }
            t.duration = Some(tf.properties().duration().as_secs());
        }
    }
    t
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = true;
    for c in s.chars() {
        if up {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        up = !c.is_alphanumeric() && c != '\'';
    }
    out
}

fn split(name: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn join(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

fn invalid_reason(name: &str) -> Option<String> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Some(tr("nombre vacío").into());
    }
    if name.contains('/') || name.contains('\0') {
        return Some(tr("no puede llevar «/»").into());
    }
    if cfg!(windows) && name.chars().any(|c| "\\:*?\"<>|".contains(c)) {
        return Some(tr("lleva caracteres no permitidos").into());
    }
    if name.len() > 255 {
        return Some(tr("demasiado largo").into());
    }
    None
}

/// Renames through temporary names so swaps and chains (a→b, b→a) work.
fn two_phase(pairs: &[(PathBuf, PathBuf)]) -> (Vec<(PathBuf, PathBuf)>, Vec<String>) {
    let mut errs = vec![];
    let mut temps = vec![];
    for (k, (from, to)) in pairs.iter().enumerate() {
        let tmp = from.with_file_name(format!(".ren-tmp-{}-{k}", std::process::id()));
        match fs::rename(from, &tmp) {
            Ok(()) => temps.push((tmp, from.clone(), to.clone())),
            Err(e) => errs.push(format!("{}: {e}", from.display())),
        }
    }
    let mut done = vec![];
    for (tmp, from, to) in temps {
        if to.exists() {
            let _ = fs::rename(&tmp, &from);
            errs.push(format!("{}: {}", to.display(), tr("ya existe en la carpeta")));
            continue;
        }
        match fs::rename(&tmp, &to) {
            Ok(()) => done.push((from, to)),
            Err(e) => {
                let _ = fs::rename(&tmp, &from);
                errs.push(format!("{}: {e}", to.display()));
            }
        }
    }
    (done, errs)
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = x.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    x.next();
                }
                let mut nb = String::new();
                while let Some(d) = y.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(d);
                    y.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then(ta.cmp(tb));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

fn mtime(i: &Item) -> Option<SystemTime> {
    i.meta.as_ref().and_then(|m| m.modified().ok())
}
fn size(i: &Item) -> u64 {
    i.meta.as_ref().map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(stem: &'a str, ext: &'a str, path: &'a Path) -> Ctx<'a> {
        let tags: &'static OnceCell<Tags> = Box::leak(Box::new(OnceCell::from(Tags { artist: Some("AC/DC".into()), track: Some(3), ..Default::default() })));
        let vars = Box::leak(Box::new(std::cell::RefCell::new(HashMap::from([("x".to_string(), "hola".to_string())]))));
        Ctx { n: 7, pad: 3, path, stem, ext, meta: None, total: 10, now: SystemTime::UNIX_EPOCH, seed: 1, tags, vars }
    }

    #[test]
    fn placeholders() {
        let p = Path::new("/fotos/viaje/IMG_0042.JPG");
        let c = ctx("IMG_0042", "JPG", p);
        assert_eq!(expand("{carpeta}-{n}-{nombre:5:4}", &c, "x"), "viaje-007-0042");
        assert_eq!(expand("{n:1} de {total} {raro}", &c, "x"), "7 de 10 {raro}");
        assert_eq!(expand("{nombre:-2}", &c, "x"), "42");
    }

    #[test]
    fn regex_and_rules() {
        let p = Path::new("/a/b.txt");
        let c = ctx("b", "txt", p);
        let k = RuleKind::Replace { find: r"(\d+)_(\w+)".into(), with: "$2-$1".into(), regex: true, wildcard: false, case_sensitive: false, all: true };
        let re = Regex::new(r"(\d+)_(\w+)").unwrap();
        assert_eq!(apply_rule(&k, Some(&re), "12_foto", &c, ""), "foto-12");
        let clean = RuleKind::Clean { trim: true, collapse: true, spaces: 1, accents: true, special: true };
        assert_eq!(apply_rule(&clean, None, "  Canción  más ¡bonita! ", &c, ""), "Cancion_mas_bonita");
        assert_eq!(apply_rule(&RuleKind::Remove { from: 0, count: 2, from_end: true }, None, "abcdef", &c, ""), "abcd");
        assert_eq!(apply_rule(&RuleKind::Insert { text: "X".into(), pos: 1, from_end: false }, None, "abc", &c, ""), "aXbc");
        assert_eq!(natural_cmp("img2", "img10"), std::cmp::Ordering::Less);
        assert_eq!(apply_rule(&RuleKind::PadNumbers { digits: 3 }, None, "foto 2 de 15", &c, ""), "foto 002 de 015");
        assert_eq!(apply_rule(&RuleKind::RemoveBetween { open: "(".into(), close: ")".into() }, None, "video (1080p) final (v2)", &c, ""), "video final");
        assert_eq!(apply_rule(&RuleKind::Swap { sep: " - ".into(), joiner: " - ".into() }, None, "Artista - Canción", &c, ""), "Canción - Artista");
        assert_eq!(apply_rule(&RuleKind::Strip { mode: 0 }, None, "01 - canción", &c, ""), "canción");
        assert_eq!(apply_rule(&RuleKind::Strip { mode: 2 }, None, "informe - copia (2)", &c, ""), "informe");
        assert_eq!(apply_rule(&RuleKind::Strip { mode: 2 }, None, "informe (1)", &c, ""), "informe");
        assert_eq!(apply_rule(&RuleKind::Truncate { max: 3, from_end: false }, None, "abcdef", &c, ""), "abc");
        assert_eq!(expand("{pista} {artista}", &c, ""), "03 AC-DC");
        assert_eq!(builtin_presets().len(), 17);
        let wc = RuleKind::Replace { find: "IMG_*".into(), with: "Foto *".into(), regex: false, wildcard: true, case_sensitive: false, all: true };
        let wre = RegexBuilder::new(&wildcard_to_regex("IMG_*")).case_insensitive(true).build().unwrap();
        assert_eq!(apply_rule(&wc, Some(&wre), "IMG_0042 playa", &c, ""), "Foto 0042 playa");
        assert_eq!(apply_rule(&wc, Some(&wre), "DSC_1", &c, ""), "DSC_1");
        let m = name_matcher("*.jpg; foto*", false).unwrap().unwrap();
        assert!(m.is_match("a.JPG") && m.is_match("Foto 1.png") && !m.is_match("b.png"));
        assert!(name_matcher("(", true).is_err());
        assert_eq!(fmt_counter(28, 2, 1), "ab");
        assert_eq!(fmt_counter(14, 2, 3), "XIV");
        assert_eq!(fmt_counter(5, 3, 0), "005");
        assert_eq!(place("abc", "9", 2, 1, "-"), "a9bc");
        assert_eq!(fmt_duration(3725), "1h02m05s");
        assert_eq!(expand("{var:x}!", &c, ""), "hola!");
        assert_eq!(apply_rule(&RuleKind::Prefix { text: "{n}_".into() }, None, "a", &c, ""), "007_a");
    }

    #[test]
    fn counters_accumulators_and_variables() {
        let dir = std::env::temp_dir().join(format!("ren-acc-{}", std::process::id()));
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        for (f, n) in [("a/x1.txt", 10), ("a/y2.txt", 20), ("b/z3.txt", 30)] {
            fs::write(dir.join(f), vec![0u8; n]).unwrap();
        }
        let files = vec![dir.join("a/x1.txt"), dir.join("a/y2.txt"), dir.join("b/z3.txt")];
        let mut r = Renamer::new(files, dir.clone());
        let rule = |kind| Rule { on: true, scope: Scope::Name, kind };
        r.rules = vec![
            rule(RuleKind::Variable { name: "v".into(), value: "{nombre:1:1}".into(), mode: 1, sep: "+".into() }),
            rule(RuleKind::Accumulate { what: 2, inclusive: true, pos: 1, at: 0, sep: "_s".into() }),
            rule(RuleKind::Counter { start: 1, step: 1, pad: 1, style: 1, pos: 0, at: 0, sep: "-".into(), reset: 2, group: "{carpeta}".into() }),
            rule(RuleKind::Suffix { text: "_{var:v}".into() }),
        ];
        r.recompute();
        let names: Vec<String> = r.items.iter().map(|i| i.new.clone()).collect();
        assert_eq!(names, vec!["a-x1_s1_x.txt", "b-y2_s3_x+y.txt", "a-z3_s6_x+y+z.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn awk_scripts() {
        let same = |script: &str| {
            let lines = vec!["uno dos".to_string(), "tres cuatro".to_string()];
            let a = run_builtin_awk(script, "", &lines).unwrap();
            if awk_program().is_some() {
                assert_eq!(a, run_awk(script, "", &lines).unwrap(), "{script}");
            }
            a
        };
        assert_eq!(same("{ printf \"%03d %s\\n\", NR, toupper($2) }"), vec!["001 DOS", "002 CUATRO"]);
        assert_eq!(same("{ n = split($0, p, \" \"); gsub(/o/, \"0\", p[1]); print p[n] \"_\" p[1] \"_\" total }"), vec!["dos_un0_2", "cuatro_tres_2"]);
        assert!(run_builtin_awk("BEGIN { while (1) {} }", "", &["a".to_string()]).is_err());
        for (sc, mode, _) in AWK_EXAMPLES {
            let line = if *mode == 1 { "a\tb\ttxt\tdir\t2000000\t2024-01-02\t/x/b.txt" } else { "uno_dos tres" };
            let out = run_builtin_awk(sc, if *mode == 1 { "\t" } else { "" }, &[line.to_string()]).unwrap();
            assert_eq!(out.len(), 1, "{sc}");
        }
        let dir = std::env::temp_dir().join(format!("ren-awk-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for f in ["uno dos.txt", "tres cuatro.txt"] {
            fs::write(dir.join(f), "x").unwrap();
        }
        let mut r = Renamer::new(vec![dir.join("uno dos.txt"), dir.join("tres cuatro.txt")], dir.clone());
        let rule = |kind| Rule { on: true, scope: Scope::Name, kind };
        r.rules = vec![
            rule(RuleKind::Prefix { text: "a ".into() }),
            rule(RuleKind::Awk { script: "{ print $NF \"_\" NR \"/\" total }".into(), fs: String::new(), input: 0, system: false }),
            rule(RuleKind::Suffix { text: "!".into() }),
        ];
        r.recompute();
        let names: Vec<String> = r.items.iter().map(|i| i.new.clone()).collect();
        assert_eq!(names, vec!["dos_1/2!.txt", "cuatro_2/2!.txt"]);
        r.rules[1] = rule(RuleKind::Awk { script: "{ print $4 \"-\" $3 }".into(), fs: String::new(), input: 1, system: false });
        r.recompute();
        assert!(r.items[0].new.ends_with("-txt!.txt"), "{}", r.items[0].new);
        r.rules[1] = rule(RuleKind::Awk { script: "NR == 1 { print }".into(), fs: String::new(), input: 0, system: false });
        r.recompute();
        assert!(r.rule_errors[1].is_some());
        r.rules[1] = rule(RuleKind::Awk { script: "{ print (".into(), fs: String::new(), input: 0, system: false });
        r.recompute();
        assert!(r.rule_errors[1].is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_and_undo_swap() {
        let dir = std::env::temp_dir().join(format!("ren-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "A").unwrap();
        fs::write(dir.join("b.txt"), "B").unwrap();
        let mut r = Renamer::new(vec![dir.join("a.txt"), dir.join("b.txt")], dir.clone());
        r.rules = vec![Rule { on: true, scope: Scope::Name, kind: RuleKind::Replace { find: "^(a|b)$".into(), with: "x_$1".into(), regex: true, wildcard: false, case_sensitive: true, all: true } }];
        r.recompute();
        assert!(r.items.iter().all(|i| i.status == Status::Ok));
        let mut ev = vec![];
        r.apply(&mut ev);
        assert_eq!(fs::read_to_string(dir.join("x_a.txt")).unwrap(), "A");
        r.undo_last(&mut ev);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "A");
        // Two renames in a row, then back to the original names; the history can restore too.
        r.apply(&mut ev);
        r.rules = vec![Rule { on: true, scope: Scope::Name, kind: RuleKind::Prefix { text: "z_".into() } }];
        r.recompute();
        r.apply(&mut ev);
        assert!(dir.join("z_x_b.txt").exists());
        r.restore_originals(&mut ev);
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "B");
        let last = r.history.len() - 1;
        r.restore_batch(last, &mut ev);
        assert!(dir.join("z_x_b.txt").exists());
        r.restore_originals(&mut ev);
        r.recompute();
        // Conflict with an existing file, then automatic suffix.
        fs::write(dir.join("c.txt"), "C").unwrap();
        r.rules = vec![Rule { on: true, scope: Scope::Name, kind: RuleKind::Template { pattern: "c".into() } }];
        r.recompute();
        assert!(r.items.iter().all(|i| matches!(i.status, Status::Conflict(_))));
        r.conflicts = 1;
        r.recompute();
        let names: Vec<String> = r.items.iter().map(|i| i.new.clone()).collect();
        assert_eq!(names, vec!["c (2).txt", "c (3).txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
