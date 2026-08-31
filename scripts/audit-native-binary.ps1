[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Item = Get-Item -Force -LiteralPath $Binary
if ($Item.PSIsContainer -or ($Item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw "native audit input must be one regular file"
}

$VsWhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -PathType Leaf -LiteralPath $VsWhere)) {
    throw "vswhere.exe is required for the Windows native audit"
}
$VisualStudio = (& $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($VisualStudio)) {
    throw "Visual Studio C++ tools could not be located"
}
$VersionFile = Join-Path $VisualStudio "VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt"
if (-not (Test-Path -PathType Leaf -LiteralPath $VersionFile)) {
    throw "default MSVC tools version file could not be located"
}
$ToolsVersion = (Get-Content -Raw -LiteralPath $VersionFile).Trim()
if ($ToolsVersion -notmatch '^[0-9]+(\.[0-9]+)+$') {
    throw "default MSVC tools version is invalid"
}
$Dumpbin = Join-Path $VisualStudio "VC\Tools\MSVC\$ToolsVersion\bin\Hostx64\x64\dumpbin.exe"
if (-not (Test-Path -PathType Leaf -LiteralPath $Dumpbin)) {
    throw "dumpbin.exe could not be located"
}

$Headers = (& $Dumpbin /NOLOGO /HEADERS $Binary | Out-String)
if ($LASTEXITCODE -ne 0 -or $Headers -notmatch '(?im)^\s*8664 machine \(x64\)') {
    throw "Windows release binary is not an x64 PE image"
}
$Dependents = (& $Dumpbin /NOLOGO /DEPENDENTS $Binary | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "Windows dependency inspection failed"
}
$DynamicCrtPattern = '(?im)^\s*(VCRUNTIME[^\s]*|MSVCP[^\s]*|MSVCR[^\s]*|CONCRT[^\s]*|UCRTBASE|api-ms-win-crt-[^\s]*)\.dll\s*$'
if ($Dependents -match $DynamicCrtPattern) {
    throw "Windows release binary imports a dynamic Visual C++ or Universal CRT library"
}
$ImportedDlls = @()
$DependencySections = 0
$InDependencySection = $false
$SectionHasEntry = $false
foreach ($Line in ($Dependents -split "`r?`n")) {
    $Trimmed = $Line.Trim()
    if ($Trimmed -match '^Image has the following (delay load )?dependencies:$') {
        if ($InDependencySection) {
            throw "Windows dependency inspection returned overlapping sections"
        }
        $DependencySections += 1
        $InDependencySection = $true
        $SectionHasEntry = $false
        continue
    }
    if (-not $InDependencySection) {
        continue
    }
    if ([string]::IsNullOrWhiteSpace($Trimmed)) {
        if ($SectionHasEntry) {
            $InDependencySection = $false
        }
        continue
    }
    if ($Trimmed -notmatch '^[A-Za-z0-9._-]+\.dll$') {
        throw "Windows dependency inspection returned an unrecognized import entry"
    }
    $ImportedDlls += $Trimmed
    $SectionHasEntry = $true
}
if ($InDependencySection -and -not $SectionHasEntry) {
    throw "Windows dependency inspection returned an empty dependency section"
}
if ($DependencySections -eq 0) {
    throw "Windows dependency inspection found no dependency section"
}
if ($ImportedDlls.Count -eq 0) {
    throw "Windows dependency inspection found no imported system libraries"
}
$ApiSetPattern = '^(api|ext)-[A-Za-z0-9-]+-l[0-9]+-[0-9]+-[0-9]+\.dll$'
$ApiSetResolverSource = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class XgenyApiSetResolver
{
    [DllImport(
        "api-ms-win-core-apiquery-l2-1-0.dll",
        ExactSpelling = true,
        CallingConvention = CallingConvention.Winapi)]
    public static extern int GetApiSetModuleBaseName(
        [MarshalAs(UnmanagedType.LPStr)] string contractName,
        uint bufferLength,
        [MarshalAs(UnmanagedType.LPWStr)] StringBuilder moduleBaseName,
        out uint actualNameLength);
}
'@
Add-Type -TypeDefinition $ApiSetResolverSource -Language CSharp
foreach ($ImportedDll in $ImportedDlls) {
    if (Test-Path -PathType Leaf -LiteralPath (Join-Path $env:SystemRoot "System32\$ImportedDll")) {
        continue
    }
    if ($ImportedDll -match $ApiSetPattern) {
        $HostBuffer = New-Object System.Text.StringBuilder 260
        [uint32]$ActualNameLength = 0
        $Result = [XgenyApiSetResolver]::GetApiSetModuleBaseName(
            $ImportedDll,
            [uint32]$HostBuffer.Capacity,
            $HostBuffer,
            [ref]$ActualNameLength
        )
        if ($Result -ne 0) {
            throw "Windows API-set contract is not implemented: $ImportedDll"
        }
        $HostModule = $HostBuffer.ToString()
        if (
            $HostModule -notmatch '^[A-Za-z0-9._-]+\.dll$' -or
            $ActualNameLength -ne [uint32]($HostModule.Length + 1) -or
            -not (Test-Path -PathType Leaf -LiteralPath (Join-Path $env:SystemRoot "System32\$HostModule"))
        ) {
            throw "Windows API-set contract resolved outside System32: $ImportedDll"
        }
        Write-Output "API-set import: $ImportedDll -> $HostModule"
        continue
    }
    throw "Windows release binary imports a non-system library: $ImportedDll"
}

Write-Output "runner image: $env:ImageOS $env:ImageVersion"
Write-Output "MSVC tools: $ToolsVersion"
Write-Output "native binary audit passed for x86_64-pc-windows-msvc"
