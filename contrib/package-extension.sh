#!/bin/sh
# Packages extension/ into dist/habitfocus.xpi (install via about:addons →
# "Install Add-on From File…"; Zen/unbranded builds need
# xpinstall.signatures.required = false).
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
python3 - <<'PY'
import pathlib, zipfile
src = pathlib.Path("extension")
with zipfile.ZipFile("dist/habitfocus.xpi", "w", zipfile.ZIP_DEFLATED) as xpi:
    for path in sorted(src.rglob("*")):
        if path.is_file():
            xpi.write(path, path.relative_to(src))
PY
echo "wrote dist/habitfocus.xpi"
