foreach ($id in 'SOUNDBOARDSPIKE\ENDPOINT\1&49A4548&0&2', 'ROOT\DEVGEN\{D2B70EE2-741E-1048-B285-18B269A877FE}') {
  "== $id"
  Get-PnpDeviceProperty -InstanceId $id | Where-Object { $_.KeyName -match 'Class|Container|Driver|Problem|InstallState|Service|DeviceDesc|IsPresent|Parent|Capabilities|ConfigFlags|Enumerator|HardwareIds|Stack' } |
    ForEach-Object { '  {0} = {1}' -f $_.KeyName, ($_.Data -join ',') }
}
'== audio logs (last 20 min)'
Get-WinEvent -ListLog *Audio* -ErrorAction SilentlyContinue | Where-Object { $_.RecordCount -gt 0 } | ForEach-Object {
  $log = $_.LogName
  Get-WinEvent -LogName $log -MaxEvents 40 -ErrorAction SilentlyContinue | Where-Object { $_.TimeCreated -gt (Get-Date).AddMinutes(-20) } |
    ForEach-Object { '  [{0}] {1:HH:mm:ss} {2} {3}' -f $log, $_.TimeCreated, $_.Id, (($_.Message -split "`n")[0]) }
}
'== System log mentioning the spike (last 20 min)'
Get-WinEvent -LogName System -MaxEvents 300 | Where-Object { $_.TimeCreated -gt (Get-Date).AddMinutes(-20) -and $_.Message -match 'Spike|SOUNDBOARDSPIKE' } |
  Select-Object -First 12 | ForEach-Object { '  {0:HH:mm:ss} {1} {2}: {3}' -f $_.TimeCreated, $_.ProviderName, $_.Id, ((($_.Message -split "`n")[0..3]) -join ' | ') }
