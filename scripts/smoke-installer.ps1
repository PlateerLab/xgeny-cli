[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$Asset,
    [Parameter(Mandatory = $true)][string]$Installer
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$SemVerTagPattern = '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?$'

function Invoke-InteractiveSmoke([string]$Executable) {
    $StartInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $StartInfo.FileName = $Executable
    $StartInfo.UseShellExecute = $false
    $StartInfo.CreateNoWindow = $true
    $StartInfo.RedirectStandardInput = $true
    $StartInfo.RedirectStandardOutput = $true
    $StartInfo.RedirectStandardError = $true

    $Process = [System.Diagnostics.Process]::new()
    $Process.StartInfo = $StartInfo
    $StandardOutputBuffer = [System.IO.MemoryStream]::new()
    $StandardErrorBuffer = [System.IO.MemoryStream]::new()
    try {
        $Started = $Process.Start()
        if (-not $Started) {
            throw "failed to start installed binary"
        }
        $StandardOutputTask = $Process.StandardOutput.BaseStream.CopyToAsync($StandardOutputBuffer)
        $StandardErrorTask = $Process.StandardError.BaseStream.CopyToAsync($StandardErrorBuffer)
        $StandardInputBytes = [System.Text.Encoding]::ASCII.GetBytes("/status`n/exit`n")
        [void]$Process.StandardInput.BaseStream.Write(
            $StandardInputBytes,
            0,
            $StandardInputBytes.Length
        )
        [void]$Process.StandardInput.BaseStream.Flush()
        [void]$Process.StandardInput.Close()
        [void]$Process.WaitForExit()
        [void]$StandardOutputTask.GetAwaiter().GetResult()
        [void]$StandardErrorTask.GetAwaiter().GetResult()
        return [PSCustomObject]@{
            ExitCode = $Process.ExitCode
            StandardOutput = [System.Text.Encoding]::UTF8.GetString($StandardOutputBuffer.ToArray())
            StandardError = [System.Text.Encoding]::UTF8.GetString($StandardErrorBuffer.ToArray())
        }
    } finally {
        [void]$StandardOutputBuffer.Dispose()
        [void]$StandardErrorBuffer.Dispose()
        [void]$Process.Dispose()
    }
}

$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Installer = (Resolve-Path -LiteralPath $Installer).Path
$ReportedVersion = (& $Binary --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $ReportedVersion -notmatch '^xgeny (.+)$') {
    throw "binary version output is invalid"
}
$PackageVersion = $Matches[1]
$Tag = "v$PackageVersion"
if ($Tag -notmatch $SemVerTagPattern) {
    throw "binary version is not SemVer"
}

$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) "xgeny-installer-smoke-$([Guid]::NewGuid().ToString('N'))"
$ServerRoot = Join-Path $TestRoot "server"
$ReleaseRoot = Join-Path $ServerRoot $Tag
$InstallRoot = Join-Path $TestRoot "install"
$UnsafeInstallRoot = Join-Path $TestRoot "unsafe-install"
$UnexpectedState = Join-Path $TestRoot "unexpected-state"
New-Item -ItemType Directory -Path $ReleaseRoot -Force | Out-Null
Copy-Item -LiteralPath $Binary -Destination (Join-Path $ReleaseRoot $Asset)
$Digest = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $ReleaseRoot $Asset)).Hash.ToLowerInvariant()
Set-Content -NoNewline -Encoding ascii -LiteralPath (Join-Path $ReleaseRoot "checksums.sha256") -Value "$Digest  $Asset`n"

$Port = 38191
$Server = $null
$PreviousTesting = $env:XGENY_INSTALLER_TESTING
$PreviousBase = $env:XGENY_DOWNLOAD_BASE_URL
$PreviousInstall = $env:XGENY_INSTALL_DIR
$PreviousState = $env:XGENY_STATE_HOME
try {
    $Server = Start-Process -FilePath "python" -ArgumentList @(
        "-m", "http.server", "$Port", "--bind", "127.0.0.1"
    ) -WorkingDirectory $ServerRoot -PassThru -WindowStyle Hidden

    $Ready = $false
    foreach ($Attempt in 1..100) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/$Tag/checksums.sha256" | Out-Null
            $Ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $Ready) {
        throw "loopback fixture server did not start"
    }

    $env:XGENY_INSTALLER_TESTING = "1"
    $env:XGENY_DOWNLOAD_BASE_URL = "http://127.0.0.1:$Port"
    $env:XGENY_INSTALL_DIR = $InstallRoot
    $env:XGENY_STATE_HOME = $UnexpectedState

    $InvalidSemVerRejected = $false
    try {
        & $Installer -Version "v1.2.3-01" -InstallDir $InstallRoot | Out-Null
    } catch {
        $InvalidSemVerRejected = $true
    }
    if (-not $InvalidSemVerRejected) {
        throw "installer accepted a non-SemVer numeric prerelease"
    }

    New-Item -ItemType Directory -Path $UnsafeInstallRoot | Out-Null
    $UnsafeAcl = Get-Acl -LiteralPath $UnsafeInstallRoot
    $Everyone = [System.Security.Principal.SecurityIdentifier]::new("S-1-1-0")
    $Inheritance = (
        [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
        [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
    )
    $UnsafeRule = [System.Security.AccessControl.FileSystemAccessRule]::new(
        $Everyone,
        [System.Security.AccessControl.FileSystemRights]::Write,
        $Inheritance,
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Allow
    )
    $UnsafeAcl.AddAccessRule($UnsafeRule) | Out-Null
    Set-Acl -LiteralPath $UnsafeInstallRoot -AclObject $UnsafeAcl
    $BroadWriteRejected = $false
    try {
        & $Installer -Version $Tag -InstallDir $UnsafeInstallRoot | Out-Null
    } catch {
        $BroadWriteRejected = $true
    }
    if (-not $BroadWriteRejected) {
        throw "installer accepted a broadly writable install directory"
    }

    & $Installer -Version $Tag -InstallDir $InstallRoot | Out-Null

    $Installed = Join-Path $InstallRoot "xgeny.exe"
    $InstalledItem = Get-Item -Force -LiteralPath $Installed
    if ($InstalledItem.PSIsContainer) {
        throw "installer did not create one regular binary"
    }
    if (($InstalledItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "installed binary is a reparse point"
    }
    $InstalledDigest = (Get-FileHash -Algorithm SHA256 -LiteralPath $Installed).Hash
    Add-Content -NoNewline -LiteralPath (Join-Path $ReleaseRoot $Asset) -Value "corrupt"
    $MismatchRejected = $false
    try {
        & $Installer -Version $Tag -InstallDir $InstallRoot | Out-Null
    } catch {
        $MismatchRejected = $true
    }
    if (-not $MismatchRejected) {
        throw "installer accepted a checksum mismatch"
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Installed).Hash -ne $InstalledDigest) {
        throw "failed install changed the existing binary"
    }
    Copy-Item -Force -LiteralPath $Binary -Destination (Join-Path $ReleaseRoot $Asset)

    $InstalledAcl = Get-Acl -LiteralPath $Installed
    $UnsafeFileRule = [System.Security.AccessControl.FileSystemAccessRule]::new(
        $Everyone,
        [System.Security.AccessControl.FileSystemRights]::Write,
        [System.Security.AccessControl.AccessControlType]::Allow
    )
    $InstalledAcl.AddAccessRule($UnsafeFileRule) | Out-Null
    Set-Acl -LiteralPath $Installed -AclObject $InstalledAcl
    $UnsafeInstalledDigest = (Get-FileHash -Algorithm SHA256 -LiteralPath $Installed).Hash
    $BroadFileWriteRejected = $false
    try {
        & $Installer -Version $Tag -InstallDir $InstallRoot | Out-Null
    } catch {
        $BroadFileWriteRejected = $true
    }
    if (-not $BroadFileWriteRejected) {
        throw "installer accepted a broadly writable existing binary"
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Installed).Hash -ne $UnsafeInstalledDigest) {
        throw "rejected unsafe upgrade changed the existing binary"
    }
    $InstalledAcl.RemoveAccessRuleSpecific($UnsafeFileRule)
    Set-Acl -LiteralPath $Installed -AclObject $InstalledAcl

    & $Installer -Version $Tag -InstallDir $InstallRoot | Out-Null
    $InstallEntries = @(Get-ChildItem -Force -LiteralPath $InstallRoot)
    if ($InstallEntries.Count -ne 1 -or $InstallEntries[0].Name -ne "xgeny.exe") {
        throw "installer left temporary or backup files after upgrade"
    }
    $ObservedInstalledVersion = (& $Installed --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $ObservedInstalledVersion -ne $ReportedVersion) {
        throw "installed version is wrong"
    }
    & $Installed protocol check | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "installed protocol check failed"
    }
    $InteractiveResult = Invoke-InteractiveSmoke $Installed
    $InteractiveBannerPresent = $InteractiveResult.StandardOutput.Contains("XGENy Developer Preview")
    $InteractiveStatusPresent = $InteractiveResult.StandardOutput.Contains("status: idle")
    $InteractiveExitPresent = $InteractiveResult.StandardOutput.Contains("bye")
    if (
        $InteractiveResult.ExitCode -ne 0 -or
        -not $InteractiveBannerPresent -or
        -not $InteractiveStatusPresent -or
        -not $InteractiveExitPresent
    ) {
        $InteractiveDiagnostic = @(
            "exit=$($InteractiveResult.ExitCode)",
            "banner=$InteractiveBannerPresent",
            "status=$InteractiveStatusPresent",
            "bye=$InteractiveExitPresent",
            "stdout_chars=$($InteractiveResult.StandardOutput.Length)",
            "stderr_chars=$($InteractiveResult.StandardError.Length)"
        ) -join " "
        throw "installed interactive smoke failed ($InteractiveDiagnostic)"
    }
    $LicenseOutput = (& $Installed licenses | Out-String)
    if (
        $LASTEXITCODE -ne 0 -or
        -not $LicenseOutput.Contains("XGENy CLI Third-Party License Notices") -or
        -not $LicenseOutput.Contains("Copyright notices for The Rust Standard Library") -or
        -not $LicenseOutput.Contains("===== musl C runtime notices =====") -or
        -not $LicenseOutput.Contains("===== LLVM libunwind notices =====")
    ) {
        throw "installed binary is missing embedded license notices"
    }
    if (Test-Path -LiteralPath $UnexpectedState) {
        throw "installer smoke unexpectedly created runtime state"
    }

    Remove-Item -Force -LiteralPath $Installed
    if (Test-Path -LiteralPath $Installed) {
        throw "test-owned install cleanup failed"
    }
    New-Item -ItemType Directory -Path $Installed | Out-Null
    $DirectoryRejected = $false
    try {
        & $Installer -Version $Tag -InstallDir $InstallRoot | Out-Null
    } catch {
        $DirectoryRejected = $true
    }
    if (-not $DirectoryRejected) {
        throw "installer accepted a directory as the binary destination"
    }
    Remove-Item -Recurse -Force -LiteralPath $Installed
    Write-Output "installer smoke passed for $Asset"
} finally {
    $env:XGENY_INSTALLER_TESTING = $PreviousTesting
    $env:XGENY_DOWNLOAD_BASE_URL = $PreviousBase
    $env:XGENY_INSTALL_DIR = $PreviousInstall
    $env:XGENY_STATE_HOME = $PreviousState
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force
        $Server.WaitForExit()
    }
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $TestRoot
}
