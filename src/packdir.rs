//! Move items into new folders, in several ways: all into one folder, one folder per file
//! (named after it, the way film and music libraries like it), by the common start of their
//! names (everything before a separator: «Serie - 01», «Serie - 02» → «Serie»), or in batches
//! of N («Parte 1», «Parte 2»…). Only plans here; the tool window moves and undoes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    One,
    PerFile,
    Common,
    Batches,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::One, Mode::PerFile, Mode::Common, Mode::Batches];
}

/// Where each item goes: (item, folder it moves into). Items that stay are left out.
pub fn plan(items: &[PathBuf], base: &Path, mode: Mode, name: &str, sep: &str, batch: usize) -> Vec<(PathBuf, PathBuf)> {
    let name = name.trim();
    let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_else(|| base.to_path_buf());
    let stem = |p: &Path| p.file_stem().unwrap_or_default().to_string_lossy().trim().to_string();
    let mut sorted: Vec<PathBuf> = items.to_vec();
    sorted.sort_by(|a, b| crate::rename::natural_cmp(&a.file_name().unwrap_or_default().to_string_lossy(), &b.file_name().unwrap_or_default().to_string_lossy()));
    match mode {
        Mode::One => {
            if name.is_empty() {
                return vec![];
            }
            let dir = base.join(name);
            sorted.into_iter().filter(|p| p != &dir).map(|p| (p, dir.clone())).collect()
        }
        Mode::PerFile => sorted
            .into_iter()
            .filter(|p| !p.is_dir())
            .filter_map(|p| {
                let s = stem(&p);
                (!s.is_empty()).then(|| {
                    let d = parent(&p).join(s);
                    (p, d)
                })
            })
            .collect(),
        Mode::Common => {
            let sep = sep.trim();
            if sep.is_empty() {
                return vec![];
            }
            let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
            for p in sorted {
                let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                if let Some(i) = n.find(sep) {
                    let key = n[..i].trim();
                    if !key.is_empty() {
                        groups.entry(parent(&p).join(key)).or_default().push(p);
                    }
                }
            }
            // A group of one isn't worth a folder.
            groups.into_iter().filter(|(_, v)| v.len() > 1).flat_map(|(d, v)| v.into_iter().map(move |p| (p, d.clone()))).collect()
        }
        Mode::Batches => {
            let batch = batch.max(1);
            let name = if name.is_empty() { "Parte" } else { name };
            sorted.into_iter().enumerate().map(|(i, p)| (p, base.join(format!("{name} {}", i / batch + 1)))).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[(PathBuf, PathBuf)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.file_name().unwrap().to_string_lossy().to_string(), b.file_name().unwrap().to_string_lossy().to_string())).collect()
    }

    #[test]
    fn plans() {
        let b = PathBuf::from("/x");
        let items: Vec<PathBuf> = ["Serie - 10.mkv", "Serie - 2.mkv", "Otra - 1.mkv", "suelto.txt", "Peli (2020).mp4"].iter().map(|n| b.join(n)).collect();
        let per = names(&plan(&items, &b, Mode::PerFile, "", "", 0));
        assert!(per.contains(&("Peli (2020).mp4".into(), "Peli (2020)".into())));
        let common = names(&plan(&items, &b, Mode::Common, "", "-", 0));
        assert_eq!(common, [("Serie - 2.mkv".to_string(), "Serie".to_string()), ("Serie - 10.mkv".into(), "Serie".into())], "Otra is alone, suelto has no separator");
        let lots = names(&plan(&items, &b, Mode::Batches, "Lote", "", 2));
        assert_eq!(lots.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), ["Lote 1", "Lote 1", "Lote 2", "Lote 2", "Lote 3"]);
        assert_eq!(plan(&items, &b, Mode::One, "Todo", "", 0).len(), 5);
    }
}
