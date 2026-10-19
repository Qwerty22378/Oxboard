#!/usr/bin/env bash
# U1: run the overlay spike over the fake game on sb-build's desktop, pressing the real
# hotkey and clicking through libvirt, and print both logs merged by wall-clock time.
#
#   tools/vm/overlay-spike.sh TAG "SPIKE ARGS" "GAME ARGS" [elevated]
#   tools/vm/overlay-spike.sh w32 "--shell win32" "--mode borderless --capture"
#   HKARGS=--no-allow tools/vm/overlay-spike.sh remote "--shell win32 --hotkey remote" "--mode windowed"
#
# Needs: someone logged in on sb-build's console (tools/vm/vm.sh view build) and
# `tools/vm/win-cargo.sh build --release -p sb-ui-spike --bins --examples` first.
# The sequence: game, spike, click the game, hotkey (show), Esc (hide), wait (hidden cost),
# hotkey + click outside (hide), hotkey twice (show, hide). Screenshots go to target/vm/spike/.
set -euo pipefail
tag=${1:?tag} ui=${2:?spike args} game=${3:?game args} elevated=${4:-}
OUT=$REPO_DIR/target/vm/spike; mkdir -p "$OUT"

source "$(dirname "$0")/desktop.sh"
hotkey() { keys KEY_LEFTCTRL KEY_LEFTSHIFT KEY_S; }
snap() { shot "$OUT/$tag-$1.png"; }
stop_all() { vm_ps sb-build "Get-Process sb-ui-spike,fake_game -ErrorAction SilentlyContinue | Stop-Process -Force; exit 0" >/dev/null || true; }

stop_all; sleep 1
vm_ps sb-build "
  New-Item -ItemType Directory -Force C:\src\spike | Out-Null
  Remove-Item C:\src\spike\*.log -ErrorAction SilentlyContinue
  Copy-Item $REMOTE_SRC\target\release\sb-ui-spike.exe, $REMOTE_SRC\target\release\examples\fake_game.exe C:\src\spike\ -Force" >/dev/null
desk game 'C:\src\spike\fake_game.exe' "$game --seconds 60 --log C:\src\spike\game.log" 'C:\src\spike' "$([[ -n $elevated ]] && echo Highest || echo Limited)"
sleep 2
[[ $ui == *"--hotkey remote"* ]] && desk hk 'C:\src\spike\sb-ui-spike.exe' "--hotkey-server ${HKARGS:-} --log C:\src\spike\hk.log" 'C:\src\spike'
desk ui 'C:\src\spike\sb-ui-spike.exe' "$ui --log C:\src\spike\ui.log" 'C:\src\spike'
sleep 4; click 400 300; sleep 2; snap 0-before
hotkey; sleep 1.5; snap 1-shown
sleep 4
keys KEY_ESC; sleep 1.5; snap 2-hidden
sleep 6
hotkey; sleep 2; click 300 250; sleep 1.5; snap 3-clicked-outside
hotkey; sleep 1.5; hotkey; sleep 2
stop_all
vm_ps sb-build "Get-Content -Encoding UTF8 C:\src\spike\*.log | Sort-Object { \$_.Substring(1,9) }" | tee "$OUT/$tag.log"
