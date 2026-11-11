# Run in an elevated PowerShell AFTER pairing the Galaxy Buds 4 Pro with Windows (LE Audio off).
# Read-only: it only queries state. Output: tools\diag\diag-<timestamp>.txt
#Requires -RunAsAdministrator
$ErrorActionPreference = 'Continue'
$out = Join-Path $PSScriptRoot ("diag-{0:yyyyMMdd-HHmmss}.txt" -f (Get-Date))
& {
    "== OS"; Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' | Select ProductName, DisplayVersion, CurrentBuild, UBR | Format-List
    "== Secure Boot"; try { Confirm-SecureBootUEFI } catch { "error: $_" }
    "== BitLocker"; Get-BitLockerVolume | Select MountPoint, ProtectionStatus, VolumeStatus, KeyProtector | Format-List
    "== Boot config (testsigning etc.)"; bcdedit /enum '{current}'
    "== VBS / HVCI (memory integrity)"
    Get-CimInstance -ClassName Win32_DeviceGuard -Namespace root\Microsoft\Windows\DeviceGuard |
        Select VirtualizationBasedSecurityStatus, SecurityServicesConfigured, SecurityServicesRunning | Format-List
    "== Bluetooth radio"
    Get-PnpDevice -Class Bluetooth | Where-Object { $_.InstanceId -match '^(USB|PCI)' } | ForEach-Object {
        $_ | Select FriendlyName, Status, InstanceId | Format-List
        Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_DriverVersion, DEVPKEY_Device_DriverProvider, DEVPKEY_Device_Service |
            Select KeyName, Data | Format-Table -AutoSize | Out-String -Width 300
    }
    "== Bluetooth LE Audio / A2DP related services"
    Get-Service | Where-Object { $_.Name -match '^(Bth|BTAG|bthserv|BluetoothUserService|Bth.*)' } | Select Name, Status, StartType | Format-Table -AutoSize
    "== Every PnP node mentioning Buds or Samsung, plus BTHENUM / BTHLEDevice children"
    Get-PnpDevice | Where-Object {
        $_.FriendlyName -match 'Buds|Samsung' -or $_.InstanceId -match '^(BTHENUM|BTHLEDEVICE|BTHLE|SWD\\MMDEVAPI)'
    } | ForEach-Object {
        "---- $($_.FriendlyName) [$($_.Class)] $($_.Status)"
        "InstanceId: $($_.InstanceId)"
        Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_HardwareIds, DEVPKEY_Device_CompatibleIds, DEVPKEY_Device_Service, DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_Parent |
            ForEach-Object { "  {0}: {1}" -f $_.KeyName, (($_.Data) -join ' | ') }
    }
    "== Audio endpoints"
    Get-PnpDevice -Class AudioEndpoint | Select FriendlyName, Status, InstanceId | Format-Table -AutoSize | Out-String -Width 300
    "== WDK / tools"
    Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Include" -ErrorAction SilentlyContinue | Select -Expand Name
    Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Tools" -Recurse -Filter btvs.exe -ErrorAction SilentlyContinue | Select -Expand FullName
} *>&1 | Out-File -FilePath $out -Encoding utf8 -Width 400
"Wrote $out"
