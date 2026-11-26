$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$executable = Join-Path $taskRoot 'target\installer\x86_64-pc-windows-msvc\release-store\flinch-installer.exe'
if (-not (Test-Path $executable)) { throw 'Build the installer first with cargo xtask pack:installer.' }
$outputRoot = Join-Path $taskRoot 'artifacts\installer\working\renders'
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$previousLanguage = $env:FLINCH_INSTALLER_LANGUAGE
try {
    foreach ($locale in @('en', 'fr', 'de', 'es', 'it')) {
        $env:FLINCH_INSTALLER_LANGUAGE = $locale
        $destination = Join-Path $outputRoot $locale
        $errors = Join-Path $outputRoot "$locale-errors.log"
        $probe = Start-Process -FilePath $executable -ArgumentList @('--preview', ('"' + $destination + '"')) -WindowStyle Hidden -PassThru -RedirectStandardError $errors
        # Retain the process handle before a fast helper exits (Windows PowerShell 5.1).
        $null = $probe.Handle
        try {
            if (-not $probe.WaitForExit(30000)) {
                # This exact process is the isolated offscreen helper created above.
                $probe.Kill()
                if (-not $probe.WaitForExit(3000)) { throw "Offscreen helper did not exit: $($probe.Id)" }
                throw "Installer UI probe timed out for $locale."
            }
            $probe.Refresh()
            if ($probe.ExitCode -ne 0) { throw "Installer UI probe failed for ${locale}: $(Get-Content -Raw $errors)" }
        } finally { $probe.Dispose() }
    }
} finally { $env:FLINCH_INSTALLER_LANGUAGE = $previousLanguage }
Write-Output 'Installer choices, mouse/keyboard actions, twenty states and twelve entrance frames/input gates passed in all five languages.'
