# Shared bounded Windows build operations. Never installs or removes an installed app.
foreach ($module in @('Microsoft.PowerShell.Management','Microsoft.PowerShell.Utility','Microsoft.PowerShell.Security')) {
    Import-Module (Join-Path $PSHOME "Modules/$module") -ErrorAction Stop
}
function Invoke-Tool {
    param([string]$Executable, [string[]]$ToolArguments)
    # xtask owns child-tree deadlines and stream draining, including nested tools.
    & $Executable @ToolArguments | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "$Executable failed with exit $LASTEXITCODE" }
}
function Reset-Staging {
    param([string]$Path, [string]$ExpectedRelative)
    $expected = [IO.Path]::GetFullPath((Join-Path $taskRoot $ExpectedRelative))
    if ([IO.Path]::GetFullPath($Path) -ne $expected) { throw 'Unexpected generated staging path' }
    if (Test-Path -LiteralPath $expected) { Remove-Item -LiteralPath $expected -Recurse -Force }
    New-Item -ItemType Directory -Path $expected -Force | Out-Null
}
