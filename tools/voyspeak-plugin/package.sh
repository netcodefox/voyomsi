#!/bin/sh
# Packs the plugin as the ZIP VoySpeak installs (Extensions -> Plugins -> Install ZIP...).
set -e
cd "$(dirname "$0")"
out="${1:-voyomsi-voice.zip}"
rm -f "$out"
zip -q -r "$out" manifest.json dist README.md
echo "$out"
