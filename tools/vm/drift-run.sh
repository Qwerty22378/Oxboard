#!/usr/bin/env bash
# A long Interface 1 run on sb-build's desktop to watch drift: the PC's sound goes into
# VB-Cable (the controller makes CABLE Input the default), the engine reads CABLE Output and
# plays it to the HDA speakers, and a looping system sound keeps something playing. The
# engine prints every ring's fill, drift correction (ppm) and glitches every 10 s
# (SB_ENGINE_STATS); the log lands in target/vm/drift.log.
#
#   [STATS=SECONDS] [LOAD=1] tools/vm/drift-run.sh [MINUTES] [OUTPUT-NAME]
#
# Defaults: stats every 10 s, 10 minutes, "high definition". LOAD=1 keeps every core busy.
#
# Needs: VB-Cable on sb-build, dev logged in on the console, and
# `tools/vm/win-cargo.sh build --release -p sb-controller -p sb-engine-host -p sb-ui --bins --examples`.
source "$(dirname "$0")/desktop.sh"
set -euo pipefail
min=${1:-10} output=${2:-high definition} STATS=${STATS:-10}
OUT=$REPO_DIR/target/vm; mkdir -p "$OUT"
vm_ps sb-build "
  Get-Process sb-ui,sb-controller,sb-engine-host -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep 1
  New-Item -ItemType Directory -Force C:\src\drift | Out-Null
  Copy-Item $REMOTE_SRC\target\release\sb-controller.exe, $REMOTE_SRC\target\release\sb-engine-host.exe, $REMOTE_SRC\target\release\examples\ctl.exe, $REMOTE_SRC\target\release\examples\default_output.exe C:\src\drift\ -Force" >/dev/null
desk drift 'powershell.exe' "-WindowStyle Hidden -Command \"\$env:SB_ENGINE_STATS='$STATS'; Start-Process C:\src\drift\sb-controller.exe -ArgumentList '--no-config','--no-ui','--log' -RedirectStandardError C:\src\drift\drift.log -WindowStyle Hidden\"" 'C:\src\drift'
sleep 4
vm_ps sb-build "
  & C:\src\drift\ctl.exe system 'cable output' 2>&1 | Out-Null
  & C:\src\drift\ctl.exe select '$output' on 2>&1 | Out-Null
  Start-Sleep 2
  & C:\src\drift\default_output.exe | Select-Object -First 1"
desk player 'powershell.exe' "-WindowStyle Hidden -Command \"\$p = New-Object System.Media.SoundPlayer C:\Windows\Media\Alarm01.wav; \$p.PlayLooping(); Start-Sleep $((min * 60 + 30))\"" 'C:\src\drift'
if [[ ${LOAD:-} == 1 ]]; then
  # Every core busy for the whole run, as a game would keep them.
  vm_ps sb-build "1..[Environment]::ProcessorCount | ForEach-Object {
    Start-Process powershell -WindowStyle Hidden -ArgumentList '-NoProfile','-Command','\$e=(Get-Date).AddSeconds($((min * 60 + 20))); while((Get-Date) -lt \$e){[math]::Sqrt(7)|Out-Null}' }" >/dev/null
  echo "every core busy"
fi
echo "running for $min min..."
sleep $((min * 60))
vm_ps sb-build "Get-Content C:\src\drift\drift.log" > "$OUT/drift.log"
# Stopped hard: the default output stays on the cable, with the marker; a controller started
# without an engine finds the marker and gives the user's default back.
vm_ps sb-build "
  Get-Process sb-controller,sb-engine-host -ErrorAction SilentlyContinue | Stop-Process -Force
  \$p = Start-Process C:\src\drift\sb-controller.exe -ArgumentList '--no-config','--no-ui','--no-shell','--no-engine' -PassThru -WindowStyle Hidden
  Start-Sleep 3; Stop-Process -Id \$p.Id -Force
  & C:\src\drift\default_output.exe | Select-Object -First 1; exit 0" || true
grep -ac "engine " "$OUT/drift.log" || true
