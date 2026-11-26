@echo off
rem Flinch vendor encoder campaign (NVENC / Quick Sync). Built by `cargo xtask pack:vendor-kit`.
rem Runs on the NVIDIA or Intel machine itself, from this folder, in a normal desktop session.
rem Usage: run-vendor-kit.bat [long]   ("long" adds a ten-minute recording)
rem Every result lands in a new results-* folder next to this script; send that folder back.
rem Keep the display awake for the whole run: Desktop Duplication delivers no frames while the
rem display is off, and recordings then fail with "No valid capture frame arrived".
setlocal
cd /d "%~dp0"
set "OUT=%~dp0results-%RANDOM%%RANDOM%"
mkdir "%OUT%"
set "FLINCH_VENDOR_REPORT_DIR=%OUT%\offscreen"

echo [1/7] Capabilities
flinch-video-lab.exe doctor > "%OUT%\doctor.json" 2>&1

echo [2/7] Offscreen encoder acceptance (6 sizes, rates and rate controls)
engine-tests.exe windows::vendor_acceptance::vendor_encoder_offscreen_acceptance --ignored --exact --nocapture --test-threads=1 > "%OUT%\offscreen.log" 2>&1
echo        exit %ERRORLEVEL% >> "%OUT%\offscreen.log"
echo        Smallest recordable areas (offscreen)
set "FLINCH_VENDOR_REPORT_DIR=%OUT%\smallest"
engine-tests.exe windows::vendor_acceptance::smallest_area_probe --ignored --exact --nocapture --test-threads=1 > "%OUT%\smallest.log" 2>&1
echo        exit %ERRORLEVEL% >> "%OUT%\smallest.log"

echo [3/7] Desktop recording, automatic encoder, 60 FPS with audio (10 s)
call :record auto-60 --duration 10 --fps 60 --audio

echo [4/7] Desktop recordings at 30 and 120 FPS
call :record auto-30 --duration 8 --fps 30
call :record auto-120 --duration 8 --fps 120

echo [5/7] Area recording 1366x768 (not 16-aligned)
call :record area-1366 --duration 8 --fps 60 --region "100,100,1366,768"

echo [6/7] A/V sync with the fixture (keep the fixture window visible, do not touch the mouse)
start "" /b flinch-video-lab.exe fixture 26 "%OUT%\sync-fixture.json" > "%OUT%\sync-fixture.log" 2>&1
ping -n 4 127.0.0.1 >nul
flinch-video-lab.exe record --output "%OUT%\sync.mp4" --duration 20 --fps 60 --audio --capture auto --no-cursor --trace-frames > "%OUT%\sync.log" 2>&1
ping -n 8 127.0.0.1 >nul
flinch-video-lab.exe verify "%OUT%\sync.mp4" --markers --fixture "%OUT%\sync-fixture.json" > "%OUT%\sync-verify.json" 2>&1

if /i "%~1"=="long" (
  echo [7/7] Ten-minute recording at 60 FPS with audio
  call :record long-600 --duration 600 --fps 60 --audio
) else (
  echo [7/7] Skipped the ten-minute recording; run with "long" to include it
)

echo Done. Results: %OUT%
exit /b 0

:record
set "NAME=%~1"
shift
flinch-video-lab.exe record --output "%OUT%\%NAME%.mp4" --capture auto --encoder auto %1 %2 %3 %4 %5 %6 %7 %8 > "%OUT%\%NAME%.log" 2>&1
flinch-video-lab.exe verify "%OUT%\%NAME%.mp4" > "%OUT%\%NAME%-verify.json" 2>&1
exit /b 0
