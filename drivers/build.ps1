<#
.SYNOPSIS
  Builds the ORBIT Display driver package into drivers\out\x64:
  OrbitDisplay.dll + OrbitDisplay.inf (stamped) + OrbitDisplay.cat (unsigned).
  The ORBIT installer signs the package on the PC it installs on.

  Needs Visual Studio 2022 or its Build Tools with the C++ workload. The Windows SDK and WDK
  headers/libraries come from NuGet (packages.config), so nothing else is installed. nuget.exe is
  downloaded into drivers\.tools when it isn't on PATH.
.EXAMPLE
  drivers\build.ps1 -Version 0.3.11
#>
param([string] $Version = "0.0.0")
# Not "Stop": native tools print progress on stderr, which Windows PowerShell would treat as
# fatal. Each step is checked by its exit code instead.
$ErrorActionPreference = "Continue"
$Root = $PSScriptRoot
$Kit = "10.0.28000.2526"
$Sdk = "10.0.28000.0"
$Umdf = "2.25"   # UMDF 2.25: Windows 10 1809+
$Iddcx = "1.4"   # IddCx 1.4: Windows 10 1903+ (choosing the render GPU needs it)

# Driver versions are four numbers. The fourth counts hours since 2026, so every build is newer
# than the last and Windows replaces a loaded driver instead of keeping it.
$Version = $Version.Split('-')[0].Split('+')[0]  # 0.3.11-beta -> 0.3.11
$Hours = [int][Math]::Floor(((Get-Date).ToUniversalTime() - [datetime]'2026-01-01').TotalHours) % 65535
$DriverVersion = "$Version.$Hours"

# --- WDK + SDK from NuGet ---------------------------------------------------------------------
$nuget = (Get-Command nuget.exe -ErrorAction SilentlyContinue).Source
if (-not $nuget) {
  $nuget = Join-Path $Root ".tools\nuget.exe"
  if (-not (Test-Path $nuget)) {
    New-Item -ItemType Directory -Force (Split-Path $nuget) | Out-Null
    Invoke-WebRequest "https://dist.nuget.org/win-x86-commandline/v6.11.1/nuget.exe" -OutFile $nuget -UseBasicParsing
  }
}
& $nuget restore (Join-Path $Root "packages.config") -PackagesDirectory (Join-Path $Root "packages") -NonInteractive -Verbosity quiet
if ($LASTEXITCODE) { throw "could not fetch the WDK/SDK packages" }
$WdkC = Join-Path $Root "packages\Microsoft.Windows.WDK.x64.$Kit\c"
$SdkC = Join-Path $Root "packages\Microsoft.Windows.SDK.CPP.$Kit\c"
$SdkLibC = Join-Path $Root "packages\Microsoft.Windows.SDK.CPP.x64.$Kit\c"

# --- MSVC environment (cl.exe, link.exe, the C++ standard library) ----------------------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -products * -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio 2022 (or Build Tools) with the C++ workload is required" }
cmd /c "`"$vs\VC\Auxiliary\Build\vcvars64.bat`" >nul 2>&1 && set" | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') { Set-Item "env:$($Matches[1])" $Matches[2] }
}

# --- Compile + link ---------------------------------------------------------------------------
$Obj = Join-Path $Root "out\obj"
$Pkg = Join-Path $Root "out\x64"
foreach ($d in $Obj, $Pkg) { New-Item -ItemType Directory -Force $d | Out-Null; Get-ChildItem $d -File | Remove-Item }

$includes = @(
  "$WdkC\Include\wdf\umdf\$Umdf", "$WdkC\Include\$Sdk\um\iddcx\$Iddcx", "$WdkC\Include\$Sdk\um", "$WdkC\Include\$Sdk\shared",
  "$SdkC\Include\$Sdk\um", "$SdkC\Include\$Sdk\shared", "$SdkC\Include\$Sdk\ucrt"
) | ForEach-Object { "/I`"$_`"" }
$defines = @(
  "UNICODE", "_UNICODE", "_WINDLL", "_WIN32_WINNT=0x0A00", "WINVER=0x0A00", "NTDDI_VERSION=0x0A000007",
  "UMDF_USING_NTSTATUS", "UMDF_VERSION_MAJOR=2", "UMDF_VERSION_MINOR=$($Umdf.Split('.')[1])",
  "IDDCX_VERSION_MAJOR=1", "IDDCX_VERSION_MINOR=$($Iddcx.Split('.')[1])", "IDDCX_MINIMUM_VERSION_REQUIRED=$($Iddcx.Split('.')[1])"
) | ForEach-Object { "/D$_" }
$sources = Get-ChildItem (Join-Path $Root "orbit-display") -Filter *.cpp | ForEach-Object { "`"$($_.FullName)`"" }

# The EDID/CVT code is plain C++: test it on its own before building the driver.
$Test = Join-Path $Root "out\test"
New-Item -ItemType Directory -Force $Test | Out-Null
cmd /c "cl /nologo /std:c++17 /permissive- /EHsc /W4 /WX /MT /Fo`"$Test\\`" /Fe`"$Test\edid_test.exe`" `"$Root\orbit-display\tests\edid_test.cpp`" `"$Root\orbit-display\edid.cpp`""
if ($LASTEXITCODE) { throw "compiling the EDID test failed" }
& "$Test\edid_test.exe"
if ($LASTEXITCODE) { throw "EDID test failed" }

# /MT: the C runtime is linked in, so the driver needs no Visual C++ redistributable on the target.
cmd /c "cl /nologo /c /std:c++17 /permissive- /EHsc /W4 /WX /O2 /MT /GS /guard:cf /Zi /Fd`"$Obj\\`" /Fo`"$Obj\\`" $($includes -join ' ') $($defines -join ' ') $($sources -join ' ')"
if ($LASTEXITCODE) { throw "compiling failed" }

$libPaths = @(
  "$WdkC\Lib\wdf\umdf\x64\$Umdf", "$WdkC\Lib\$Sdk\um\x64\iddcx\$Iddcx", "$SdkLibC\um\x64", "$SdkLibC\ucrt\x64"
) | ForEach-Object { "/LIBPATH:`"$_`"" }
$libs = "WdfDriverStubUm.lib iddcxstub.lib avrt.lib d3d11.lib dxgi.lib kernel32.lib advapi32.lib ntdll.lib"
cmd /c "link /nologo /DLL /NOIMPLIB /NOEXP /OUT:`"$Pkg\OrbitDisplay.dll`" /PDB:`"$Obj\OrbitDisplay.pdb`" /DEBUG /OPT:REF /OPT:ICF /guard:cf /DYNAMICBASE /NXCOMPAT /SUBSYSTEM:WINDOWS $($libPaths -join ' ') `"$Obj\*.obj`" $libs"
if ($LASTEXITCODE) { throw "linking failed" }

# --- INF + catalog ----------------------------------------------------------------------------
Copy-Item (Join-Path $Root "orbit-display\OrbitDisplay.inf") $Pkg
& "$WdkC\bin\$Sdk\x64\stampinf.exe" -f "$Pkg\OrbitDisplay.inf" -d * -a amd64 -v $DriverVersion -u "$Umdf.0" | Out-Null
if ($LASTEXITCODE) { throw "stampinf failed" }
& "$WdkC\bin\$Sdk\x86\Inf2Cat.exe" "/driver:$Pkg" /os:10_19H1_X64 /uselocaltime | Out-Null
if ($LASTEXITCODE) { & "$WdkC\bin\$Sdk\x86\Inf2Cat.exe" "/driver:$Pkg" /os:10_19H1_X64 /uselocaltime; throw "Inf2Cat failed" }

Write-Host "ORBIT Display $DriverVersion (unsigned): $Pkg"
Get-ChildItem $Pkg | Format-Table Name, Length -AutoSize
