//! Encrypt and decrypt files with a password: XChaCha20-Poly1305 in 1 MiB authenticated chunks
//! (any change or truncation is detected), key from the password with Argon2id. The original
//! name and date travel inside the encrypted data, so decrypting restores them even when the
//! encrypted file was given a random name.

use crate::fsops::{self, human_size};
use crate::i18n::tr;
use crate::roots::{picker_window, DestField, Picker};
use crate::theme;
use crate::tools::ToolEvent;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::XChaCha20Poly1305;
use eframe::egui::{self, Color32, RichText};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"FARCRYPT";
const VERSION: u8 = 1;
const TAG: usize = 16;
const CHUNK_LOG2: u8 = 20;

#[derive(Clone, Copy, PartialEq, Debug)]
struct Kdf {
    m: u32,
    t: u32,
    p: u32,
}

const STRENGTHS: [(&str, &str, Kdf); 3] = [
    ("Normal", "19 MB de memoria: rápido y seguro para casi todo.", Kdf { m: 19 * 1024, t: 2, p: 1 }),
    ("Fuerte", "64 MB y más pasadas: más lento de abrir, mucho más difícil de adivinar.", Kdf { m: 64 * 1024, t: 3, p: 1 }),
    ("Máxima", "256 MB y más pasadas: para lo más delicado (tarda unos segundos por contraseña).", Kdf { m: 256 * 1024, t: 4, p: 2 }),
];

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

fn derive(password: &str, salt: &[u8; 16], k: Kdf) -> Result<[u8; 32], String> {
    let params = argon2::Params::new(k.m, k.t, k.p, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params).hash_password_into(password.as_bytes(), salt, &mut key).map_err(|e| e.to_string())?;
    Ok(key)
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Default)]
struct Progress {
    current: Mutex<String>,
    bytes: AtomicU64,
    done: AtomicUsize,
    total: AtomicUsize,
}

fn check(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr("cancelado").to_string())
    } else {
        Ok(())
    }
}

/// What is stored (encrypted) alongside the data.
fn meta_json(name: &str, mtime: u64, size: u64) -> Vec<u8> {
    serde_json::json!({ "name": name, "mtime": mtime, "size": size }).to_string().into_bytes()
}

struct Header {
    kdf: Kdf,
    salt: [u8; 16],
    nonce: [u8; 19],
    chunk: usize,
    meta_ct: Vec<u8>,
    len: u64,
}

fn read_header(f: &mut fs::File) -> Result<Header, String> {
    let total = f.metadata().map_err(es)?.len();
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|_| tr("no es un archivo cifrado con esta herramienta").to_string())?;
    if &magic != MAGIC {
        return Err(tr("no es un archivo cifrado con esta herramienta").to_string());
    }
    let mut b = [0u8; 1 + 12 + 16 + 19 + 1 + 4];
    f.read_exact(&mut b).map_err(|_| tr("archivo dañado: cabecera incompleta").to_string())?;
    if b[0] != VERSION {
        return Err(tr("versión de formato desconocida").to_string());
    }
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let kdf = Kdf { m: u32_at(1), t: u32_at(5), p: u32_at(9) };
    if kdf.m > 2 * 1024 * 1024 || kdf.t > 64 || kdf.p > 64 || kdf.m < 8 || kdf.t == 0 || kdf.p == 0 {
        return Err(tr("archivo dañado: parámetros no válidos").to_string());
    }
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&b[13..29]);
    let mut nonce = [0u8; 19];
    nonce.copy_from_slice(&b[29..48]);
    let chunk_log2 = b[48];
    if !(10..=26).contains(&chunk_log2) {
        return Err(tr("archivo dañado: parámetros no válidos").to_string());
    }
    let meta_len = u32_at(49) as usize;
    if meta_len > 64 * 1024 || meta_len as u64 + 62 > total {
        return Err(tr("archivo dañado: cabecera incompleta").to_string());
    }
    let mut meta_ct = vec![0u8; meta_len];
    f.read_exact(&mut meta_ct).map_err(es)?;
    Ok(Header { kdf, salt, nonce, chunk: 1 << chunk_log2, meta_ct, len: total })
}

#[allow(clippy::too_many_arguments)]
fn encrypt_stream(src: &Path, out: &mut dyn Write, key: &[u8; 32], kdf: Kdf, salt: [u8; 16], chunk_log2: u8, prog: &Progress, cancel: &AtomicBool) -> Result<(), String> {
    let mut input = fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let meta = input.metadata().map_err(es)?;
    let size = meta.len();
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let nonce = random::<19>()?;
    let mut enc = Some(EncryptorBE32::from_aead(XChaCha20Poly1305::new(key.into()), nonce.as_slice().into()));
    let meta_ct = enc.as_mut().unwrap().encrypt_next(meta_json(&name, mtime, size).as_slice()).map_err(|_| tr("error al cifrar").to_string())?;
    out.write_all(MAGIC).map_err(es)?;
    out.write_all(&[VERSION]).map_err(es)?;
    for v in [kdf.m, kdf.t, kdf.p] {
        out.write_all(&v.to_le_bytes()).map_err(es)?;
    }
    out.write_all(&salt).map_err(es)?;
    out.write_all(&nonce).map_err(es)?;
    out.write_all(&[chunk_log2]).map_err(es)?;
    out.write_all(&(meta_ct.len() as u32).to_le_bytes()).map_err(es)?;
    out.write_all(&meta_ct).map_err(es)?;
    let chunk = 1usize << chunk_log2;
    let mut buf = vec![0u8; chunk];
    let mut remaining = size;
    loop {
        check(cancel)?;
        let n = remaining.min(chunk as u64) as usize;
        input.read_exact(&mut buf[..n]).map_err(|e| format!("{}: {e}", src.display()))?;
        remaining -= n as u64;
        let last = remaining == 0;
        let ct = if last { enc.take().unwrap().encrypt_last(&buf[..n]) } else { enc.as_mut().unwrap().encrypt_next(&buf[..n]) }.map_err(|_| tr("error al cifrar").to_string())?;
        out.write_all(&ct).map_err(es)?;
        prog.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if last {
            return Ok(());
        }
    }
}

struct Opened {
    name: String,
    mtime: u64,
}

/// Decrypts into `out`; every chunk is authenticated, so a wrong password or damage is an error.
fn decrypt_stream(f: &mut fs::File, h: &Header, key: &[u8; 32], out: Option<&mut dyn Write>, prog: &Progress, cancel: &AtomicBool) -> Result<Opened, String> {
    let mut dec = Some(DecryptorBE32::from_aead(XChaCha20Poly1305::new(key.into()), h.nonce.as_slice().into()));
    let meta = dec.as_mut().unwrap().decrypt_next(h.meta_ct.as_slice()).map_err(|_| tr("contraseña incorrecta o archivo dañado").to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&meta).map_err(|_| tr("archivo dañado").to_string())?;
    let opened = Opened { name: v["name"].as_str().unwrap_or("").to_string(), mtime: v["mtime"].as_u64().unwrap_or(0) };
    let ct_chunk = h.chunk + TAG;
    let header_len = 8 + 1 + 12 + 16 + 19 + 1 + 4 + h.meta_ct.len() as u64;
    let mut remaining = h.len.saturating_sub(header_len);
    let mut sink = io::sink();
    let out: &mut dyn Write = match out {
        Some(o) => o,
        None => &mut sink,
    };
    let mut buf = vec![0u8; ct_chunk];
    loop {
        check(cancel)?;
        let n = remaining.min(ct_chunk as u64) as usize;
        if n < TAG {
            return Err(tr("archivo dañado o incompleto").to_string());
        }
        f.read_exact(&mut buf[..n]).map_err(es)?;
        remaining -= n as u64;
        let last = remaining == 0;
        let plain = if last { dec.take().unwrap().decrypt_last(&buf[..n]) } else { dec.as_mut().unwrap().decrypt_next(&buf[..n]) }.map_err(|_| tr("archivo dañado o modificado").to_string())?;
        out.write_all(&plain).map_err(es)?;
        prog.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if last {
            return Ok(opened);
        }
    }
}

fn is_encrypted(p: &Path) -> bool {
    let mut m = [0u8; 8];
    fs::File::open(p).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == MAGIC
}

// ---------------- plan and run ----------------

#[derive(Clone)]
struct Opts {
    mode: u8,
    password: String,
    kdf: Kdf,
    hide_name: bool,
    verify: bool,
    delete_orig: bool,
    dest_other: bool,
    dest: PathBuf,
    conflict: u8,
    keep_dates: bool,
    #[allow(dead_code)]
    recursive: bool,
    chunk_log2: u8,
}

#[derive(Clone, Default)]
struct Res {
    src: PathBuf,
    out: PathBuf,
    encrypted: bool,
    bytes: u64,
    error: Option<String>,
    skipped: bool,
    deleted: bool,
}

fn collect(items: &[PathBuf], recursive: bool) -> Vec<PathBuf> {
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
                    if k.is_dir() {
                        if recursive {
                            stack.push(k);
                        }
                    } else if k.is_file() {
                        out.push(k);
                    }
                }
            }
        }
    }
    out.dedup();
    out
}

fn target(dir: &Path, name: &str, conflict: u8, taken: &Mutex<std::collections::HashSet<PathBuf>>) -> Option<PathBuf> {
    let mut out = dir.join(name);
    let mut t = taken.lock().ok()?;
    if out.exists() || t.contains(&out) {
        match conflict {
            2 if !t.contains(&out) => return None,
            1 if !t.contains(&out) => {}
            _ => {
                out = fsops::unique_name(dir, name);
                let (stem, ext) = match name.rfind('.') {
                    Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
                    _ => (name.to_string(), String::new()),
                };
                let mut k = 2;
                while t.contains(&out) {
                    out = dir.join(format!("{stem} ({k}){ext}"));
                    k += 1;
                }
            }
        }
    }
    t.insert(out.clone());
    Some(out)
}

struct Shared<'a> {
    o: &'a Opts,
    prog: &'a Progress,
    cancel: &'a AtomicBool,
    taken: Mutex<std::collections::HashSet<PathBuf>>,
    enc_key: Mutex<Option<([u8; 16], [u8; 32])>>,
    keys: Mutex<HashMap<([u8; 16], u32, u32, u32), [u8; 32]>>,
}

fn run_one(src: &Path, sh: &Shared) -> Res {
    let o = sh.o;
    let mut r = Res { src: src.to_path_buf(), ..Default::default() };
    if let Ok(mut c) = sh.prog.current.lock() {
        *c = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    }
    let is_enc = is_encrypted(src);
    let encrypt = match o.mode {
        0 => true,
        1 => false,
        _ => !is_enc,
    };
    r.encrypted = encrypt;
    let result = (|| -> Result<(), String> {
        let dir = if o.dest_other { o.dest.clone() } else { src.parent().map(Path::to_path_buf).unwrap_or_default() };
        fs::create_dir_all(&dir).map_err(es)?;
        if encrypt {
            let (salt, key) = {
                let mut g = sh.enc_key.lock().map_err(es)?;
                if g.is_none() {
                    let salt = random::<16>()?;
                    *g = Some((salt, derive(&o.password, &salt, o.kdf)?));
                }
                g.unwrap()
            };
            let name = if o.hide_name { format!("{}.enc", random::<6>()?.iter().map(|b| format!("{b:02x}")).collect::<String>()) } else { format!("{}.enc", src.file_name().unwrap_or_default().to_string_lossy()) };
            let Some(out) = target(&dir, &name, o.conflict, &sh.taken) else {
                r.skipped = true;
                return Ok(());
            };
            let tmp = out.with_file_name(format!(".{}.part", out.file_name().unwrap_or_default().to_string_lossy()));
            let write = || -> Result<(), String> {
                let mut w = io::BufWriter::new(fs::File::create(&tmp).map_err(es)?);
                encrypt_stream(src, &mut w, &key, o.kdf, salt, o.chunk_log2, sh.prog, sh.cancel)?;
                w.flush().map_err(es)
            };
            if let Err(e) = write() {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            if o.verify {
                let check = (|| -> Result<(), String> {
                    let mut f = fs::File::open(&tmp).map_err(es)?;
                    let h = read_header(&mut f)?;
                    decrypt_stream(&mut f, &h, &key, None, &Progress::default(), sh.cancel).map(|_| ())
                })();
                if let Err(e) = check {
                    let _ = fs::remove_file(&tmp);
                    return Err(format!("{}: {e}", tr("falló la verificación")));
                }
            }
            fs::rename(&tmp, &out).map_err(es)?;
            r.bytes = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            r.out = out;
        } else {
            let mut f = fs::File::open(src).map_err(es)?;
            let h = read_header(&mut f)?;
            let id = (h.salt, h.kdf.m, h.kdf.t, h.kdf.p);
            let cached = sh.keys.lock().map_err(es)?.get(&id).copied();
            let key = match cached {
                Some(k) => k,
                None => {
                    let k = derive(&o.password, &h.salt, h.kdf)?;
                    sh.keys.lock().map_err(es)?.insert(id, k);
                    k
                }
            };
            // The name is only known after the first (authenticated) chunk, so write to a temporary file first.
            let tmp = dir.join(format!(".{}.dec.part", random::<4>()?.iter().map(|b| format!("{b:02x}")).collect::<String>()));
            let mut w = io::BufWriter::new(fs::File::create(&tmp).map_err(es)?);
            let opened = match decrypt_stream(&mut f, &h, &key, Some(&mut w), sh.prog, sh.cancel) {
                Ok(o) => o,
                Err(e) => {
                    drop(w);
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
            };
            w.flush().map_err(es)?;
            let file = w.into_inner().map_err(|e| e.to_string())?;
            if o.keep_dates && opened.mtime > 0 {
                let _ = file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(opened.mtime));
            }
            drop(file);
            let safe: String = Path::new(&opened.name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let name = if safe.is_empty() {
                let s = src.file_name().unwrap_or_default().to_string_lossy().to_string();
                s.strip_suffix(".enc").map(str::to_string).unwrap_or_else(|| format!("{s}.descifrado"))
            } else {
                safe
            };
            let Some(out) = target(&dir, &name, o.conflict, &sh.taken) else {
                let _ = fs::remove_file(&tmp);
                r.skipped = true;
                return Ok(());
            };
            if out.exists() {
                let _ = fs::remove_file(&out);
            }
            fs::rename(&tmp, &out).map_err(es)?;
            r.bytes = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            r.out = out;
        }
        if o.delete_orig && trash::delete(src).is_ok() {
            r.deleted = true;
        }
        Ok(())
    })();
    if let Err(e) = result {
        r.error = Some(e);
    }
    sh.prog.done.fetch_add(1, Ordering::Relaxed);
    r
}

fn run_all(files: &[PathBuf], o: &Opts, prog: &Progress, cancel: &AtomicBool) -> Vec<Res> {
    let sh = Shared { o, prog, cancel, taken: Mutex::new(Default::default()), enc_key: Mutex::new(None), keys: Mutex::new(HashMap::new()) };
    prog.total.store(files.len(), Ordering::Relaxed);
    let results: Mutex<Vec<(usize, Res)>> = Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..2.min(files.len()).max(1) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = run_one(&files[i], &sh);
                if let Ok(mut v) = results.lock() {
                    v.push((i, r));
                }
            });
        }
    });
    let mut v = results.into_inner().unwrap_or_default();
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

// ---------------- the tool ----------------

struct Job {
    rx: Receiver<(Vec<Res>, f32)>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct Crypter {
    items: Vec<PathBuf>,
    picker: Option<Picker>,
    dest: DestField,
    mode: u8,
    password: String,
    password2: String,
    show_pw: bool,
    strength: usize,
    hide_name: bool,
    verify: bool,
    delete_orig: bool,
    confirm_delete: bool,
    dest_other: bool,
    conflict: u8,
    keep_dates: bool,
    recursive: bool,
    job: Option<Job>,
    results: Vec<Res>,
    status: Option<(String, bool)>,
}

const MODES: [(&str, &str); 3] = [
    ("Cifrar", "Convierte los archivos en copias ilegibles sin la contraseña (.enc)."),
    ("Descifrar", "Recupera los archivos originales con la contraseña."),
    ("Automático", "Los archivos .enc se descifran y los demás se cifran."),
];
const CONFLICTS: [&str; 3] = ["Cambiar el nombre (copia)", "Sobrescribir", "Omitir"];

impl Crypter {
    pub fn new(targets: Vec<PathBuf>) -> Crypter {
        Crypter {
            items: targets.into_iter().filter(|p| p.exists()).collect(),
            picker: None,
            dest: DestField::new("crypt_dest"),
            mode: 2,
            password: String::new(),
            password2: String::new(),
            show_pw: false,
            strength: 0,
            hide_name: false,
            verify: true,
            delete_orig: false,
            confirm_delete: false,
            dest_other: false,
            conflict: 0,
            keep_dates: true,
            recursive: false,
            job: None,
            results: vec![],
            status: None,
        }
    }

    /// Encrypts the folder's files with a sample password, for trying the tool (`--crypt`).
    pub fn demo(&mut self, dir: &Path) {
        self.items = vec![dir.to_path_buf()];
        self.password = "demo".into();
        self.password2 = "demo".into();
        self.mode = 0;
        self.dest_other = true;
        self.dest.text = dir.join("_cifrado").to_string_lossy().to_string();
        self.start();
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.exists() && !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn start(&mut self) {
        if self.items.is_empty() {
            self.status = Some((tr("Añade archivos o carpetas.").to_string(), true));
            return;
        }
        if self.password.is_empty() {
            self.status = Some((tr("Escribe la contraseña.").to_string(), true));
            return;
        }
        let files = collect(&self.items, self.recursive);
        let encrypting = files.iter().any(|f| self.mode == 0 || (self.mode == 2 && !is_encrypted(f)));
        if encrypting && self.password != self.password2 {
            self.status = Some((tr("Las dos contraseñas no coinciden.").to_string(), true));
            return;
        }
        if self.dest_other && self.dest.text.trim().is_empty() {
            self.status = Some((tr("Elige la carpeta de destino.").to_string(), true));
            return;
        }
        if self.delete_orig && !self.confirm_delete {
            self.status = Some((tr("Marca la casilla para confirmar que los originales vayan a la papelera.").to_string(), true));
            return;
        }
        if files.is_empty() {
            self.status = Some((tr("No hay archivos en lo que has añadido.").to_string(), true));
            return;
        }
        let o = Opts {
            mode: self.mode,
            password: self.password.clone(),
            kdf: STRENGTHS[self.strength].2,
            hide_name: self.hide_name,
            verify: self.verify,
            delete_orig: self.delete_orig,
            dest_other: self.dest_other,
            dest: self.dest.path(),
            conflict: self.conflict,
            keep_dates: self.keep_dates,
            recursive: self.recursive,
            chunk_log2: CHUNK_LOG2,
        };
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let r = run_all(&files, &o, &p2, &c2);
            let _ = tx.send((r, started.elapsed().as_secs_f32()));
        });
        self.results.clear();
        self.status = None;
        self.job = Some(Job { rx, progress, cancel, started: Instant::now() });
    }

    fn poll(&mut self, events: &mut Vec<ToolEvent>) {
        let Some(job) = &self.job else { return };
        let Ok((results, secs)) = job.rx.try_recv() else { return };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        let _ = job.started;
        self.job = None;
        let ok = results.iter().filter(|r| r.error.is_none() && !r.skipped).count();
        let bad = results.iter().filter(|r| r.error.is_some()).count();
        let head = if cancelled { tr("Cancelado") } else { tr("Terminado") };
        self.status = Some((trf!("{}: {} archivo(s) · {} error(es) · {} s", head, ok, bad, format!("{secs:.1}")), bad > 0 || cancelled));
        self.results = results;
        self.confirm_delete = false;
        events.push(ToolEvent::Reload);
    }

    // ---------------- UI ----------------

    pub fn ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<ToolEvent>, selection: &[PathBuf], cwd: &Path) {
        self.poll(events);
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);
        let busy = self.job.is_some();
        egui::ScrollArea::vertical().id_salt("crypt_all").auto_shrink([false, false]).max_height(ui.available_height() - 4.0).show(ui, |ui| {
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
        let added = picker_window(&ctx, "crypt_picker", &mut self.picker);
        self.add(added);
    }

    fn items_ui(&mut self, ui: &mut egui::Ui, selection: &[PathBuf], cwd: &Path) {
        ui.label(RichText::new(tr("ARCHIVOS")).small().strong().color(theme::p().sub));
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("crypt_items").max_height(100.0).auto_shrink([false, true]).show(ui, |ui| {
            for (k, p) in self.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text(tr("Quitar de la lista")).clicked() {
                        remove = Some(k);
                    }
                    ui.label(if p.is_dir() { "📁" } else if is_encrypted(p) { "🔒" } else { "🗋" });
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
            if ui.button(tr("📂 Examinar…")).clicked() {
                self.picker = Some(Picker::new(self.items.last().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| cwd.to_path_buf())));
            }
            if ui.add_enabled(!self.items.is_empty(), egui::Button::new(tr("🧹 Vaciar lista"))).clicked() {
                self.items.clear();
            }
            ui.checkbox(&mut self.recursive, tr("Incluir subcarpetas"));
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui, cwd: &Path, selection: &[PathBuf]) {
        ui.label(RichText::new(tr("QUÉ HACER")).small().strong().color(theme::p().sub));
        ui.horizontal_wrapped(|ui| {
            for (k, (name, desc)) in MODES.iter().enumerate() {
                ui.selectable_value(&mut self.mode, k as u8, tr(name)).on_hover_text(tr(desc));
            }
        });
        ui.label(RichText::new(tr(MODES[self.mode as usize].1)).small().color(theme::p().sub));
        ui.horizontal(|ui| {
            crate::ui_kit::label(ui, tr("Contraseña"));
            ui.add(egui::TextEdit::singleline(&mut self.password).password(!self.show_pw).desired_width(220.0));
            if self.mode != 1 {
                ui.add(egui::TextEdit::singleline(&mut self.password2).password(!self.show_pw).hint_text(tr("repetir")).desired_width(220.0));
            }
            ui.checkbox(&mut self.show_pw, tr("mostrar"));
        });
        ui.label(RichText::new(tr("Sin la contraseña no hay forma de recuperar los archivos: ni esta herramienta ni nadie puede. Guárdala en un lugar seguro.")).small().color(theme::p().danger));
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Guardar en"));
            ui.selectable_value(&mut self.dest_other, false, tr("Junto al original"));
            ui.selectable_value(&mut self.dest_other, true, tr("Otra carpeta"));
            if self.dest_other {
                self.dest.ui(ui, cwd, selection);
            }
        });
        ui.horizontal_wrapped(|ui| {
            crate::ui_kit::label(ui, tr("Si ya existe"));
            for (k, l) in CONFLICTS.iter().enumerate() {
                ui.selectable_value(&mut self.conflict, k as u8, tr(l));
            }
        });
        egui::CollapsingHeader::new(tr("⚙ Más opciones")).default_open(false).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                crate::ui_kit::label(ui, tr("Protección"));
                for (k, (name, desc, _)) in STRENGTHS.iter().enumerate() {
                    ui.selectable_value(&mut self.strength, k, tr(name)).on_hover_text(tr(desc));
                }
            });
            ui.label(RichText::new(tr(STRENGTHS[self.strength].1)).small().color(theme::p().muted));
            ui.checkbox(&mut self.hide_name, tr("Ocultar también el nombre: el archivo cifrado recibe un nombre al azar")).on_hover_text(tr("Al descifrar se recupera el nombre original."));
            ui.checkbox(&mut self.verify, tr("Verificar al cifrar")).on_hover_text(tr("Comprueba que lo cifrado se puede volver a abrir con la contraseña. Recomendado."));
            ui.checkbox(&mut self.keep_dates, tr("Al descifrar, recuperar la fecha de modificación"));
            ui.checkbox(&mut self.delete_orig, tr("Enviar los originales a la papelera al terminar")).on_hover_text(tr("Los archivos de partida de cada operación correcta."));
            if self.delete_orig {
                ui.checkbox(&mut self.confirm_delete, RichText::new(tr("Confirmo que los originales vayan a la papelera.")).color(theme::p().danger));
            }
            ui.label(RichText::new(tr("Cifrado XChaCha20-Poly1305 en bloques autenticados y clave derivada con Argon2id. Cualquier cambio en el archivo cifrado se detecta.")).small().color(theme::p().muted));
        });
    }

    fn action_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            let label = match self.mode {
                0 => tr("🔒 Cifrar"),
                1 => tr("🔓 Descifrar"),
                _ => tr("🔐 Cifrar o descifrar"),
            };
            let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(theme::accent());
            if ui.add_enabled(!busy && !self.items.is_empty(), btn).clicked() {
                self.start();
            }
            if let Some(job) = &self.job {
                ui.spinner();
                let p = &job.progress;
                let (d, t) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(180.0).show_percentage());
                let secs = job.started.elapsed().as_secs_f64().max(0.1);
                ui.label(RichText::new(format!("{d}/{t} · {}/s", human_size((p.bytes.load(Ordering::Relaxed) as f64 / secs) as u64))).small());
                if ui.button(tr("Cancelar")).clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                if let Ok(c) = p.current.lock() {
                    ui.label(RichText::new(c.as_str()).small().color(theme::p().muted));
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
        egui::ScrollArea::vertical().id_salt("crypt_res").max_height(crate::tools::grow(ui, 240.0)).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("crypt_grid").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for r in &self.results {
                    ui.label(r.src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                    ui.label(RichText::new(tr(if r.encrypted { "🔒 cifrado" } else { "🔓 descifrado" })).small().color(theme::p().sub));
                    if r.out.as_os_str().is_empty() {
                        ui.label("");
                    } else if ui.link(RichText::new(r.out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).small()).on_hover_text(tr("Ir a la carpeta")).clicked() {
                        if let Some(p) = r.out.parent() {
                            events.push(ToolEvent::Navigate(p.to_path_buf()));
                        }
                    }
                    let (txt, col) = match (&r.error, r.skipped) {
                        (Some(e), _) => (format!("✖ {e}"), theme::p().danger),
                        (_, true) => (tr("omitido: ya existe").to_string(), theme::p().muted),
                        _ => (format!("✔ {}{}", human_size(r.bytes), if r.deleted { format!(" · {}", tr("original a la papelera")) } else { String::new() }), theme::p().ok),
                    };
                    ui.label(RichText::new(txt).small().color(col));
                    ui.end_row();
                }
            });
        });
    }
}

#[allow(dead_code)]
fn _now() -> SystemTime {
    SystemTime::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("crypt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(mode: u8, pw: &str) -> Opts {
        Opts { mode, password: pw.into(), kdf: Kdf { m: 64, t: 1, p: 1 }, hide_name: false, verify: true, delete_orig: false, dest_other: false, dest: PathBuf::new(), conflict: 0, keep_dates: true, recursive: false, chunk_log2: 10 }
    }

    fn go(files: &[PathBuf], o: &Opts) -> Vec<Res> {
        run_all(files, o, &Progress::default(), &AtomicBool::new(false))
    }

    #[test]
    fn round_trip_across_chunk_sizes_and_random_names() {
        let d = dir("rt");
        // Empty, tiny, exactly one chunk (1 KiB), a multiple of it, and a bit more.
        let sizes = [0usize, 5, 1024, 2048, 3000];
        for (k, n) in sizes.iter().enumerate() {
            fs::write(d.join(format!("f{k}.bin")), (0..*n).map(|i| (i * 7 % 251) as u8).collect::<Vec<u8>>()).unwrap();
        }
        let files: Vec<PathBuf> = (0..sizes.len()).map(|k| d.join(format!("f{k}.bin"))).collect();
        let mut o = opts(0, "clave");
        o.hide_name = true;
        let enc = go(&files, &o);
        assert!(enc.iter().all(|r| r.error.is_none()), "{:?}", enc.iter().map(|r| r.error.clone()).collect::<Vec<_>>());
        assert!(enc.iter().all(|r| is_encrypted(&r.out) && r.out.extension().unwrap() == "enc"));
        assert!(enc.iter().all(|r| !r.out.file_name().unwrap().to_string_lossy().contains(".bin")), "names hidden");
        // The originals are still there; move them away so decryption has to recreate them.
        let backup = dir("rt-orig");
        for f in &files {
            fs::rename(f, backup.join(f.file_name().unwrap())).unwrap();
        }
        let encrypted: Vec<PathBuf> = enc.iter().map(|r| r.out.clone()).collect();
        let dec = go(&encrypted, &opts(1, "clave"));
        assert!(dec.iter().all(|r| r.error.is_none()), "{:?}", dec.iter().map(|r| r.error.clone()).collect::<Vec<_>>());
        for f in &files {
            assert_eq!(fs::read(f).unwrap(), fs::read(backup.join(f.file_name().unwrap())).unwrap(), "{f:?}");
        }
        fs::remove_dir_all(&d).unwrap();
        fs::remove_dir_all(&backup).unwrap();
    }

    #[test]
    fn wrong_password_damage_and_truncation_are_detected() {
        let d = dir("bad");
        let f = d.join("secreto.txt");
        fs::write(&f, "contenido muy privado ".repeat(300)).unwrap();
        let enc = go(&[f.clone()], &opts(0, "bien"));
        let out = enc[0].out.clone();
        assert!(out.ends_with("secreto.txt.enc"));
        let wrong = go(&[out.clone()], &opts(1, "mal"));
        assert!(wrong[0].error.as_deref().unwrap().contains("contraseña incorrecta"), "{:?}", wrong[0].error);
        // Flip one byte in the data.
        let mut bytes = fs::read(&out).unwrap();
        let n = bytes.len();
        bytes[n - 40] ^= 1;
        let bad = d.join("danado.enc");
        fs::write(&bad, &bytes).unwrap();
        assert!(go(&[bad], &opts(1, "bien"))[0].error.is_some());
        // Cut the last chunk off.
        let cut = d.join("cortado.enc");
        fs::write(&cut, &fs::read(&out).unwrap()[..n - 300]).unwrap();
        assert!(go(&[cut], &opts(1, "bien"))[0].error.is_some(), "truncation");
        // Not an encrypted file at all.
        assert!(go(&[f.clone()], &opts(1, "bien"))[0].error.as_deref().unwrap().contains("no es un archivo cifrado"));
        // Automatic mode decrypts .enc files and encrypts the rest; conflicts are renamed.
        let auto = go(&[out.clone(), f.clone()], &opts(2, "bien"));
        assert!(auto[0].error.is_none() && !auto[0].encrypted && auto[1].encrypted, "{:?}", auto.iter().map(|r| r.error.clone()).collect::<Vec<_>>());
        assert!(d.join("secreto (copia).txt").exists(), "existing file kept, the new one renamed");
        fs::remove_dir_all(&d).unwrap();
    }
}
