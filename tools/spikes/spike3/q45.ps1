# Spike 3, Q4 + Q5 on sb-target: remove an endpoint while apps play into it, then bring it
# back with the same instance ID and see whether a per-app route still lands on it.
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$d = 'C:\deploy'
function Parent { Get-PnpDevice | Where-Object { $_.HardwareID -contains 'ROOT\AudioCodec' } | Select-Object -First 1 }
function T { '{0,6:N1}s' -f ((Get-Date) - $script:t0).TotalSeconds }

$p = Parent
$key = "HKLM:\SYSTEM\CurrentControlSet\Enum\$($p.InstanceId)\Device Parameters"
$old = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
if ($old) { $old.PSObject.Properties | Where-Object Name -like 'Log*' | ForEach-Object { Remove-ItemProperty -Path $key -Name $_.Name } }
$script:t0 = Get-Date
pnputil /restart-device "$($p.InstanceId)" | Out-Null
"$(T) parent restarted"

$ep = $null
while (-not $ep -and ((Get-Date) - $t0).TotalSeconds -lt 60) {
    $ep = Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue | Where-Object FriendlyName -like 'Speakers (Spike r1)*'
    Start-Sleep -Milliseconds 500
}
if (-not $ep) { "$(T) endpoint never appeared"; exit 1 }
$id = $ep.InstanceId -replace '^SWD\\MMDEVAPI\\', ''
"$(T) endpoint up: $id"
"$(T) Windows default output now:"; & "$d\default_output.exe" 2>&1 | ForEach-Object { "    $_" }
"$(T) pinning the default to the real speakers:"; & "$d\default_output.exe" 'High Definition' 2>&1 | Select-Object -Last 1 | ForEach-Object { "    $_" }

$p1 = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$d\player.ps1",'-Id',$id,'-Seconds','70','-Out',"$d\p1.log"
$p2 = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$d\player.ps1",'-Default','-Reopen','-SegmentS','5','-Seconds','110','-Out',"$d\p2.log"
"$(T) player 1 pid $($p1.Id) (direct), player 2 pid $($p2.Id) (default, 5 s segments)"
Start-Sleep -Seconds 4
"$(T) pointing player 2 at the endpoint:"
& "$d\app_output.exe" $p2.Id $id 2>&1 | ForEach-Object { "    $_" }
& "$d\app_output.exe" $p2.Id 2>&1 | ForEach-Object { "    now: $_" }

# r1 is removed 60 s after it was created; r2 comes back 15 s later. When it does, check
# whether Windows made it the default again, and pin the real speakers again either way.
$gone = $false; $back = $false
while (-not $p2.HasExited -and ((Get-Date) - $t0).TotalSeconds -lt 150) {
    $now = Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue | Where-Object FriendlyName -like 'Speakers (Spike*'
    if (-not $now -and -not $gone) { $gone = $true; "$(T) endpoint gone" }
    if ($now -and $gone -and -not $back) {
        $back = $true
        "$(T) endpoint back: $($now.FriendlyName) $($now.InstanceId)"
        Start-Sleep -Milliseconds 500
        "$(T) Windows default output now:"; & "$d\default_output.exe" 2>&1 | ForEach-Object { "    $_" }
        "$(T) pinning the real speakers again:"; & "$d\default_output.exe" 'High Definition' 2>&1 | Select-Object -Last 1 | ForEach-Object { "    $_" }
    }
    Start-Sleep -Milliseconds 500
}
"$(T) player 2 finished; per-app route now:"
& "$d\app_output.exe" $p2.Id 2>&1 | ForEach-Object { "    $_" }

"=== player 1 (direct)"; Get-Content "$d\p1.log"
"=== player 2 (default + per-app route)"; Get-Content "$d\p2.log"
"=== driver log"
$props = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
$props.PSObject.Properties | Where-Object Name -like 'Log*' | Sort-Object Name | ForEach-Object { "  $($_.Name): $($_.Value)" } |
    Where-Object { $_ -notmatch 'IoRegisterDeviceInterface|interface props|pin name' } |
    ForEach-Object { $_ }
"=== endpoints now"
Get-PnpDevice -Class AudioEndpoint -PresentOnly | ForEach-Object { "  [$($_.Status)] $($_.FriendlyName)  $($_.InstanceId)" }
