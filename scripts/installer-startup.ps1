function Test-MsiMaintenancePolicy {
    param([string]$Package)
    # OpenDatabase mode 0 only reads MSI tables; it cannot install or remove files.
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    try {
        $database = $installer.OpenDatabase([IO.Path]::GetFullPath($Package), 0)
        foreach ($policy in @(
            @{name='MSIRESTARTMANAGERCONTROL'; value='0'},
            @{name='MSIRMSHUTDOWN'; value='1'},
            @{name='MSIDISABLERMRESTART'; value='1'}
        )) {
            $view = $null; $record = $null
            try {
                $query = 'SELECT `Value` FROM `Property` WHERE `Property` = ''{0}''' -f $policy.name
                $view = $database.OpenView($query)
                $view.Execute()
                $record = $view.Fetch()
                if ($null -eq $record -or $record.StringData(1) -ne $policy.value) {
                    throw "Compiled MSI maintenance policy requires $($policy.name)=$($policy.value)"
                }
            } finally {
                if ($null -ne $record) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) }
                if ($null -ne $view) { $view.Close(); [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) }
            }
        }
    } finally {
        if ($null -ne $database) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) }
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer)
    }
    Write-Output 'Compiled MSI enables file-owner shutdown and prevents automatic app restart; no installation executed.'
}

function Test-InstallerIcon {
    param([string]$Executable)
    if (-not ('FlinchInstallerResources' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class FlinchInstallerResources {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr LoadLibraryExW(string file, IntPtr reserved, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint SizeofResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LockResource(IntPtr resource);
    [DllImport("kernel32.dll")]
    static extern bool FreeLibrary(IntPtr module);
    public static byte[] Read(string path, int id, int type) {
        // DATAFILE | IMAGE_RESOURCE: map resources without executing the program.
        IntPtr module = LoadLibraryExW(path, IntPtr.Zero, 0x22);
        if (module == IntPtr.Zero) throw new Win32Exception();
        try {
            IntPtr resource = FindResourceW(module, new IntPtr(id), new IntPtr(type));
            if (resource == IntPtr.Zero) throw new Win32Exception();
            uint size = SizeofResource(module, resource);
            IntPtr data = LockResource(LoadResource(module, resource));
            if (size == 0 || data == IntPtr.Zero) throw new Win32Exception();
            byte[] bytes = new byte[checked((int)size)];
            Marshal.Copy(data, bytes, 0, bytes.Length);
            return bytes;
        } finally { FreeLibrary(module); }
    }
}
'@
    }
    $source = [IO.File]::ReadAllBytes((Join-Path $taskRoot 'icons/icon.ico'))
    $group = [FlinchInstallerResources]::Read($Executable, 1, 14)
    $count = [BitConverter]::ToUInt16($source, 4)
    if ($group.Length -ne 6 + 14 * $count -or [BitConverter]::ToUInt16($group, 2) -ne 1 -or [BitConverter]::ToUInt16($group, 4) -ne $count) { throw 'Custom installer Flinch icon directory differs' }
    for ($i = 0; $i -lt $count; $i++) {
        $entry = 6 + 16 * $i
        $compiled = 6 + 14 * $i
        for ($j = 0; $j -lt 12; $j++) {
            if ($source[$entry + $j] -ne $group[$compiled + $j]) { throw 'Custom installer Flinch icon entry differs' }
        }
        $length = [BitConverter]::ToUInt32($source, $entry + 8)
        $offset = [BitConverter]::ToUInt32($source, $entry + 12)
        $expected = New-Object byte[] $length
        [Array]::Copy($source, $offset, $expected, 0, $length)
        $actual = [FlinchInstallerResources]::Read($Executable, [BitConverter]::ToUInt16($group, $compiled + 12), 3)
        if ([Convert]::ToBase64String($actual) -ne [Convert]::ToBase64String($expected)) { throw 'Custom installer does not embed the Flinch icon' }
    }
}

function Test-BundleStartup {
    param([string]$Bundle,[string]$Directory)
    Invoke-Tool $wix @('burn','extract','-acceptEula','wix7',$Bundle,'-oba',$Directory)
    [xml]$manifest = Get-Content -LiteralPath "$Directory/manifest.xml" -Raw
    foreach ($policy in @(
        @{name='MSIRESTARTMANAGERCONTROL'; value='0'},
        @{name='MSIRMSHUTDOWN'; value='1'},
        @{name='MSIDISABLERMRESTART'; value='1'}
    )) {
        $property = $manifest.SelectSingleNode("//*[local-name()='MsiPackage' and @Id='FlinchMsi']/*[local-name()='MsiProperty' and @Id='$($policy.name)']")
        if ($null -eq $property -or $property.Value -ne $policy.value) { throw "Bundle must override cached MSI shutdown policy: $($policy.name)=$($policy.value)" }
    }
    $ux = $manifest.SelectSingleNode('//*[local-name()="UX"]')
    if (-not $ux.PrimaryPayloadId.StartsWith('WixPrereqBootstrapperApplication.Primary') -or $ux.SecondaryPayloadId -ne 'FlinchUI') { throw 'System-only prerequisite must be primary; custom UI secondary' }
    $payload = $manifest.SelectNodes('//*[local-name()="Payload"]') | Where-Object { $_.Id -eq $ux.PrimaryPayloadId } | Select-Object -First 1
    if ($payload.FilePath -ne 'wixprqba.exe') { throw 'Unexpected prerequisite executable' }
    Invoke-Tool cargo @('xtask','installer:verify-imports', (Join-Path $Directory $payload.FilePath))
    $custom = $manifest.SelectNodes('//*[local-name()="Payload"]') | Where-Object { $_.Id -eq 'FlinchUI' } | Select-Object -First 1
    if (-not $custom) { throw 'Custom installer payload is missing' }
    $customPath = Join-Path $Directory $custom.FilePath
    $metadata = [Diagnostics.FileVersionInfo]::GetVersionInfo($customPath)
    if ($metadata.ProductName -ne 'Flinch' -or $metadata.FileDescription -ne 'Flinch Setup' -or $metadata.OriginalFilename -ne 'flinch-installer.exe') { throw 'Custom installer Windows branding is missing' }
    Test-InstallerIcon $customPath
    Write-Output 'Custom installer Windows name and Flinch icon verified without launching it.'
    Write-Output 'Bundle forwards maintenance shutdown policy to cached MSI packages.'
}
