#!/usr/bin/env bash
set -euo pipefail
mkdir -p ui-fixture native-shots
cat > ui-fixture/release-notes.txt <<'TEXT'
Quarry fixture
The first needle is in this matching line.
This line is ordinary context.
An ignored dependency folder must never be searched.
The second needle is here for keyboard navigation.
TEXT
export DISPLAY=:99
export VK_ICD_FILENAMES
VK_ICD_FILENAMES=$(find /usr/share/vulkan/icd.d -name 'lvp*.json' -print -quit)
export XDG_CONFIG_HOME="$PWD/native-shots/config"
export QUARRY_SMOKE_ROOT="$PWD/ui-fixture"
Xvfb :99 -screen 0 1366x900x24 >native-shots/xvfb.log 2>&1 &
xvfb_pid=$!
app_pid=""
cleanup() { [ -z "$app_pid" ] || kill "$app_pid" 2>/dev/null || true; kill "$xvfb_pid" 2>/dev/null || true; }
trap cleanup EXIT
sleep 2
for variant in dark light phosphor; do
  export FERRITE_APPEARANCE=dark FERRITE_SCHEME=ferrite
  [ "$variant" != light ] || export FERRITE_APPEARANCE=light
  [ "$variant" != phosphor ] || export FERRITE_SCHEME=phosphor
  target/debug/ferrite-quarry >"native-shots/$variant.log" 2>&1 &
  app_pid=$!
  window=""
  for attempt in $(seq 1 30); do
    kill -0 "$app_pid" || { cat "native-shots/$variant.log"; exit 1; }
    window=$(xdotool search --name '^Quarry$' 2>/dev/null | head -1 || true)
    [ -z "$window" ] || break
    sleep 1
  done
  [ -n "$window" ] || { cat "native-shots/$variant.log"; exit 1; }
  sleep 4
  xdotool key --window "$window" ctrl+j
  sleep 1
  import -window "$window" "native-shots/$variant.png"
  kill "$app_pid"
  wait "$app_pid" || true
  app_pid=""
done
