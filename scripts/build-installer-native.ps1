$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$toolRoot = Join-Path $taskRoot '.tools\installer'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$installation = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
if (-not $installation) { throw 'Visual Studio C++ tools are required.' }
$vcvars = Join-Path $installation 'VC\Auxiliary\Build\vcvars64.bat'
$nativeOut = Join-Path $toolRoot 'native'
New-Item -ItemType Directory -Path $nativeOut -Force | Out-Null
# cmd is required by vcvars64.bat. Reject shell metacharacters before constructing its command.
foreach ($value in @($taskRoot, $vcvars)) {
    if ($value -match '["%&|^<>!\r\n]') { throw 'Unsupported compiler path.' }
}
$compiler = 'call "' + $vcvars + '" >nul && cl /nologo '
$flags = '/MD /EHsc /std:c++17 /W4 /DUNICODE /D_UNICODE /I"' + $toolRoot + '\ba-api\build\native\include" /I"' + $toolRoot + '\wixtoolset.dutil\build\native\include" '
$libraries = ' "' + $toolRoot + '\ba-api\build\native\v14\x64\balutil.lib" "' + $toolRoot + '\wixtoolset.dutil\build\native\v14\x64\dutil.lib" user32.lib msi.lib shlwapi.lib shell32.lib ole32.lib oleaut32.lib advapi32.lib version.lib userenv.lib wininet.lib comctl32.lib bcrypt.lib crypt32.lib rpcrt4.lib uuid.lib ws2_32.lib cabinet.lib'
foreach ($build in @(
    @{source='tests\burn_bridge.cpp'; name='burn_bridge_test'; extension='exe'; option=''},
    @{source='burn_bridge.cpp'; name='flinch_burn'; extension='dll'; option='/LD '}
)) {
    $compileLine = $compiler + $build.option + $flags + '"' + $taskRoot + '\windows\installer\' + $build.source + '" /Fo"' + $nativeOut + '\' + $build.name + '.obj" /link /OUT:"' + $nativeOut + '\' + $build.name + '.' + $build.extension + '" /IMPLIB:"' + $nativeOut + '\' + $build.name + '.lib"' + $libraries
    & cmd.exe /d /s /c $compileLine
    if ($LASTEXITCODE -ne 0) { throw "Native $($build.name) compilation failed: $LASTEXITCODE" }
    if ($build.extension -eq 'exe') {
        & (Join-Path $nativeOut 'burn_bridge_test.exe')
        if ($LASTEXITCODE -ne 0) { throw "Native Burn callback regression failed: $LASTEXITCODE" }
    }
}
