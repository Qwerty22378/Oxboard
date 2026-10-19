#!/usr/bin/env bash
# Driving sb-build's desktop from Linux: start programs in dev's session, press keys, click and
# drag through the VM's USB tablet, take screenshots. Needs dev logged in on the console
# (tools/vm/vm.sh view build). Source it, or run one command:
#
#   tools/vm/desktop.sh start NAME EXE "ARGS" [DIR]    a program in dev's session
#   tools/vm/desktop.sh keys KEY_LEFTCTRL KEY_S         (Linux key names, held together)
#   tools/vm/desktop.sh click X Y | drag X1 Y1 X2 Y2 [STEPS] | shot FILE.png
#   tools/vm/desktop.sh ps 'POWERSHELL'                 run a snippet over SSH
#
# Screen coordinates are pixels of the 1920x1440 console.
source "$(dirname "${BASH_SOURCE[0]}")/config.sh"

# Start EXE ARGS in dev's desktop session: a one-shot scheduled task (SSH has no desktop).
desk() {
  local name=$1 exe=$2 args=$3 dir=${4:-C:\\src} level=${5:-Limited}
  vm_ps sb-build "
    \$a = New-ScheduledTaskAction -Execute '$exe' -Argument '$args' -WorkingDirectory '$dir'
    \$p = New-ScheduledTaskPrincipal -UserId $WIN_USER -LogonType Interactive -RunLevel $level
    Register-ScheduledTask -TaskName 'sb-desk-$name' -Action \$a -Principal \$p -Force | Out-Null
    Start-ScheduledTask -TaskName 'sb-desk-$name'; Start-Sleep -Milliseconds 300
    Unregister-ScheduledTask -TaskName 'sb-desk-$name' -Confirm:\$false" >/dev/null
}
keys() { virsh send-key sb-build --codeset linux "$@" >/dev/null; }
shot() { virsh screenshot sb-build "$1" >/dev/null 2>&1 || true; }
qmp() { virsh qemu-monitor-command sb-build "$1" >/dev/null; }
_ev='{"execute":"input-send-event","arguments":{"events":['
# Move the tablet to screen pixel X Y (its axes run 0..32767).
pointer() {
  qmp "$_ev"'{"type":"abs","data":{"axis":"x","value":'$(($1 * 32767 / 1920))'}},{"type":"abs","data":{"axis":"y","value":'$(($2 * 32767 / 1440))'}}]}}'
}
button() { qmp "$_ev"'{"type":"btn","data":{"down":'$1',"button":"'${2:-left}'"}}]}}'; }
click() { pointer "$1" "$2"; sleep 0.2; button true "${3:-left}"; sleep 0.1; button false "${3:-left}"; }
# Press at X1 Y1, move to X2 Y2 in STEPS steps, release.
drag() {
  local x1=$1 y1=$2 x2=$3 y2=$4 n=${5:-20} i
  pointer "$x1" "$y1"; sleep 0.15; button true; sleep 0.05
  for ((i = 1; i <= n; i++)); do pointer $((x1 + (x2 - x1) * i / n)) $((y1 + (y2 - y1) * i / n)); sleep 0.01; done
  sleep 0.05; button false
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  cmd=${1:?command}; shift
  case $cmd in
    start) desk "$@" ;;
    keys | click | drag | shot) "$cmd" "$@" ;;
    ps) vm_ps sb-build "$1" ;;
    *) die "unknown command $cmd" ;;
  esac
fi
