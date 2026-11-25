// Windows: embeds the icon and the program's name into the .exe (shown in Explorer and the taskbar).
fn main() {
    #[cfg(windows)]
    {
        if std::path::Path::new("assets/icon.ico").exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "Ficherón");
            res.set("FileDescription", "Ficherón: administrador de archivos y utilidades");
            res.set("InternalName", "Ficherón");
            let _ = res.compile();
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
