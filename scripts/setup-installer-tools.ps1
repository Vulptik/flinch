$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$toolRoot = Join-Path $taskRoot '.tools\installer'
New-Item -ItemType Directory -Path $toolRoot -Force | Out-Null
$env:DOTNET_CLI_HOME = Join-Path $toolRoot 'dotnet-home'
$env:NUGET_PACKAGES = Join-Path $toolRoot 'packages'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_SKIP_FIRST_TIME_EXPERIENCE = '1'
$env:DOTNET_ROLL_FORWARD = 'Major'
if (-not (Test-Path (Join-Path $toolRoot 'bin\wix.exe'))) {
    & dotnet tool install wix --tool-path (Join-Path $toolRoot 'bin') --version 7.0.0 --allow-roll-forward
    if ($LASTEXITCODE -ne 0) { throw 'WiX tool installation failed.' }
}
foreach ($entry in @(
    @('wixtoolset.bootstrapperapplicationapi', 'ba-api'),
    @('wixtoolset.dutil', 'wixtoolset.dutil'),
    @('wixtoolset.bootstrapperapplications.wixext', 'wixtoolset.bootstrapperapplications.wixext'),
    @('wixtoolset.util.wixext', 'wixtoolset.util.wixext')
)) {
    $package = $entry[0]
    $destination = Join-Path $toolRoot $entry[1]
    if (Test-Path (Join-Path $destination "$package.nuspec")) { continue }
    $archive = Join-Path $toolRoot "$package.zip"
    Invoke-WebRequest "https://api.nuget.org/v3-flatcontainer/$package/7.0.0/$package.7.0.0.nupkg" -OutFile $archive -TimeoutSec 60
    Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
}
Write-Output 'Pinned WiX 7.0.0 tools are ready. No application or runtime was installed.'
