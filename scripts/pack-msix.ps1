param([Parameter(Mandatory)][string]$Version, [switch]$LocalTest, [switch]$StageOnly, [switch]$SkipBuild, [int]$Revision = 0)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $taskRoot
. "$PSScriptRoot/tooling.ps1"
if ($Revision -lt 0 -or $Revision -gt 65535 -or ($Revision -gt 0 -and -not $LocalTest)) { throw 'Invalid local revision' }
if ($SkipBuild -and -not $StageOnly) { throw 'Skipping builds is allowed only for staging' }
$build = Join-Path $taskRoot 'artifacts/store/working/build'
$staging = Join-Path $build 'staging'
$certificate = Join-Path $taskRoot 'artifacts/store/working/certificates/store-test.pfx'
$release = Join-Path $taskRoot 'target/x86_64-pc-windows-msvc/release-store'
if ($LocalTest -and -not $StageOnly -and -not (Test-Path -LiteralPath $certificate)) { throw 'Missing local-test certificate matching the manifest publisher' }
if (-not $StageOnly) { Invoke-Tool winapp @('--version') }
Invoke-Tool cargo @('xtask', 'check:contracts')
if (-not $SkipBuild) { Invoke-Tool cargo @('build', '--locked', '--target', 'x86_64-pc-windows-msvc', '--features', 'msix', '--profile', 'release-store') }
Invoke-Tool cargo @('xtask', 'assets:verify-executable', (Join-Path $release 'flinch.exe'))
Reset-Staging $staging 'artifacts/store/working/build/staging'
New-Item -ItemType Directory -Force "$staging/Assets", "$staging/resources", "$staging/licenses" | Out-Null
Copy-Item -LiteralPath "$release/flinch.exe" -Destination "$staging/flinch.exe"
Copy-Item -LiteralPath "$taskRoot/resources/defaultConfig.json" -Destination "$staging/resources/defaultConfig.json"
Copy-Item -LiteralPath "$taskRoot/LICENSE" -Destination "$staging/licenses/FLINCH-LICENSE.txt"
Copy-Item -LiteralPath "$taskRoot/NOTICE.md" -Destination "$staging/licenses/FLINCH-NOTICE.md"
Copy-Item -LiteralPath "$taskRoot/resources/ocr/NOTICE.md" -Destination "$staging/licenses/OCR-NOTICE.md"
Copy-Item -LiteralPath "$taskRoot/resources/ocr/LICENSE-APACHE-2.0.txt" -Destination "$staging/licenses/OCR-LICENSE-APACHE-2.0.txt"
Copy-Item -LiteralPath "$taskRoot/resources/icons/fluent/LICENSE" -Destination "$staging/licenses/FLUENT-ICONS-LICENSE.txt"
Copy-Item -LiteralPath "$taskRoot/resources/cursors/CHROMIUM-LICENSE" -Destination "$staging/licenses/CHROMIUM-CURSORS-LICENSE.txt"
$manifest = Get-Content -LiteralPath "$taskRoot/appxmanifest.xml" -Raw
if ($Revision -gt 0) { $manifest = [regex]::Replace($manifest, '(<Identity\b[^>]*\bVersion=")[^"]+("[^>]*>)', "`${1}$Version.$Revision`${2}") }
[IO.File]::WriteAllText("$staging/AppxManifest.xml", $manifest)
foreach ($name in @('Square44x44Logo.png','Square71x71Logo.png','Square150x150Logo.png','StoreLogo.png','InstallerLogo.png','InstallerLogo.scale-300.png')) { Copy-Item -LiteralPath "$taskRoot/icons/$name" -Destination "$staging/Assets/$name" }
Get-ChildItem -LiteralPath "$taskRoot/icons" -File | Where-Object { $_.Name -match '^Square(?:44x44|71x71|150x150)Logo\.(?:targetsize-|scale-).*\.png$' } | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination "$staging/Assets/$($_.Name)" }
if ($StageOnly) { Write-Host "Staged $staging. No package created; a skipped build does not verify channel identity."; return }
$suffix = if ($LocalTest) { '_local-test' } else { '' }
$packArgs = @('pack',$staging,'--manifest',"$staging/AppxManifest.xml",'--executable','flinch.exe','--output',"$build/Flinch_$Version.$($Revision)_x64$suffix.msix")
if ($LocalTest) { $packArgs += @('--cert',$certificate) }
Invoke-Tool winapp $packArgs
