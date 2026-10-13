# Runs at first logon (from the unattend CD). Gets the VM to "reachable over SSH with key auth";
# everything else is done by provision/*.ps1 over SSH. Safe to re-run: if the OpenSSH install is
# left pending a reboot, it schedules itself to run again at startup (as SYSTEM) and reboots.
$ErrorActionPreference = 'Stop'
Start-Transcript C:\sb-firstlogon.log -Append

# Keep our own copy: the unattend CD is ejected later, and the startup re-run needs these files.
$home_ = 'C:\Windows\Setup\sb'
$src = Split-Path -Parent $PSCommandPath
if ($src -ne $home_) {
    New-Item -ItemType Directory -Force $home_ | Out-Null
    Copy-Item (Join-Path $src 'firstlogon.ps1'), (Join-Path $src 'authorized_keys') $home_ -Force
}
$task = 'sb-firstlogon-resume'

# 1. virtio guest tools (storage/net/balloon/qemu-ga + display) from the virtio-win ISO.
$tools = Get-PSDrive -PSProvider FileSystem | ForEach-Object { Join-Path $_.Root 'virtio-win-guest-tools.exe' } |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if ($tools -and -not (Get-Service QEMU-GA -ErrorAction SilentlyContinue)) {
    # Pre-trust the Red Hat publisher so the silent install can't stall on a "trust this publisher?" prompt.
    $cert = Join-Path (Split-Path $tools) 'cert\Virtio_Win_Red_Hat_CA.cer'
    if (Test-Path $cert) { certutil -addstore -f TrustedPublisher $cert | Out-Null }
    # Not critical (network drivers are already in from setup), so never let it block SSH setup.
    $p = Start-Process $tools -ArgumentList '/install', '/quiet', '/norestart' -PassThru
    if (-not $p.WaitForExit(600000)) { Write-Warning "guest tools install still running after 10 min, continuing" }
}

# 2. Wait for network, mark it Private (firewall profile for SSH/KDNET rules).
$deadline = (Get-Date).AddMinutes(10)
while (-not (Test-Connection 1.1.1.1 -Count 1 -Quiet)) {
    if ((Get-Date) -gt $deadline) { throw "no network after 10 min" }
    Start-Sleep 5
}
Get-NetConnectionProfile | Set-NetConnectionProfile -NetworkCategory Private

# 3. OpenSSH server (a Windows feature-on-demand, downloaded from Windows Update).
$cap = 'OpenSSH.Server~~~~0.0.1.0'
for ($i = 1; $i -le 5 -and (Get-WindowsCapability -Online -Name $cap).State -ne 'Installed'; $i++) {
    try { Add-WindowsCapability -Online -Name $cap | Out-Null }
    catch { Write-Warning "Add-WindowsCapability attempt $i failed: $_" }
    $state = (Get-WindowsCapability -Online -Name $cap).State
    if ($state -eq 'InstallPending') {
        # Windows servicing was busy (e.g. updates); the install finishes on reboot. Resume after it.
        Write-Warning "OpenSSH install pending a reboot; rebooting and resuming at startup"
        $action = New-ScheduledTaskAction -Execute powershell.exe `
            -Argument "-NoProfile -ExecutionPolicy Bypass -File $home_\firstlogon.ps1"
        Register-ScheduledTask $task -Action $action -Trigger (New-ScheduledTaskTrigger -AtStartup) `
            -User SYSTEM -RunLevel Highest -Force | Out-Null
        Stop-Transcript
        Restart-Computer -Force
        exit
    }
    if ($state -ne 'Installed') { Start-Sleep 30 }
}
if ((Get-WindowsCapability -Online -Name $cap).State -ne 'Installed') { throw "OpenSSH server failed to install" }

Set-Service sshd -StartupType Automatic
New-ItemProperty HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -PropertyType String -Force `
    -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" | Out-Null

# Admin accounts read keys from administrators_authorized_keys, which must be ACL'd to Administrators + SYSTEM only.
$keys = "$env:ProgramData\ssh\administrators_authorized_keys"
New-Item -ItemType Directory -Force (Split-Path $keys) | Out-Null
Copy-Item "$home_\authorized_keys" $keys -Force
icacls $keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
if (-not (Get-NetFirewallRule -Name OpenSSH-Server-In-TCP -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name OpenSSH-Server-In-TCP -DisplayName 'OpenSSH Server (sshd)' -Protocol TCP -LocalPort 22 -Action Allow -Direction Inbound | Out-Null
}
Start-Service sshd

# 4. Never sleep; no hibernation (would also make the disk unsafe to touch from the host).
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /hibernate off

Unregister-ScheduledTask $task -Confirm:$false -ErrorAction SilentlyContinue
New-Item C:\sb-firstlogon.done -ItemType File -Force | Out-Null
Stop-Transcript
