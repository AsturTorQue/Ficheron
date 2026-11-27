use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
    pub readonly: bool,
}

impl Entry {
    pub fn from_path(path: &Path) -> Option<Entry> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let lmeta = fs::symlink_metadata(path).ok()?;
        let is_link = lmeta.file_type().is_symlink();
        let meta = if is_link { fs::metadata(path).unwrap_or(lmeta) } else { lmeta };
        Some(Entry {
            hidden: name.starts_with('.'),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            readonly: meta.permissions().readonly(),
            is_link,
            path: path.to_path_buf(),
            name,
        })
    }

    pub fn kind(&self) -> String {
        if self.is_dir {
            return "Carpeta".into();
        }
        match self.ext().as_str() {
            "" => "Archivo".into(),
            "txt" | "md" | "log" => "Documento de texto".into(),
            "pdf" => "Documento PDF".into(),
            "doc" | "docx" | "odt" | "rtf" => "Documento".into(),
            "xls" | "xlsx" | "ods" | "csv" => "Hoja de cálculo".into(),
            "ppt" | "pptx" | "odp" => "Presentación".into(),
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tiff" => "Imagen".into(),
            "mp3" | "wav" | "ogg" | "flac" | "m4a" | "aac" | "opus" => "Audio".into(),
            "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" => "Vídeo".into(),
            "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" | "zst" => "Archivo comprimido".into(),
            "rs" | "py" | "js" | "ts" | "c" | "cpp" | "h" | "java" | "go" | "sh" | "html" | "css" | "json" | "toml" | "yaml" | "yml" | "xml" => "Código".into(),
            "exe" | "msi" | "appimage" | "deb" | "rpm" | "flatpak" => "Programa".into(),
            e => format!("Archivo {}", e.to_uppercase()),
        }
    }

    /// The kind, in the chosen language.
    pub fn kind_label(&self) -> String {
        let k = self.kind();
        match k.strip_prefix("Archivo ") {
            Some(ext) if ext.chars().all(|c| !c.is_lowercase()) => crate::i18n::fill(crate::i18n::tr("Archivo {}"), &[&ext]),
            _ => crate::i18n::tr(&k).to_string(),
        }
    }

    pub fn ext(&self) -> String {
        if self.is_dir {
            return String::new();
        }
        Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }

    pub fn icon(&self) -> &'static str {
        if self.is_dir {
            return "📁";
        }
        match self.kind().as_str() {
            "Imagen" => "🖼",
            "Audio" => "🎵",
            "Vídeo" => "🎞",
            "Archivo comprimido" => "📦",
            "Código" => "📜",
            "Programa" => "⚙",
            "Documento PDF" | "Documento" | "Documento de texto" => "📄",
            "Hoja de cálculo" => "📊",
            _ => "🗋",
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind().as_str(), "Documento de texto" | "Código")
            || matches!(self.ext().as_str(), "csv" | "ini" | "conf" | "cfg")
    }
}

pub fn list_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        if let Some(e) = Entry::from_path(&item.path()) {
            out.push(e);
        }
    }
    Ok(out)
}

pub fn search(dir: &Path, query: &str, show_hidden: bool, limit: usize) -> Vec<Entry> {
    let q = query.to_lowercase();
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        visited += 1;
        if visited > 20_000 || out.len() >= limit {
            break;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let ft = item.file_type().ok();
            if name.to_lowercase().contains(&q) {
                if let Some(e) = Entry::from_path(&item.path()) {
                    out.push(e);
                }
            }
            if ft.map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(item.path());
            }
        }
    }
    out
}

pub fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        if dst.starts_with(src) {
            return Err(io::Error::other(crate::i18n::tr("no se puede copiar una carpeta dentro de sí misma")));
        }
        fs::create_dir_all(dst)?;
        for item in fs::read_dir(src)? {
            let item = item?;
            copy_recursive(&item.path(), &dst.join(item.file_name()))?;
        }
    } else if meta.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)?;
        }
        #[cfg(not(unix))]
        {
            fs::copy(src, dst)?;
        }
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_recursive(src, dst)?;
    remove_permanently(src)
}

pub fn remove_permanently(p: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

/// A name in `dir` that doesn't exist yet, based on `name` ("foto (copia).png", "foto (copia 2).png"...).
pub fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 1.. {
        let n = if i == 1 { format!("{stem} (copia){ext}") } else { format!("{stem} (copia {i}){ext}") };
        let c = dir.join(n);
        if !c.exists() {
            return c;
        }
    }
    unreachable!()
}

pub fn dir_size(p: &Path, budget: &mut usize) -> (u64, usize) {
    let mut total = 0;
    let mut count = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for item in rd.flatten() {
            if *budget == 0 {
                return (total, count);
            }
            *budget -= 1;
            count += 1;
            if let Ok(m) = item.metadata() {
                if m.is_dir() {
                    stack.push(item.path());
                } else {
                    total += m.len();
                }
            }
        }
    }
    (total, count)
}

/// Windows "verbatim" paths (`\\?\C:\dir`, `\\?\UNC\server\share`) written the way people
/// know them (`C:\dir`, `\\server\share`). Other paths are returned unchanged.
pub fn strip_verbatim(s: &str) -> String {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        // Only drive-letter paths: other verbatim forms (`\\?\Volume{…}`) have no plain spelling.
        let b = r.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            r.to_string()
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    }
}

/// `fs::canonicalize` without the `\\?\` prefix Windows adds; the path itself if it can't be resolved.
pub fn canonical(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if cfg!(windows) {
        PathBuf::from(strip_verbatim(&c.to_string_lossy()))
    } else {
        c
    }
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%d/%m/%Y %H:%M").to_string()
        }
        None => "—".into(),
    }
}

pub fn open_with_system(p: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(p).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(p).spawn();
    r.map(|_| ())
}

pub fn preview_text(p: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = fs::File::open(p).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.contains(&0) {
        return None;
    }
    let s = String::from_utf8_lossy(&buf).to_string();
    Some(s.lines().take(40).collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod verbatim_tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_removed() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\ana"), r"C:\Users\ana");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\a"), r"\\srv\share\a");
        assert_eq!(strip_verbatim(r"\\?\Volume{1234}\x"), r"\\?\Volume{1234}\x");
        assert_eq!(strip_verbatim(r"C:\ya\normal"), r"C:\ya\normal");
        assert_eq!(strip_verbatim("/home/ana"), "/home/ana");
    }
}
