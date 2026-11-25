#!/bin/sh
# Instala Ficherón para el usuario actual (sin permisos de administrador):
# el programa, su icono y su acceso en el menú, para que Wayland y X11 muestren el icono propio.
# Uso: ./packaging/instalar-linux.sh   (desde la carpeta del proyecto, tras `cargo build --release`)
set -e
base="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"
mkdir -p "$bin" "$base/applications"
install -m 755 target/release/ficheron "$bin/ficheron"
for s in 16 32 48 64 128 256 512; do
  mkdir -p "$base/icons/hicolor/${s}x${s}/apps"
  install -m 644 "assets/icon-$s.png" "$base/icons/hicolor/${s}x${s}/apps/ficheron.png"
done
install -m 644 packaging/ficheron.desktop "$base/applications/ficheron.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$base/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$base/icons/hicolor" || true
echo "Ficherón instalado. Búscalo en el menú de aplicaciones."
