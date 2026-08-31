[CmdletBinding()]
param(
    [string]$Version = $env:XGENY_VERSION,
    [string]$InstallDir = $env:XGENY_INSTALL_DIR
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Net.Http

$Repository = "PlateerLab/xgeny-cli"
$DefaultDownloadBase = "https://github.com/$Repository/releases/download"
$SemVerTagPattern = '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?$'

function Fail([string]$Message) {
    throw "xgeny installer: $Message"
}

function Assert-CurrentIdentityOwnedPath([string]$Path, [string]$Kind) {
    $Acl = Get-Acl -LiteralPath $Path
    $OwnerSid = $Acl.GetOwner([System.Security.Principal.SecurityIdentifier])
    $AllowedOwnerSids = @()
    $CurrentIdentity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        if ($null -ne $CurrentIdentity.User) {
            $AllowedOwnerSids += $CurrentIdentity.User.Value
        }
        if ($null -ne $CurrentIdentity.Owner) {
            $AllowedOwnerSids += $CurrentIdentity.Owner.Value
        }
    } finally {
        $CurrentIdentity.Dispose()
    }
    if ($AllowedOwnerSids.Count -eq 0 -or $AllowedOwnerSids -notcontains $OwnerSid.Value) {
        Fail "$Kind must be owned by the current Windows security context"
    }

    $BroadSids = @("S-1-1-0", "S-1-5-11", "S-1-5-32-545")
    [long]$MutationRights = [long](
        [System.Security.AccessControl.FileSystemRights]::CreateFiles -bor
        [System.Security.AccessControl.FileSystemRights]::CreateDirectories -bor
        [System.Security.AccessControl.FileSystemRights]::WriteExtendedAttributes -bor
        [System.Security.AccessControl.FileSystemRights]::WriteAttributes -bor
        [System.Security.AccessControl.FileSystemRights]::Delete -bor
        [System.Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
        [System.Security.AccessControl.FileSystemRights]::ChangePermissions -bor
        [System.Security.AccessControl.FileSystemRights]::TakeOwnership
    )
    $Rules = $Acl.GetAccessRules(
        $true,
        $true,
        [System.Security.Principal.SecurityIdentifier]
    )
    foreach ($Rule in $Rules) {
        if (
            $Rule.AccessControlType -eq [System.Security.AccessControl.AccessControlType]::Allow -and
            $BroadSids -contains $Rule.IdentityReference.Value -and
            (([long]$Rule.FileSystemRights -band $MutationRights) -ne 0)
        ) {
            Fail "$Kind must not be writable by broad Windows principals"
        }
    }
}

function Assert-DownloadUri([Uri]$Uri) {
    if ($Uri.Scheme -eq "https" -and [string]::IsNullOrEmpty($Uri.UserInfo)) {
        return
    }
    if (
        $env:XGENY_INSTALLER_TESTING -eq "1" -and
        $Uri.Scheme -eq "http" -and
        $Uri.Host -eq "127.0.0.1" -and
        $Uri.Port -gt 0 -and
        [string]::IsNullOrEmpty($Uri.UserInfo)
    ) {
        return
    }
    Fail "download redirect left the trusted HTTPS boundary"
}

function Download-File([string]$Uri, [string]$Destination, [long]$MaximumBytes) {
    $Handler = [System.Net.Http.HttpClientHandler]::new()
    $Handler.AllowAutoRedirect = $false
    $Client = [System.Net.Http.HttpClient]::new($Handler)
    $Client.Timeout = [TimeSpan]::FromMilliseconds(-1)
    $Client.DefaultRequestHeaders.UserAgent.ParseAdd("xgeny-installer")
    $Cancellation = [System.Threading.CancellationTokenSource]::new([TimeSpan]::FromMinutes(5))
    $Response = $null
    $InputStream = $null
    $OutputStream = $null
    try {
        $CurrentUri = [Uri]$Uri
        foreach ($Redirect in 0..10) {
            Assert-DownloadUri $CurrentUri
            $Response = $Client.GetAsync(
                $CurrentUri,
                [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead,
                $Cancellation.Token
            ).GetAwaiter().GetResult()
            $StatusCode = [int]$Response.StatusCode
            if ($StatusCode -ge 300 -and $StatusCode -lt 400) {
                if ($Redirect -eq 10 -or $null -eq $Response.Headers.Location) {
                    Fail "release download exceeded the redirect limit"
                }
                $CurrentUri = [Uri]::new($CurrentUri, $Response.Headers.Location)
                $Response.Dispose()
                $Response = $null
                continue
            }
            $Response.EnsureSuccessStatusCode() | Out-Null
            break
        }
        Assert-DownloadUri $CurrentUri
        if (
            $null -ne $Response.Content.Headers.ContentLength -and
            $Response.Content.Headers.ContentLength -gt $MaximumBytes
        ) {
            Fail "release download exceeds the installer limit"
        }

        $InputStream = $Response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $OutputStream = [System.IO.File]::Open(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $Buffer = New-Object byte[] 65536
        [long]$TotalBytes = 0
        while (($Read = $InputStream.ReadAsync(
            $Buffer,
            0,
            $Buffer.Length,
            $Cancellation.Token
        ).GetAwaiter().GetResult()) -gt 0) {
            $TotalBytes += $Read
            if ($TotalBytes -gt $MaximumBytes) {
                Fail "release download exceeds the installer limit"
            }
            $OutputStream.Write($Buffer, 0, $Read)
        }
        $OutputStream.Flush()
    } finally {
        if ($null -ne $OutputStream) { $OutputStream.Dispose() }
        if ($null -ne $InputStream) { $InputStream.Dispose() }
        if ($null -ne $Response) { $Response.Dispose() }
        $Cancellation.Dispose()
        $Client.Dispose()
        $Handler.Dispose()
    }
}

function Resolve-LatestTag {
    $Handler = [System.Net.Http.HttpClientHandler]::new()
    $Handler.AllowAutoRedirect = $false
    $Client = [System.Net.Http.HttpClient]::new($Handler)
    $Client.Timeout = [TimeSpan]::FromMilliseconds(-1)
    $Client.DefaultRequestHeaders.UserAgent.ParseAdd("xgeny-installer")
    $Cancellation = [System.Threading.CancellationTokenSource]::new([TimeSpan]::FromMinutes(1))
    $Response = $null
    try {
        $CurrentUri = [Uri]"https://github.com/$Repository/releases/latest"
        foreach ($Redirect in 0..10) {
            Assert-DownloadUri $CurrentUri
            if ($CurrentUri.Host -ne "github.com" -or -not $CurrentUri.IsDefaultPort) {
                Fail "latest release redirect left GitHub"
            }
            $Response = $Client.GetAsync(
                $CurrentUri,
                [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead,
                $Cancellation.Token
            ).GetAwaiter().GetResult()
            $StatusCode = [int]$Response.StatusCode
            if ($StatusCode -ge 300 -and $StatusCode -lt 400) {
                if ($Redirect -eq 10 -or $null -eq $Response.Headers.Location) {
                    Fail "latest release lookup exceeded the redirect limit"
                }
                $CurrentUri = [Uri]::new($CurrentUri, $Response.Headers.Location)
                $Response.Dispose()
                $Response = $null
                continue
            }
            $Response.EnsureSuccessStatusCode() | Out-Null
            break
        }

        $ExpectedPathPrefix = "/$Repository/releases/tag/"
        if (
            $CurrentUri.Host -ne "github.com" -or
            -not $CurrentUri.IsDefaultPort -or
            -not [string]::IsNullOrEmpty($CurrentUri.Query) -or
            -not [string]::IsNullOrEmpty($CurrentUri.Fragment) -or
            -not $CurrentUri.AbsolutePath.StartsWith(
                $ExpectedPathPrefix,
                [System.StringComparison]::Ordinal
            )
        ) {
            Fail "latest release lookup returned an unexpected location"
        }
        $Candidate = $CurrentUri.AbsolutePath.Substring($ExpectedPathPrefix.Length)
        if ($Candidate -notmatch $SemVerTagPattern) {
            Fail "latest release lookup returned an invalid version"
        }
        return $Candidate
    } finally {
        if ($null -ne $Response) { $Response.Dispose() }
        $Cancellation.Dispose()
        $Client.Dispose()
        $Handler.Dispose()
    }
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = "latest"
}

if ($Version -eq "latest") {
    if (-not [string]::IsNullOrWhiteSpace($env:XGENY_DOWNLOAD_BASE_URL)) {
        Fail "latest cannot be resolved with a custom download base"
    }
    $Version = Resolve-LatestTag
}

if ($Version -notmatch $SemVerTagPattern) {
    Fail "release version must be an exact v-prefixed SemVer tag"
}

$DownloadBase = $DefaultDownloadBase
if (-not [string]::IsNullOrWhiteSpace($env:XGENY_DOWNLOAD_BASE_URL)) {
    if ($env:XGENY_INSTALLER_TESTING -ne "1") {
        Fail "custom download base is reserved for installer tests"
    }
    $DownloadBase = $env:XGENY_DOWNLOAD_BASE_URL.TrimEnd('/')
    if ($DownloadBase -notmatch '^http://127\.0\.0\.1:[1-9][0-9]{0,4}(/[A-Za-z0-9._~/-]*)?$') {
        Fail "loopback test download base URL is invalid"
    }
}

$Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($Architecture -ne "X64") {
    Fail "unsupported Windows architecture: $Architecture"
}
$Asset = "xgeny-x86_64-pc-windows-msvc.exe"

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        Fail "LOCALAPPDATA is required when -InstallDir is omitted"
    }
    $InstallDir = Join-Path $env:LOCALAPPDATA "XGENy\bin"
}
if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
    Fail "install directory must be absolute"
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)

if (-not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}
$DirectoryItem = Get-Item -Force -LiteralPath $InstallDir
if (-not $DirectoryItem.PSIsContainer) {
    Fail "install destination is not a directory"
}
if (($DirectoryItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
    Fail "install directory must not be a reparse point"
}
Assert-CurrentIdentityOwnedPath $InstallDir "install directory"

$Target = Join-Path $InstallDir "xgeny.exe"
if (Test-Path -LiteralPath $Target) {
    $TargetItem = Get-Item -Force -LiteralPath $Target
    if ($TargetItem.PSIsContainer) {
        Fail "existing destination is not a regular file"
    }
    if (($TargetItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        Fail "existing destination must not be a reparse point"
    }
    Assert-CurrentIdentityOwnedPath $Target "existing destination"
}

$Nonce = [Guid]::NewGuid().ToString("N")
$TemporaryBinary = Join-Path $InstallDir ".xgeny-$Nonce.tmp.exe"
$TemporaryChecksums = Join-Path $InstallDir ".xgeny-$Nonce.sha256"
$TemporaryBackup = Join-Path $InstallDir ".xgeny-$Nonce.backup.exe"

try {
    Download-File "$DownloadBase/$Version/checksums.sha256" $TemporaryChecksums 1MB
    Download-File "$DownloadBase/$Version/$Asset" $TemporaryBinary 256MB

    if ((Get-Item -LiteralPath $TemporaryChecksums).Length -gt 1MB) {
        Fail "release checksum file exceeds the installer limit"
    }
    if ((Get-Item -LiteralPath $TemporaryBinary).Length -gt 256MB) {
        Fail "release binary exceeds the installer limit"
    }

    $ExpectedDigests = @()
    foreach ($Line in Get-Content -LiteralPath $TemporaryChecksums) {
        $Match = [regex]::Match($Line, '^([0-9A-Fa-f]{64})  ([^\s]+)$')
        if ($Match.Success -and $Match.Groups[2].Value -eq $Asset) {
            $ExpectedDigests += $Match.Groups[1].Value.ToLowerInvariant()
        }
    }
    if ($ExpectedDigests.Count -ne 1) {
        Fail "release checksum entry is missing, duplicated, or invalid"
    }
    $ActualDigest = (Get-FileHash -Algorithm SHA256 -LiteralPath $TemporaryBinary).Hash.ToLowerInvariant()
    if ($ActualDigest -ne $ExpectedDigests[0]) {
        Fail "binary checksum verification failed"
    }

    $ObservedVersion = (& $TemporaryBinary --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $ObservedVersion -ne "xgeny $($Version.Substring(1))") {
        Fail "downloaded binary version does not match the requested release"
    }
    & $TemporaryBinary protocol check | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Fail "downloaded binary failed its offline protocol check"
    }

    if (Test-Path -LiteralPath $Target) {
        [System.IO.File]::Replace($TemporaryBinary, $Target, $TemporaryBackup, $true)
    } else {
        [System.IO.File]::Move($TemporaryBinary, $Target)
    }
    $InstalledItem = Get-Item -Force -LiteralPath $Target
    if ($InstalledItem.PSIsContainer) {
        Fail "installed destination is not a regular file"
    }
    if (($InstalledItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        Fail "installed destination must not be a reparse point"
    }
    Assert-CurrentIdentityOwnedPath $Target "installed destination"
    $InstalledDigest = (Get-FileHash -Algorithm SHA256 -LiteralPath $Target).Hash.ToLowerInvariant()
    if ($InstalledDigest -ne $ExpectedDigests[0]) {
        Fail "installed binary digest changed during replacement"
    }
    $FinalVersion = (& $Target --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $FinalVersion -ne "xgeny $($Version.Substring(1))") {
        Fail "installed binary version changed during replacement"
    }
    & $Target protocol check | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Fail "installed binary failed its final offline protocol check"
    }
} finally {
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $TemporaryBinary
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $TemporaryChecksums
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $TemporaryBackup
}

Write-Output "XGENy $($Version.Substring(1)) installed at $Target"
$PathEntries = $env:PATH -split ';'
if ($PathEntries -notcontains $InstallDir) {
    Write-Output "Add $InstallDir to PATH to run xgeny from any directory."
}
