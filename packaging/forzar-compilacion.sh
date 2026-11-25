#!/bin/sh
# Lanza la compilación de GitHub desde la terminal, sin abrir la web.
# Uso (desde la carpeta del proyecto):
#   ./packaging/forzar-compilacion.sh               compila y prueba en Linux, Windows y macOS
#   ./packaging/forzar-compilacion.sh --reemplazar   además sustituye los archivos de la release de esta versión
#
# Con la CLI de GitHub (`gh`) instalada se lanza el workflow directamente. Sin ella se sube un
# commit vacío a `main`, que también lanza la compilación (pero no puede pedir «reemplazar»).
set -e
REEMPLAZAR=false
[ "$1" = "--reemplazar" ] && REEMPLAZAR=true

if command -v gh >/dev/null 2>&1; then
  gh workflow run build.yml --ref main -f reemplazar="$REEMPLAZAR"
  echo "Compilación lanzada. Míralo con: gh run watch"
else
  if [ "$REEMPLAZAR" = true ]; then
    echo "Para sustituir la release hace falta la CLI «gh» o el botón Run workflow de la web." >&2
    exit 1
  fi
  git commit --allow-empty -m "Forzar compilación"
  git push origin main
  echo "Subido un commit vacío: GitHub empieza a compilar. Mira la pestaña Actions."
fi
