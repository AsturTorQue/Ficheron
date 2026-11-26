# Ficherón

Administrador de archivos y suite de utilidades para ficheros, para **Linux, Windows y macOS**.
Escrito en Rust con [egui/eframe](https://github.com/emilk/egui). Interfaz en español, inglés,
portugués y francés, con temas claro, oscuro y automático.

## Qué incluye

- **Explorador** con pestañas, favoritos, recientes, vista de lista e iconos, búsqueda (también en subcarpetas),
  copiar/cortar/pegar, papelera, panel de detalles y copia de rutas, incluidas las de red (UNC).
- **Renombrado avanzado**: marcadores, contador, expresiones regulares, comodines, modo `awk`, etiquetas de audio y EXIF, restaurar.
- **Buscar duplicados** en varias carpetas, con filtros y tamaños con unidad.
- **Sincronizar carpetas** (espejo, actualizar, bidireccional, mover…) con vista previa.
- **Comprimir** (ZIP, 7z, TAR.GZ/XZ/BZ2/ZST…) y **descomprimir**, con contraseña y un paquete por archivo.
- **Organizar por tipo o fecha**, **dividir y unir**, **borrar por patrón**, **eliminar carpetas vacías**, **destruir archivos**.
- **Buscar texto** y **buscar y reemplazar** en muchos archivos.
- **Imágenes**: convertir (PNG, JPEG, WebP, BMP, GIF, TIFF, AVIF), redimensionar, girar por EXIF.
- **Metadatos**: ver y editar el EXIF de las fotos y las etiquetas de la música en lote, y quitar metadatos o la ubicación GPS.
- **Comparar archivos**: texto lado a lado con las palabras cambiadas resaltadas, o binarios byte a byte.
- **Cifrar y descifrar** (XChaCha20-Poly1305 + Argon2id).
- **Texto**: saltos de línea, codificación, tabuladores y espacios sobrantes.
- **Crear carpetas desde una lista**, sumas de verificación, cambiar fechas y atributos, crear enlaces…

## Compilar

Hace falta [Rust](https://rustup.rs) (edición 2021 o posterior).

```sh
cargo run --release
cargo test
```

En Linux se necesitan las librerías de escritorio habituales (X11 o Wayland, y `libxkbcommon`).

### Motor gráfico

Ficherón incluye dos motores: OpenGL y wgpu (DirectX 12 / Vulkan / Metal). Prueba uno y, si no arranca, el otro.
En Windows empieza por DirectX 12, que incluye el dibujo por software de Windows y funciona en máquinas virtuales
sin aceleración 3D. Se puede forzar con la variable `FICHERON_RENDERER=glow` o `wgpu`.

### Icono y paquetes

- El icono se dibuja en código; `ficheron --export-icon assets` regenera los PNG y el `.ico`.
- Linux: `packaging/instalar-linux.sh` instala el programa, el icono y la entrada de menú para el usuario actual.
- macOS: `cargo install cargo-bundle && cargo bundle --release`.
- Windows: el icono se incrusta en el `.exe` al compilar.

## Releases automáticas

Para forzar una compilación sin tocar el código: en GitHub, pestaña **Actions › Compilar y publicar › Run workflow**.
Marcando «Sustituir los archivos de la release de esta versión» se vuelven a subir los programas aunque la release ya exista.
Desde la terminal: `./packaging/forzar-compilacion.sh` (con `--reemplazar` para sustituir la release; usa la CLI `gh` si está instalada y, si no, sube un commit vacío).

Cada subida a `main` compila y prueba el programa en Linux, Windows y macOS (Intel y Apple Silicon).
Después publica una release con los paquetes para las tres plataformas y le pone la etiqueta sola:
`vX.Y.Z` si esa versión de `Cargo.toml` aún no tiene release, y `vX.Y.Z-N` (N = número de ejecución)
si ya la tiene. No hace falta crear etiquetas a mano; para una versión «oficial» nueva, sube el número
de `version` y haz push.

## Licencia

[MIT](LICENSE)

## Menú contextual del sistema

Desde **Utilidades › Menú contextual del sistema…** (o `ficheron --install-menu` / `--uninstall-menu`) se añaden
las herramientas al clic derecho del explorador de archivos, solo para tu usuario:
Nautilus, Caja, Nemo y Dolphin en Linux; un submenú «Ficherón» y «Enviar a» en Windows; Acciones rápidas en macOS.
Cada entrada ejecuta `ficheron --tool <herramienta> <archivos…>`.

## Opciones de línea de órdenes

`ficheron [carpeta]` abre esa carpeta. `--screenshot archivo.png` guarda una captura y sale (para pruebas).
