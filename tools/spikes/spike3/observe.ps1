# Spike 3 observer, run on sb-target: records every change in Windows' view of the spike's
# devices and audio endpoints, then dumps the driver's own log and Windows' endpoint registry.
#   powershell -File observe.ps1 -Seconds 240 -Out C:\deploy\observe.log
param([int]$Seconds = 240, [string]$Out = 'C:\deploy\observe.log', [switch]$RestartParent)
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

function Parent { Get-PnpDevice | Where-Object { $_.HardwareID -contains 'ROOT\AudioCodec' } | Select-Object -First 1 }

function Snapshot {
    $lines = @()
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'SOUNDBOARDSPIKE*' -or $_.HardwareID -contains 'ROOT\AudioCodec' } |
        Sort-Object InstanceId | ForEach-Object {
            $lines += "  dev  [$($_.Status)] '$($_.FriendlyName)' class=$($_.Class) $($_.InstanceId)"
        }
    Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue |
        Sort-Object InstanceId | ForEach-Object {
            $lines += "  ep   [$($_.Status)] '$($_.FriendlyName)' $($_.InstanceId)"
        }
    $lines -join "`n"
}

"=== observe start $(Get-Date -Format o)" | Set-Content $Out
if ($RestartParent) {
    $p = Parent
    $key = "HKLM:\SYSTEM\CurrentControlSet\Enum\$($p.InstanceId)\Device Parameters"
    $old = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
    if ($old) { $old.PSObject.Properties | Where-Object Name -like 'Log*' | ForEach-Object { Remove-ItemProperty -Path $key -Name $_.Name } }
    "restarting parent $($p.InstanceId)" | Add-Content $Out
    pnputil /restart-device "$($p.InstanceId)" | Add-Content $Out
}
$start = Get-Date
$last = ''
while (((Get-Date) - $start).TotalSeconds -lt $Seconds) {
    $now = Snapshot
    if ($now -ne $last) {
        "--- t+{0:N0}s" -f ((Get-Date) - $start).TotalSeconds | Add-Content $Out
        $now | Add-Content $Out
        $last = $now
    }
    Start-Sleep -Milliseconds 1500
}

"=== driver log (parent's Device Parameters)" | Add-Content $Out
$p = Parent
$key = "HKLM:\SYSTEM\CurrentControlSet\Enum\$($p.InstanceId)\Device Parameters"
$props = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
if ($props) {
    $props.PSObject.Properties | Where-Object Name -like 'Log*' | Sort-Object Name |
        ForEach-Object { "  $($_.Name): $($_.Value)" } | Add-Content $Out
} else { "  (no Device Parameters key at $key)" | Add-Content $Out }

"=== spike child devices known to PnP (present or not)" | Add-Content $Out
Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SOUNDBOARDSPIKE*' } | ForEach-Object {
    $desc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_DeviceDesc -ErrorAction SilentlyContinue).Data
    $fn = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_FriendlyName -ErrorAction SilentlyContinue).Data
    "  [$($_.Status)] $($_.InstanceId) desc='$desc' friendly='$fn'"
} | Add-Content $Out

"=== MMDevices render endpoints" | Add-Content $Out
$base = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
Get-ChildItem $base -ErrorAction SilentlyContinue | ForEach-Object {
    $state = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).DeviceState
    $pp = Get-ItemProperty (Join-Path $_.PSPath 'Properties') -ErrorAction SilentlyContinue
    $desc = $pp.'{a45c254e-df1c-4efd-8020-67d146a850e0},2'
    $iface = $pp.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
    $inst = $pp.'{b3f8fa53-0004-438e-9003-51a46e139bfc},2'
    "  $($_.PSChildName) state=$state desc='$desc' iface='$iface' devinst='$inst'"
} | Add-Content $Out
"=== observe end $(Get-Date -Format o)" | Add-Content $Out
