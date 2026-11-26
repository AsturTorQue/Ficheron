//! Ficherón in the system's own right-click menu, for the current user only (no administrator):
//!
//! * Linux: scripts for Nautilus (GNOME) and Caja (MATE), actions for Nemo (Cinnamon) and a
//!   service menu for Dolphin (KDE), all under a «Ficherón» submenu.
//! * Windows: a «Ficherón» submenu for files, folders and a folder's background (registry, under
//!   HKEY_CURRENT_USER) and one «Enviar a» entry per tool, which receives every selected file.
//! * macOS: one Quick Action per tool (Finder › right click › Acciones rápidas / Servicios).
//!
//! Each entry runs `ficheron --tool <id> <paths…>`, which opens that tool on those files.

use crate::i18n::tr;
use crate::tools::Kind;
use std::fs;
use std::path::{Path, PathBuf};

/// The tools offered in the menu: (id used on the command line, tool).
pub const TOOLS: [(&str, Kind); 11] = [
    ("rename", Kind::BatchRename),
    ("compress", Kind::Compress),
    ("extract", Kind::Extract),
    ("checksum", Kind::Checksum),
    ("dupes", Kind::Duplicates),
    ("images", Kind::Images),
    ("meta", Kind::Meta),
    ("crypt", Kind::Crypt),
    ("compare", Kind::Compare),
    ("grep", Kind::Grep),
    ("integrity", Kind::Integrity),
];

pub fn kind_of(id: &str) -> Option<Kind> {
    TOOLS.iter().find(|(i, _)| *i == id).map(|(_, k)| *k)
}

fn exe() -> Result<PathBuf, String> {
    let e = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(crate::fsops::canonical(&e))
}

/// A folder name / menu entry without characters file systems or menus dislike.
fn clean(label: &str) -> String {
    label.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect()
}

/// What was done, one line each, or why it failed.
pub fn install() -> Result<Vec<String>, String> {
    let exe = exe()?;
    platform::install(&exe)
}

pub fn uninstall() -> Result<Vec<String>, String> {
    platform::uninstall()
}

pub fn installed() -> bool {
    platform::installed()
}

/// Wayland shows a window's icon from the desktop entry whose name matches the window's app id
/// (`ficheron.desktop`). Writes that entry and the icon for the current user when they are
/// missing or out of date, so the program's own icon shows without installing anything.
pub fn ensure_desktop_entry() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let (Some(data), Ok(exe)) = (dirs::data_dir(), exe()) else { return };
        let mut changed = false;
        for s in [16u32, 32, 48, 64, 128, 256, 512] {
            let p = data.join(format!("icons/hicolor/{s}x{s}/apps/ficheron.png"));
            if !p.exists() {
                if let Some(img) = image::RgbaImage::from_raw(s, s, crate::icon::rgba(s)) {
                    let _ = fs::create_dir_all(p.parent().unwrap_or(&data));
                    changed |= img.save(&p).is_ok();
                }
            }
        }
        let desktop = data.join("applications/ficheron.desktop");
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=Ficherón\nGenericName={}\nComment={}\nExec=\"{}\" %F\nIcon=ficheron\nTerminal=false\nCategories=Utility;FileTools;FileManager;\nStartupWMClass=ficheron\n",
            tr("Administrador de archivos"),
            tr("Administrador de archivos y utilidades para ficheros"),
            exe.display()
        );
        if fs::read_to_string(&desktop).ok().as_deref() != Some(text.as_str()) {
            let _ = fs::create_dir_all(desktop.parent().unwrap_or(&data));
            changed |= fs::write(&desktop, text).is_ok();
        }
        if changed {
            let _ = std::process::Command::new("update-desktop-database").arg(data.join("applications")).status();
            let _ = std::process::Command::new("gtk-update-icon-cache").args(["-q", "-t"]).arg(data.join("icons/hicolor")).status();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> Result<PathBuf, String> {
        dirs::home_dir().ok_or_else(|| tr("No se encuentra la carpeta personal").to_string())
    }

    fn script_dirs() -> Result<Vec<PathBuf>, String> {
        let h = home()?;
        Ok(vec![h.join(".local/share/nautilus/scripts/Ficherón"), h.join(".config/caja/scripts/Ficherón")])
    }

    fn nemo_dir() -> Result<PathBuf, String> {
        Ok(home()?.join(".local/share/nemo/actions"))
    }

    fn dolphin_files() -> Result<Vec<PathBuf>, String> {
        let h = home()?;
        Ok(vec![h.join(".local/share/kio/servicemenus/ficheron.desktop"), h.join(".local/share/kservices5/ServiceMenus/ficheron.desktop")])
    }

    fn write_exec(p: &Path, text: &str) -> Result<(), String> {
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        fs::write(p, text).map_err(|e| format!("{}: {e}", p.display()))?;
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
    }

    pub fn install(exe: &Path) -> Result<Vec<String>, String> {
        let exe = exe.to_string_lossy().replace('"', "\\\"");
        let mut done = vec![];
        // Nautilus and Caja: one script per tool; with nothing selected they work on the open folder.
        for dir in script_dirs()? {
            for (id, kind) in TOOLS {
                let body = format!("#!/bin/sh\n# Ficherón\n[ $# -eq 0 ] && set -- \"$PWD\"\nexec \"{exe}\" --tool {id} \"$@\"\n");
                write_exec(&dir.join(clean(kind.label())), &body)?;
            }
            done.push(dir.display().to_string());
        }
        // Nemo: one action per tool.
        let nd = nemo_dir()?;
        for (id, kind) in TOOLS {
            let body = format!(
                "[Nemo Action]\nName=Ficherón: {}\nComment={}\nExec=\"{exe}\" --tool {id} %F\nIcon-Name=ficheron\nSelection=Any\nExtensions=any;\n",
                kind.label(),
                kind.description().replace('\n', " ")
            );
            write_exec(&nd.join(format!("ficheron-{id}.nemo_action")), &body)?;
        }
        done.push(nd.display().to_string());
        // Dolphin: a service menu with a «Ficherón» submenu (Plasma 6 path and the older one).
        let mut desk = String::from("[Desktop Entry]\nType=Service\nMimeType=all/all;\nX-KDE-Submenu=Ficherón\nX-KDE-Priority=TopLevel\nActions=");
        desk.push_str(&TOOLS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(";"));
        desk.push_str(";\n");
        for (id, kind) in TOOLS {
            desk.push_str(&format!("\n[Desktop Action {id}]\nName={}\nIcon=ficheron\nExec=\"{exe}\" --tool {id} %F\n", kind.label()));
        }
        for f in dolphin_files()? {
            write_exec(&f, &desk)?;
            done.push(f.display().to_string());
        }
        Ok(done)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        let mut done = vec![];
        for dir in script_dirs()? {
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
                done.push(dir.display().to_string());
            }
        }
        for (id, _) in TOOLS {
            let f = nemo_dir()?.join(format!("ficheron-{id}.nemo_action"));
            if f.exists() {
                fs::remove_file(&f).map_err(|e| e.to_string())?;
            }
        }
        done.push(nemo_dir()?.display().to_string());
        for f in dolphin_files()? {
            if f.exists() {
                fs::remove_file(&f).map_err(|e| e.to_string())?;
                done.push(f.display().to_string());
            }
        }
        Ok(done)
    }

    pub fn installed() -> bool {
        script_dirs().map(|d| d.iter().any(|d| d.exists())).unwrap_or(false) || dolphin_files().map(|d| d.iter().any(|f| f.exists())).unwrap_or(false)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::process::Command;

    const BASES: [(&str, &str); 3] = [("*", "%1"), ("Directory", "%1"), ("Directory\\Background", "%V")];

    fn reg(args: &[&str]) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing for each key.
        let out = Command::new("reg").args(args).creation_flags(0x0800_0000).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn send_to() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("Microsoft\\Windows\\SendTo"))
    }

    pub fn install(exe: &Path) -> Result<Vec<String>, String> {
        let exe = exe.to_string_lossy().to_string();
        let mut done = vec![];
        for (base, arg) in BASES {
            let root = format!("HKCU\\Software\\Classes\\{base}\\shell\\Ficheron");
            reg(&["add", &root, "/v", "MUIVerb", "/d", "Ficherón", "/f"])?;
            reg(&["add", &root, "/v", "SubCommands", "/d", "", "/f"])?;
            reg(&["add", &root, "/v", "Icon", "/d", &exe, "/f"])?;
            for (n, (id, kind)) in TOOLS.iter().enumerate() {
                let key = format!("{root}\\shell\\{n:02}{id}");
                reg(&["add", &key, "/v", "MUIVerb", "/d", kind.label(), "/f"])?;
                reg(&["add", &format!("{key}\\command"), "/ve", "/d", &format!("\"{exe}\" --tool {id} \"{arg}\""), "/f"])?;
            }
            done.push(root);
        }
        // «Enviar a» hands every selected file to one program, which the registry menu can't.
        if let Some(dir) = send_to() {
            for (id, kind) in TOOLS {
                let f = dir.join(format!("Ficherón - {}.cmd", clean(kind.label())));
                fs::write(&f, format!("@start \"\" \"{exe}\" --tool {id} %*\r\n")).map_err(|e| format!("{}: {e}", f.display()))?;
            }
            done.push(dir.display().to_string());
        }
        Ok(done)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        let mut done = vec![];
        for (base, _) in BASES {
            let root = format!("HKCU\\Software\\Classes\\{base}\\shell\\Ficheron");
            if reg(&["delete", &root, "/f"]).is_ok() {
                done.push(root);
            }
        }
        if let Some(dir) = send_to() {
            for (_, kind) in TOOLS {
                let _ = fs::remove_file(dir.join(format!("Ficherón - {}.cmd", clean(kind.label()))));
            }
            done.push(dir.display().to_string());
        }
        Ok(done)
    }

    pub fn installed() -> bool {
        reg(&["query", "HKCU\\Software\\Classes\\*\\shell\\Ficheron"]).is_ok()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn services() -> Result<PathBuf, String> {
        dirs::home_dir().map(|h| h.join("Library/Services")).ok_or_else(|| tr("No se encuentra la carpeta personal").to_string())
    }

    fn bundle(label: &str) -> Result<PathBuf, String> {
        Ok(services()?.join(format!("Ficherón - {}.workflow", clean(label))))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    pub fn install(exe: &Path) -> Result<Vec<String>, String> {
        let mut done = vec![];
        for (id, kind) in TOOLS {
            let b = bundle(kind.label())?;
            let c = b.join("Contents");
            fs::create_dir_all(&c).map_err(|e| e.to_string())?;
            let name = xml(&format!("Ficherón: {}", kind.label()));
            let info = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>NSServices</key><array><dict>
<key>NSMenuItem</key><dict><key>default</key><string>{name}</string></dict>
<key>NSMessage</key><string>runWorkflowAsService</string>
<key>NSRequiredContext</key><dict><key>NSApplicationIdentifier</key><string>com.apple.finder</string></dict>
<key>NSSendFileTypes</key><array><string>public.item</string></array>
</dict></array>
</dict></plist>
"#
            );
            let cmd = xml(&format!("\"{}\" --tool {id} \"$@\" >/dev/null 2>&1 &", exe.to_string_lossy()));
            let wflow = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>AMApplicationBuild</key><string>523</string>
<key>AMApplicationVersion</key><string>2.10</string>
<key>AMDocumentVersion</key><string>2</string>
<key>actions</key><array><dict><key>action</key><dict>
<key>AMAccepts</key><dict><key>Container</key><string>List</string><key>Optional</key><true/><key>Types</key><array><string>com.apple.cocoa.string</string></array></dict>
<key>AMActionVersion</key><string>2.0.3</string>
<key>AMApplication</key><array><string>Automator</string></array>
<key>AMParameterProperties</key><dict><key>COMMAND_STRING</key><dict/><key>CheckedForUserDefaultShell</key><dict/><key>inputMethod</key><dict/><key>shell</key><dict/><key>source</key><dict/></dict>
<key>AMProvides</key><dict><key>Container</key><string>List</string><key>Types</key><array><string>com.apple.cocoa.string</string></array></dict>
<key>ActionBundlePath</key><string>/System/Library/Automator/Run Shell Script.action</string>
<key>ActionName</key><string>Run Shell Script</string>
<key>ActionParameters</key><dict>
<key>COMMAND_STRING</key><string>{cmd}</string>
<key>CheckedForUserDefaultShell</key><true/>
<key>inputMethod</key><integer>1</integer>
<key>shell</key><string>/bin/sh</string>
<key>source</key><string></string>
</dict>
<key>BundleIdentifier</key><string>com.apple.RunShellScript</string>
<key>CFBundleVersion</key><string>2.0.3</string>
<key>CanShowSelectedItemsWhenRun</key><false/>
<key>CanShowWhenRun</key><true/>
<key>Category</key><array><string>AMCategoryUtilities</string></array>
<key>Class Name</key><string>RunShellScriptAction</string>
<key>InputUUID</key><string>A1B2C3D4-0000-4000-8000-00000000000{n}</string>
<key>Keywords</key><array><string>Shell</string></array>
<key>OutputUUID</key><string>A1B2C3D4-0000-4000-8000-10000000000{n}</string>
<key>UUID</key><string>A1B2C3D4-0000-4000-8000-20000000000{n}</string>
<key>UnlocalizedApplications</key><array><string>Automator</string></array>
<key>arguments</key><dict/>
<key>isViewVisible</key><integer>1</integer>
<key>location</key><string>300:300</string>
</dict><key>isViewVisible</key><integer>1</integer></dict></array>
<key>connectors</key><dict/>
<key>workflowMetaData</key><dict>
<key>serviceApplicationBundleID</key><string>com.apple.finder</string>
<key>serviceApplicationPath</key><string>/System/Library/CoreServices/Finder.app</string>
<key>serviceInputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
<key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
<key>serviceProcessesInput</key><integer>0</integer>
<key>workflowTypeIdentifier</key><string>com.apple.Automator.servicesMenu</string>
</dict>
</dict></plist>
"#,
                n = TOOLS.iter().position(|(i, _)| *i == id).unwrap_or(0) % 10
            );
            fs::write(c.join("Info.plist"), info).map_err(|e| e.to_string())?;
            fs::write(c.join("document.wflow"), wflow).map_err(|e| e.to_string())?;
            done.push(b.display().to_string());
        }
        // Make Finder notice the new services.
        let _ = std::process::Command::new("/System/Library/CoreServices/pbs").arg("-update").status();
        Ok(done)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        let mut done = vec![];
        for (_, kind) in TOOLS {
            let b = bundle(kind.label())?;
            if b.exists() {
                fs::remove_dir_all(&b).map_err(|e| e.to_string())?;
                done.push(b.display().to_string());
            }
        }
        let _ = std::process::Command::new("/System/Library/CoreServices/pbs").arg("-update").status();
        Ok(done)
    }

    pub fn installed() -> bool {
        TOOLS.first().and_then(|(_, k)| bundle(k.label()).ok()).map(|b| b.exists()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_entry_maps_to_a_tool_and_names_are_safe() {
        for (id, kind) in TOOLS {
            assert_eq!(kind_of(id), Some(kind));
        }
        assert_eq!(kind_of("nada"), None);
        assert_eq!(clean("Texto: a/b"), "Texto- a-b");
    }
}
