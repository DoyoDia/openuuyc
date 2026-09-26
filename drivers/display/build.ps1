param([ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$SigningThumbprint, [switch]$Stage)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$outputDir = Join-Path $repoRoot 'target/display-driver'
New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vsRoot) { throw 'Visual C++ build tools are required.' }
$vcVersion = (Get-Content (Join-Path $vsRoot 'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt')).Trim()
$vcRoot = Join-Path $vsRoot "VC/Tools/MSVC/$vcVersion"
$kitRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10'
$kitVersion = '10.0.26100.0'
$includes = @("/I$PSScriptRoot", "/I$vcRoot/include", "/I$kitRoot/Include/$kitVersion/um", "/I$kitRoot/Include/$kitVersion/shared", "/I$kitRoot/Include/$kitVersion/ucrt", "/I$kitRoot/Include/$kitVersion/winrt", "/I$kitRoot/Include/wdf/umdf/2.25", "/I$kitRoot/Include/$kitVersion/um/iddcx/1.10")
$object = Join-Path $outputDir 'driver.obj'
& (Join-Path $vcRoot 'bin/Hostx64/x64/cl.exe') /nologo /c /std:c++17 /EHsc /MT /W4 /wd4324 /O2 /GS /D_AMD64_ /D_WIN64 /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000000 /DUNICODE /D_UNICODE /DUMDF_DRIVER /DUMDF_VERSION_MAJOR=2 /DUMDF_VERSION_MINOR=25 /DIDDCX_VERSION_MAJOR=1 /DIDDCX_VERSION_MINOR=10 /DIDDCX_MINIMUM_VERSION_REQUIRED=4 $includes "/Fo$object" (Join-Path $PSScriptRoot 'Driver.cpp')
if ($LASTEXITCODE) { throw 'Display driver compilation failed.' }
$binary = Join-Path $outputDir 'OpenUUYCDisplay.dll'
& (Join-Path $vcRoot 'bin/Hostx64/x64/link.exe') /nologo /dll /subsystem:windows /include:FxDriverEntryUm /machine:x64 /dynamicbase /nxcompat /release "/out:$binary" $object "/libpath:$vcRoot/lib/x64" "/libpath:$kitRoot/Lib/$kitVersion/um/x64" "/libpath:$kitRoot/Lib/$kitVersion/ucrt/x64" "/libpath:$kitRoot/Lib/wdf/umdf/x64/2.25" "/libpath:$kitRoot/Lib/$kitVersion/um/x64/iddcx/1.10" WdfDriverStubUm.lib IddCxStub.lib ntdll.lib OneCoreUAP.lib d3d11.lib dxgi.lib avrt.lib
if ($LASTEXITCODE) { throw 'Display driver linking failed.' }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'OpenUUYCDisplay.inf') -Destination $outputDir -Force
& (Join-Path $PSScriptRoot '../package.ps1') -Directory $outputDir -Name OpenUUYCDisplay -Kind display -SigningThumbprint $SigningThumbprint -Stage:$Stage
