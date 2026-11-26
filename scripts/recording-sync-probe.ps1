# Explicit opt-in playback only. Does not edit preferences or start recording.
$ErrorActionPreference = 'Stop'
$probe = Join-Path $PSScriptRoot 'recording-sync-probe.html'
if (-not (Test-Path -LiteralPath $probe)) { throw 'Recording sync probe page missing.' }
Write-Host 'Desktop synchronization probe: browser/fullscreen, colored flashes and sound.'
Write-Host 'Start Flinch recording at 1024x768 / 60 FPS with audio in the VM, then start the page.'
Write-Host 'Pause/resume at least twice between markers. Keep the resulting MP4 and Flinch log.'
Start-Process -FilePath $probe
