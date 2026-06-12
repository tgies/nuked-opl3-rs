param(
    [ValidateSet("auto", "cl", "clang-cl")]
    [string]$Compiler = "auto",

    [string]$Configuration = "release",

    [string]$NukedRoot = "src\nuked-opl3",

    [string]$OutputName
)

$ErrorActionPreference = "Stop"

$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
$OutDir = Join-Path $Root "target\$Configuration"
$CompilerExe = if ($Compiler -eq "clang-cl") { "clang-cl.exe" } else { "cl.exe" }
$OutName = if ($OutputName) {
    $OutputName
} elseif ($Compiler -eq "clang-cl") {
    "c_bench_full_file_clang_cl.exe"
} else {
    "c_bench_full_file.exe"
}
$OutExe = Join-Path $OutDir $OutName
$BenchSource = Join-Path $Root "tools\c_bench_full_file.c"
$NukedRootPath = if ([System.IO.Path]::IsPathRooted($NukedRoot)) {
    Resolve-Path $NukedRoot
} else {
    Resolve-Path (Join-Path $Root $NukedRoot)
}
$IncludeDir = $NukedRootPath
$Opl3Source = Join-Path $NukedRootPath "opl3.c"

if (!(Test-Path (Join-Path $NukedRootPath "opl3.h"))) {
    throw "opl3.h was not found under $NukedRootPath"
}
if (!(Test-Path $Opl3Source)) {
    throw "opl3.c was not found under $NukedRootPath"
}

New-Item -ItemType Directory -Force $OutDir | Out-Null

$ClArgs = @(
    "/nologo",
    "/O2",
    "/DNDEBUG",
    "/D_CRT_SECURE_NO_WARNINGS",
    "/I", $IncludeDir,
    $BenchSource,
    $Opl3Source,
    "/Fe:$OutExe"
)

if (Get-Command $CompilerExe -ErrorAction SilentlyContinue) {
    & $CompilerExe @ClArgs
    exit $LASTEXITCODE
}

$ProgramFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
$VsWhere = Join-Path $ProgramFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
if (!(Test-Path $VsWhere)) {
    throw "$CompilerExe is not on PATH and vswhere.exe was not found. Install Visual Studio Build Tools with the C++ workload."
}

$VsInstall = & $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$VsInstall) {
    throw "Visual Studio C++ tools were not found. Install Visual Studio Build Tools with the C++ workload."
}

$VsDevCmd = Join-Path $VsInstall "Common7\Tools\VsDevCmd.bat"
if (!(Test-Path $VsDevCmd)) {
    throw "VsDevCmd.bat was not found under $VsInstall"
}

$QuotedArgs = $ClArgs | ForEach-Object {
    '"' + ($_ -replace '"', '\"') + '"'
}
$Command = 'call "' + $VsDevCmd + '" -arch=x64 -host_arch=x64 && ' + $CompilerExe + ' ' + ($QuotedArgs -join ' ')

& cmd.exe /d /s /c $Command
exit $LASTEXITCODE
