param([ValidateSet('Build','Plan','Startup')][string]$Action, [Parameter(Mandatory)][string]$Version, [Parameter(Mandatory)][string]$ProductCode)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $taskRoot
. "$PSScriptRoot/tooling.ps1"
. "$PSScriptRoot/installer-startup.ps1"
$tools = Join-Path $taskRoot '.tools/installer'
$build = Join-Path $taskRoot 'artifacts/installer/working/build'
$target = Join-Path $taskRoot 'target/installer/x86_64-pc-windows-msvc/release-store'
$staging = Join-Path $build 'staging'
$wix = Join-Path $tools 'bin/wix.exe'
$env:DOTNET_ROLL_FORWARD = 'Major'
$env:DOTNET_CLI_HOME = Join-Path $tools 'dotnet-home'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:PSModulePath = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/Modules'
function Get-SharedRuntime {
    $source = $env:FLINCH_VC_REDIST_EXE
    if (-not $source) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $installation = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
        if ($LASTEXITCODE -ne 0 -or -not $installation) { throw 'C++ tools missing; set FLINCH_VC_REDIST_EXE' }
        $source = Get-ChildItem -LiteralPath "$installation/VC/Redist/MSVC" -Directory |
            Where-Object { $_.Name -match '^\d+(\.\d+)+$' } | Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'vc_redist.x64.exe' } | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    }
    if (-not $source -or [IO.Path]::GetFileName($source) -ne 'vc_redist.x64.exe') { throw 'Microsoft x64 vc_redist.x64.exe required' }
    Import-Module (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security')
    $signature = Get-AuthenticodeSignature -LiteralPath $source
    $info = (Get-Item -LiteralPath $source).VersionInfo
    # The Rust task validates metadata independently and has rejection tests.
    $env:FLINCH_REDIST_METADATA = @{status=[string]$signature.Status;subject=[string]$signature.SignerCertificate.Subject;version=$info.FileVersion;description=$info.FileDescription} | ConvertTo-Json -Compress
    Invoke-Tool cargo @('xtask','installer:validate-runtime')
    New-Item -ItemType Directory -Path $build -Force | Out-Null
    $output = Join-Path $build 'vc_redist.x64.exe'
    if ([IO.Path]::GetFullPath($source) -ne [IO.Path]::GetFullPath($output)) { Copy-Item -LiteralPath $source -Destination $output -Force }
    return @{file=$output;version=$info.FileVersion}
}
function Get-Extensions {
    return @('-ext',"$tools/wixtoolset.bootstrapperapplications.wixext/wixext7/WixToolset.BootstrapperApplications.wixext.dll",'-ext',"$tools/wixtoolset.util.wixext/wixext7/WixToolset.Util.wixext.dll")
}
function Build-Bundle {
    param([string]$Msi,[string]$Output,[string]$BundleSource,[string]$Product,[hashtable]$Runtime)
    $defines = @{Version=$Version;ProductCode=$Product;Icon="$taskRoot/icons/icon.ico";CustomUI="$target/flinch-installer.exe";Bridge="$tools/native/flinch_burn.dll";Msi=$Msi;VcRedist=$Runtime.file;VcVersion=$Runtime.version;Logo="$taskRoot/icons/InstallerLogo.scale-300.png";Prerequisite="$taskRoot/windows/installer/prerequisite"}
    $toolArgs = @('build','-acceptEula','wix7','-arch','x64') + (Get-Extensions)
    foreach ($key in $defines.Keys) { $toolArgs += @('-d',"$key=$($defines[$key])") }
    Invoke-Tool $wix ($toolArgs + @('-o',$Output,$BundleSource))
}
if (-not (Test-Path -LiteralPath $wix)) { throw 'Run cargo xtask installer:tools first' }
if ($Action -eq 'Startup') {
    Test-MsiMaintenancePolicy "$build/Flinch_$($Version)_x64.msi"
    Test-BundleStartup "$build/Flinch_$($Version)_x64-setup.exe" "$build/startup-check/$([guid]::NewGuid())"
    return
}
if ($Action -eq 'Plan') {
    foreach ($inputPath in @("$target/flinch-installer.exe","$build/Payload.wxs","$build/vc_redist.x64.exe")) { if (-not (Test-Path -LiteralPath $inputPath)) { throw 'Build installer first: cargo xtask pack:installer' } }
    $probe = Join-Path $taskRoot "artifacts/installer/working/plan-probe/$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $probe -Force | Out-Null
    $product = '{' + [guid]::NewGuid().ToString().ToUpperInvariant() + '}'
    $runtime = Get-SharedRuntime
    $packageSource = (Get-Content -LiteralPath 'windows/installer/Package.wxs' -Raw).Replace('886DC46E-70D6-4F71-B8D6-CC073021787A',[guid]::NewGuid().ToString().ToUpperInvariant())
    $bundleSource = (Get-Content -LiteralPath 'windows/installer/Bundle.wxs' -Raw).Replace('D9C9E484-92BB-479F-9B04-307BD6BE41DE',[guid]::NewGuid().ToString().ToUpperInvariant())
    [IO.File]::WriteAllText("$probe/Package.wxs",$packageSource)
    [IO.File]::WriteAllText("$probe/Bundle.wxs",$bundleSource)
    Invoke-Tool $wix @('build','-acceptEula','wix7','-arch','x64','-d',"Version=$Version",'-d',"ProductCode=$product",'-d',"Icon=$taskRoot/icons/icon.ico",'-o',"$probe/probe.msi","$probe/Package.wxs","$build/Payload.wxs")
    Build-Bundle "$probe/probe.msi" "$probe/probe.exe" "$probe/Bundle.wxs" $product $runtime
    Test-BundleStartup "$probe/probe.exe" "$probe/startup"
    foreach ($scope in @('user','machine')) {
        $flag = if ($scope -eq 'user') { '--check-plan' } else { '--check-plan-machine' }
        $expected = if ($scope -eq 'user') { 'PerUser' } else { 'PerMachine' }
        Invoke-Tool "$probe/probe.exe" @('-quiet','-layout',"$probe/layout",$flag,'-log',"$probe/$scope.log")
        $text = Get-Content -LiteralPath "$probe/$scope.log" -Raw
        if (-not $text.Contains('Flinch plan check: valid parent HWND; no apply.') -or $text.Contains('Apply begin') -or $text -notmatch "Planned package: FlinchMsi[^\r\n]*scope: $expected") { throw "Invalid $scope plan" }
        if ($text -notmatch 'Planned package: VisualCppRuntime[^\r\n]*execute: None[^\r\n]*cache: No') { throw 'Satisfied runtime must not execute or cache' }
        foreach ($feature in @('Start menu','Desktop')) { if (-not $text.Contains("Flinch shortcut plan: $feature absent.")) { throw "Fresh $feature shortcut must be deferred" } }
    }
    return
}
New-Item -ItemType Directory -Path $build -Force | Out-Null
Invoke-Tool powershell.exe @('-NoProfile','-ExecutionPolicy','Bypass','-File','scripts/build-installer-native.ps1')
Invoke-Tool cargo @('build','--locked','-p','flinch-installer','--features','ui','--target-dir','target/installer','--target','x86_64-pc-windows-msvc','--profile','release-store')
Invoke-Tool cargo @('build','--locked','--no-default-features','--features','native-settings,desktop-app','--target-dir','target/installer','--target','x86_64-pc-windows-msvc','--profile','release-store','-p','flinch')
Reset-Staging $staging 'artifacts/installer/working/build/staging'
New-Item -ItemType Directory -Force "$staging/resources", "$staging/licenses" | Out-Null
Copy-Item -LiteralPath "$target/flinch.exe" -Destination "$staging/flinch.exe"
Copy-Item -LiteralPath "$taskRoot/resources/defaultConfig.json" -Destination "$staging/resources/defaultConfig.json"
foreach ($name in @('NOTICE.md','WiX-LICENSE.txt')) { Copy-Item -LiteralPath "$taskRoot/windows/installer/$name" -Destination "$staging/licenses/$name" }
Copy-Item -LiteralPath "$taskRoot/LICENSE" -Destination "$staging/licenses/FLINCH-LICENSE.txt"
Copy-Item -LiteralPath "$taskRoot/NOTICE.md" -Destination "$staging/licenses/FLINCH-NOTICE.md"
Copy-Item -LiteralPath "$taskRoot/resources/ocr/NOTICE.md" -Destination "$staging/licenses/OCR-NOTICE.md"
Copy-Item -LiteralPath "$taskRoot/resources/ocr/LICENSE-APACHE-2.0.txt" -Destination "$staging/licenses/OCR-LICENSE-APACHE-2.0.txt"
Copy-Item -LiteralPath "$taskRoot/resources/icons/fluent/LICENSE" -Destination "$staging/licenses/FLUENT-ICONS-LICENSE.txt"
Copy-Item -LiteralPath "$taskRoot/resources/cursors/CHROMIUM-LICENSE" -Destination "$staging/licenses/CHROMIUM-CURSORS-LICENSE.txt"
function Escape-Xml([string]$Value) { return [Security.SecurityElement]::Escape($Value) }
function Get-ComponentId([string]$Name,[string]$Prefix) {
    $hash = [Security.Cryptography.SHA256]::Create()
    try { $hex = [BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes($Name))).Replace('-','').ToLowerInvariant(); return $Prefix + $hex.Substring(0,16) } finally { $hash.Dispose() }
}
$components = @()
foreach ($file in (Get-ChildItem -LiteralPath $staging -File)) {
    $id = Get-ComponentId $file.Name 'File_'
    $source = Escape-Xml $file.FullName
    $components += "<Component Id=`"$id`" Directory=`"INSTALLFOLDER`" Guid=`"*`"><File Id=`"$($id)_payload`" Source=`"$source`" KeyPath=`"yes`" /></Component>"
}
$source = Escape-Xml "$staging/resources/defaultConfig.json"
$components += "<Component Id=`"DefaultConfig`" Directory=`"RESOURCES`" Guid=`"*`"><File Source=`"$source`" KeyPath=`"yes`" /></Component>"
foreach ($file in (Get-ChildItem -LiteralPath "$staging/licenses" -File)) {
    $id = Get-ComponentId $file.Name 'Notice_'; $source = Escape-Xml $file.FullName
    $components += "<Component Id=`"$id`" Directory=`"LICENSES`" Guid=`"*`"><File Source=`"$source`" KeyPath=`"yes`" /></Component>"
}
[IO.File]::WriteAllText("$build/Payload.wxs",'<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs"><Fragment><ComponentGroup Id="Payload">' + ($components -join "`n") + '</ComponentGroup></Fragment></Wix>')
$msi = "$build/Flinch_$($Version)_x64.msi"
Invoke-Tool $wix @('build','-acceptEula','wix7','-arch','x64','-d',"Version=$Version",'-d',"ProductCode=$ProductCode",'-d',"Icon=$taskRoot/icons/icon.ico",'-o',$msi,'windows/installer/Package.wxs',"$build/Payload.wxs")
Test-MsiMaintenancePolicy $msi
$runtime = Get-SharedRuntime
Copy-Item -LiteralPath "$tools/native/flinch_burn.dll" -Destination "$target/flinch_burn.dll" -Force
$output = "$build/Flinch_$($Version)_x64-setup.exe"
Build-Bundle $msi $output 'windows/installer/Bundle.wxs' $ProductCode $runtime
Test-BundleStartup $output "$build/startup-check/$([guid]::NewGuid())"
[IO.File]::WriteAllText("$output.sha256", (Get-FileHash -LiteralPath $output -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + [IO.Path]::GetFileName($output) + "`n")
# Attached to the GitHub release; installed builds read the latest release's copy for update checks.
[IO.File]::WriteAllText("$build/version.json", '{"version":"' + $Version + '"}' + "`n")
Write-Host "Created $output and $build/version.json"
