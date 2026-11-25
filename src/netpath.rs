//! Network (UNC) form of a path that lives on a shared folder, so it can be pasted on another
//! computer: `\\server\share\dir\file` for SMB/CIFS, `server:/export/dir` for NFS.

use std::path::Path;

/// One network mount: where it is mounted and what it points to (`//server/share` or `server:/export`).
fn network_mounts() -> Vec<(String, String)> {
    let mut out = vec![];
    if let Ok(t) = std::fs::read_to_string("/proc/mounts") {
        for l in t.lines() {
            let f: Vec<&str> = l.split(' ').collect();
            if f.len() > 2 && matches!(f[2], "cifs" | "smb3" | "smbfs" | "nfs" | "nfs4") {
                out.push((f[1].replace("\\040", " "), f[0].replace("\\040", " ")));
            }
        }
    } else if cfg!(target_os = "macos") {
        if let Ok(o) = std::process::Command::new("mount").output() {
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some((src, rest)) = l.split_once(" on ") {
                    if let Some((mp, kind)) = rest.rsplit_once(" (") {
                        if kind.starts_with("smbfs") || kind.starts_with("nfs") || kind.starts_with("afpfs") {
                            out.push((mp.to_string(), src.to_string()));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Pure part of [`network_path`]; `mounts` are (mount point, source) pairs.
fn network_path_with(p: &str, mounts: &[(String, String)]) -> Option<String> {
    // Windows: extended UNC and plain UNC paths are already network paths.
    if let Some(r) = p.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{r}"));
    }
    if p.starts_with(r"\\") && !p.starts_with(r"\\?\") {
        return Some(p.to_string());
    }
    // Linux desktop (GVfs): /run/user/1000/gvfs/smb-share:server=nas,share=docs/sub
    if let Some(i) = p.find("/gvfs/smb-share:") {
        let rest = &p[i + "/gvfs/smb-share:".len()..];
        let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let (mut server, mut share) = (String::new(), String::new());
        for kv in head.split(',') {
            match kv.split_once('=') {
                Some(("server", v)) => server = v.to_string(),
                Some(("share", v)) => share = v.to_string(),
                _ => {}
            }
        }
        if !server.is_empty() {
            let tail = tail.replace('/', "\\");
            return Some(if tail.is_empty() { format!(r"\\{server}\{share}") } else { format!(r"\\{server}\{share}\{tail}") });
        }
    }
    // Mounted shares: the longest matching mount point wins.
    let best = mounts.iter().filter(|(mp, _)| p == mp || p.strip_prefix(mp.as_str()).map(|r| r.starts_with('/')).unwrap_or(false)).max_by_key(|(mp, _)| mp.len())?;
    let rest = p[best.0.len()..].trim_start_matches('/');
    let src = best.1.trim_end_matches('/');
    if let Some(s) = src.strip_prefix("//") {
        // macOS shows //user@server/share
        let s = s.split_once('@').map(|x| x.1).unwrap_or(s);
        let base = format!(r"\\{}", s.replace('/', "\\"));
        return Some(if rest.is_empty() { base } else { format!(r"{base}\{}", rest.replace('/', "\\")) });
    }
    // NFS: server:/export stays in its own notation.
    Some(if rest.is_empty() { src.to_string() } else { format!("{src}/{rest}") })
}

/// The network form of a path when it lives on a shared folder; None for local paths.
pub fn network_path(p: &Path) -> Option<String> {
    network_path_with(&p.to_string_lossy(), &network_mounts())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_from_windows_gvfs_cifs_and_nfs() {
        let m = vec![("/mnt/nas".to_string(), "//10.0.0.2/docs".to_string()), ("/mnt/nas/deep".to_string(), "//10.0.0.2/other".to_string()), ("/mnt/nfs".to_string(), "srv:/export".to_string())];
        assert_eq!(network_path_with(r"\\?\UNC\srv\share\a.txt", &m).unwrap(), r"\\srv\share\a.txt");
        assert_eq!(network_path_with(r"\\srv\share", &m).unwrap(), r"\\srv\share");
        assert_eq!(network_path_with("/run/user/1000/gvfs/smb-share:server=nas,share=docs/a/b.txt", &m).unwrap(), r"\\nas\docs\a\b.txt");
        assert_eq!(network_path_with("/run/user/1000/gvfs/smb-share:server=nas,share=docs", &m).unwrap(), r"\\nas\docs");
        assert_eq!(network_path_with("/mnt/nas/x/y.txt", &m).unwrap(), r"\\10.0.0.2\docs\x\y.txt");
        assert_eq!(network_path_with("/mnt/nas/deep/z", &m).unwrap(), r"\\10.0.0.2\other\z", "longest mount point");
        assert_eq!(network_path_with("/mnt/nas", &m).unwrap(), r"\\10.0.0.2\docs");
        assert_eq!(network_path_with("/mnt/nfs/a", &m).unwrap(), "srv:/export/a");
        assert!(network_path_with("/mnt/nasty/x", &m).is_none() && network_path_with("/home/u", &m).is_none());
    }
}
