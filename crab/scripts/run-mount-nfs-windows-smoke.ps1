#!/usr/bin/env pwsh
#
# Run a native Windows Client for NFS smoke for `crab mount --backend nfs`.

[CmdletBinding()]
param(
    [string]$Drive = $(if ($env:CRAB_NFS_SMOKE_DRIVE) { $env:CRAB_NFS_SMOKE_DRIVE } else { "Z:" }),
    [string]$RunId = $(if ($env:CRAB_NFS_SMOKE_RUN_ID) { $env:CRAB_NFS_SMOKE_RUN_ID } else { "mount-nfs-windows-$((Get-Date).ToUniversalTime().ToString("yyyyMMdd-HHmmss"))" }),
    [string]$ArtifactRoot = $(if ($env:CRAB_NFS_SMOKE_ROOT) { $env:CRAB_NFS_SMOKE_ROOT } else { Join-Path ([System.IO.Path]::GetTempPath()) "crab-mount-nfs-windows-smoke" })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Die {
    param([string]$Message)
    [Console]::Error.WriteLine("error: $Message")
    exit 1
}

function Normalize-DriveTarget {
    param([string]$Value)

    $trimmed = $Value.Trim()
    if ($trimmed -match "^[A-Za-z]:\\?$") {
        return $trimmed.Substring(0, 2).ToUpperInvariant()
    }

    Die "Drive must be a Windows drive target such as Z:"
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory = $true)][string]$LogPath,
        [string]$WorkingDirectory = ""
    )

    $previous = Get-Location
    try {
        if ($WorkingDirectory) {
            Set-Location -LiteralPath $WorkingDirectory
        }

        $output = & $FilePath @ArgumentList 2>&1
        $exitCode = $LASTEXITCODE
        $output | Tee-Object -FilePath $LogPath

        if ($exitCode -ne 0) {
            throw "$FilePath failed with exit code $exitCode; see $LogPath"
        }
    } finally {
        Set-Location $previous
    }
}

function Invoke-PythonVerifier {
    param(
        [Parameter(Mandatory = $true)][string]$ScriptPath,
        [Parameter(Mandatory = $true)][string]$ReportPath,
        [Parameter(Mandatory = $true)][string]$ExpectedGitCommit,
        [Parameter(Mandatory = $true)][string]$LogPath
    )

    $candidates = @(
        @{ Name = "python"; Prefix = @() },
        @{ Name = "python3"; Prefix = @() },
        @{ Name = "py"; Prefix = @("-3") }
    )

    foreach ($candidate in $candidates) {
        $name = [string]$candidate["Name"]
        $prefix = [string[]]$candidate["Prefix"]
        $command = Get-Command $name -ErrorAction SilentlyContinue
        if (-not $command) {
            continue
        }

        $arguments = @($prefix) + @(
            $ScriptPath,
            $ReportPath,
            "--suite",
            "mount-nfs-windows",
            "--platform",
            "windows",
            "--require-artifacts",
            "--expected-git-commit",
            $ExpectedGitCommit
        )
        $output = & $command.Source @arguments 2>&1
        $exitCode = $LASTEXITCODE
        $output | Tee-Object -FilePath $LogPath
        if ($exitCode -ne 0) {
            throw "$name failed with exit code $exitCode; see $LogPath"
        }
        return
    }

    Die "python, python3, or py -3 is required to verify retained NFS smoke evidence"
}

function Redact-ControlEndpoint {
    param([AllowNull()][string]$Endpoint)

    if ($null -eq $Endpoint) {
        return $null
    }
    if ($Endpoint.StartsWith("tcp:", [System.StringComparison]::Ordinal) -and $Endpoint.Contains("?token=")) {
        return ($Endpoint -split "\?token=", 2)[0] + "?token=<redacted>"
    }
    return $Endpoint
}

function Resolve-SystemCommand {
    param([Parameter(Mandatory = $true)][string]$Name)

    $windowsDir = $env:WINDIR
    if (-not $windowsDir) {
        $windowsDir = $env:SystemRoot
    }
    if (-not $windowsDir) {
        Die "WINDIR or SystemRoot is required to locate $Name"
    }

    $candidates = @(
        (Join-Path $windowsDir "System32\$Name"),
        (Join-Path $windowsDir "Sysnative\$Name")
    )

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return $candidate
        }
    }

    Die "Windows Client for NFS command not found: $Name"
}

function Assert-FileText {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Expected
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path"
    }

    $actual = [System.IO.File]::ReadAllText($Path)
    if ($actual -ne $Expected) {
        throw "unexpected file contents for $Path"
    }
}

function Assert-StartsWith {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Prefix
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path"
    }

    $actual = [System.IO.File]::ReadAllText($Path)
    if (-not $actual.StartsWith($Prefix, [System.StringComparison]::Ordinal)) {
        throw "unexpected file prefix for $Path"
    }
}

function Wait-ForPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    for ($i = 0; $i -lt 60; $i++) {
        if (Test-Path -LiteralPath $Path) {
            return
        }
        Start-Sleep -Seconds 1
    }

    throw "timed out waiting for $Path"
}

function Wait-ForDriveGone {
    param([Parameter(Mandatory = $true)][string]$Path)

    for ($i = 0; $i -lt 60; $i++) {
        if (-not (Test-Path -LiteralPath $Path)) {
            return
        }
        Start-Sleep -Seconds 1
    }

    throw "drive is still mounted: $Path"
}

function Expect-Failure {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Script,
        [Parameter(Mandatory = $true)][string]$Message
    )

    try {
        & $Script
    } catch {
        return
    }

    throw $Message
}

function Convert-NfsCounterMap {
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string[]]$Keys
    )

    $snapshot = [ordered]@{}
    foreach ($key in $Keys) {
        $snapshot[$key] = [int64]($Value.PSObject.Properties[$key].Value)
    }
    return $snapshot
}

function Convert-NfsVfsSnapshot {
    param([Parameter(Mandatory = $true)]$Vfs)

    $snapshot = Convert-NfsCounterMap `
        -Value $Vfs `
        -Keys @(
            "open_read_calls",
            "read_at_calls",
            "returned_bytes",
            "source_cache_hits",
            "resolver_calls_avoided",
            "source_cache_misses",
            "source_cache_evictions",
            "source_cache_invalidations",
            "source_cache_stale_evictions",
            "stale_generation_rejections",
            "stale_overlay_view_rejections",
            "stale_overlay_file_rejections"
        )

    foreach ($sourceName in @("base_pointer", "base_blob", "base_empty", "overlay_file")) {
        $source = $Vfs.PSObject.Properties[$sourceName].Value
        $snapshot["${sourceName}_reads"] = [int64]$source.reads
        $snapshot["${sourceName}_bytes"] = [int64]$source.bytes
    }

    foreach ($adaptiveName in @("first", "sequential", "strided", "repeated", "random")) {
        $total = [int64]0
        foreach ($sourceName in @("base_pointer", "base_blob", "base_empty", "overlay_file")) {
            $source = $Vfs.PSObject.Properties[$sourceName].Value
            $total += [int64]$source.adaptive.PSObject.Properties[$adaptiveName].Value
        }
        $snapshot["adaptive_${adaptiveName}"] = $total
    }

    return $snapshot
}

function Get-NfsRuntimeSnapshot {
    param(
        [Parameter(Mandatory = $true)][string]$CrabExe,
        [Parameter(Mandatory = $true)][string]$Mountpoint
    )

    $output = & $CrabExe mount status --mountpoint $Mountpoint --json 2>&1
    $exitCode = $LASTEXITCODE
    $text = ($output | Out-String).Trim()
    if ($exitCode -ne 0) {
        throw "crab mount status --json failed while sampling native read counters: $text"
    }
    $status = $text | ConvertFrom-Json
    if (-not $status.nfs_runtime -or -not $status.nfs_runtime.protocol) {
        throw "mount status did not include NFS protocol counters"
    }
    if (-not $status.nfs_runtime.read_leases) {
        throw "mount status did not include NFS read lease counters"
    }
    if (-not $status.nfs_runtime.vfs) {
        throw "mount status did not include VFS read counters"
    }
    if (-not $status.nfs_runtime.hydration) {
        throw "mount status did not include hydration counters"
    }
    $runtime = $status.nfs_runtime
    return [ordered]@{
        protocol = Convert-NfsCounterMap `
            -Value $runtime.protocol `
            -Keys @("read_rpcs", "read_requested_bytes", "read_returned_bytes")
        read_leases = Convert-NfsCounterMap `
            -Value $runtime.read_leases `
            -Keys @(
                "temporary_overflows",
                "hits",
                "misses",
                "evictions",
                "stale_retries"
            )
        vfs = Convert-NfsVfsSnapshot -Vfs $runtime.vfs
        hydration = Convert-NfsCounterMap `
            -Value $runtime.hydration `
            -Keys @(
                "read_range_requests",
                "read_range_requested_bytes",
                "read_range_returned_bytes",
                "read_window_cache_hits",
                "read_window_cache_misses",
                "read_window_inflight_waits",
                "read_window_remote_fetches",
                "read_window_remote_bytes",
                "read_window_prefetch_requests",
                "read_window_prefetch_scheduled",
                "read_window_prefetch_skipped",
                "read_window_prefetch_errors",
                "chunk_cache_hits",
                "chunk_cache_misses",
                "chunk_inflight_waits",
                "chunk_remote_fetches",
                "chunk_remote_bytes"
            )
    }
}

function New-NfsCounterDelta {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Before,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$After
    )

    $delta = [ordered]@{}
    foreach ($key in $Before.Keys) {
        $delta[$key] = [int64]$After[$key] - [int64]$Before[$key]
    }
    return $delta
}

function Cleanup-Mount {
    if (-not $script:MountAttempted) {
        return
    }

    if ($script:CrabExe -and (Test-Path -LiteralPath $script:CrabExe)) {
        $crabUnmountLog = Join-Path $script:LogDir "cleanup-crab-unmount.log"
        & $script:CrabExe unmount --mountpoint $script:Drive *> $crabUnmountLog
    }

    if ($script:UmountExe -and (Test-Path -LiteralPath $script:UmountExe -PathType Leaf)) {
        $umountLog = Join-Path $script:LogDir "cleanup-umount.log"
        & $script:UmountExe $script:Drive *> $umountLog
    }
}

$nativeWindows = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if (-not $nativeWindows) {
    Die "native Windows NFS smoke must run on Windows"
}

$Drive = Normalize-DriveTarget $Drive
$driveName = $Drive.Substring(0, 1)
$DriveRoot = "$Drive\"

if (Get-PSDrive -Name $driveName -ErrorAction SilentlyContinue) {
    Die "$Drive is already assigned; set CRAB_NFS_SMOKE_DRIVE to an unused drive"
}
if (Test-Path -LiteralPath $DriveRoot) {
    Die "$DriveRoot already exists; set CRAB_NFS_SMOKE_DRIVE to an unused drive"
}

Get-Command cargo -ErrorAction Stop | Out-Null
Get-Command git -ErrorAction Stop | Out-Null
$MountExe = Resolve-SystemCommand "mount.exe"
$UmountExe = Resolve-SystemCommand "umount.exe"

$ScriptDir = Split-Path -Parent $PSCommandPath
$CrabDir = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$WorkspaceRoot = (Resolve-Path (Join-Path $CrabDir "..")).Path
$RunRoot = Join-Path $ArtifactRoot $RunId
$LogDir = Join-Path $RunRoot "logs"
$TestHome = Join-Path $RunRoot "home"
$Source = Join-Path $RunRoot "source"
$DebugDir = Join-Path $WorkspaceRoot "target\debug"
$CrabExe = Join-Path $DebugDir "crab.exe"
$HelperExe = Join-Path $DebugDir "crab-nfs-mount.exe"
$Utf8NoBom = New-Object System.Text.UTF8Encoding -ArgumentList $false
$MountAttempted = $false
$MountDoctorPath = Join-Path $RunRoot "mount-doctor.json"
$MountStatusPath = Join-Path $RunRoot "mount-status.json"
$ControlStatusPath = Join-Path $RunRoot "control-status.json"
$NativeReadBenchmarkPath = Join-Path $RunRoot "native-read-benchmark.json"
$WritebackCheckPath = Join-Path $RunRoot "writeback-check.json"
$UnmountCheckPath = Join-Path $RunRoot "unmount-check.json"
$ControlShutdownPath = Join-Path $RunRoot "control-shutdown.json"
$RemountCheckPath = Join-Path $RunRoot "remount-check.json"
$GitCommit = (& git -C $WorkspaceRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or -not $GitCommit) {
    Die "failed to resolve current Git commit for NFS smoke evidence"
}

New-Item -ItemType Directory -Force -Path $LogDir, $TestHome, $Source | Out-Null

Write-Host "run_id=$RunId"
Write-Host "artifact_root=$RunRoot"
Write-Host "drive=$Drive"

$env:HOME = $TestHome
$env:USERPROFILE = $TestHome
$env:CRAB_CACHE_DIR = Join-Path $RunRoot "crab-cache"
$env:GIT_TERMINAL_PROMPT = "0"
$env:PATH = "$DebugDir;$env:PATH"
New-Item -ItemType Directory -Force -Path $env:CRAB_CACHE_DIR | Out-Null

try {
    Invoke-Native `
        -FilePath "cargo" `
        -ArgumentList @("build", "-p", "crab", "--bin", "crab", "--no-default-features", "--features", "nfs") `
        -LogPath (Join-Path $LogDir "cargo-build-nfs.log") `
        -WorkingDirectory $CrabDir

    Invoke-Native `
        -FilePath "cargo" `
        -ArgumentList @("build", "-p", "crab", "--bin", "crab-nfs-mount", "--no-default-features") `
        -LogPath (Join-Path $LogDir "cargo-build-nfs-helper.log") `
        -WorkingDirectory $CrabDir

    Invoke-Native -FilePath $CrabExe -ArgumentList @("--version") -LogPath (Join-Path $RunRoot "crab-version.txt")
    Invoke-Native -FilePath $HelperExe -ArgumentList @("--version") -LogPath (Join-Path $RunRoot "crab-nfs-mount-version.txt")

    Invoke-Native -FilePath "git" -ArgumentList @("-C", $Source, "init", "-b", "main") -LogPath (Join-Path $LogDir "git-init.log")
    Invoke-Native -FilePath "git" -ArgumentList @("-C", $Source, "config", "user.email", "nfs-smoke@crab.local") -LogPath (Join-Path $LogDir "git-config-email.log")
    Invoke-Native -FilePath "git" -ArgumentList @("-C", $Source, "config", "user.name", "Crab NFS Smoke") -LogPath (Join-Path $LogDir "git-config-name.log")

    [System.IO.File]::WriteAllText((Join-Path $Source "hello.txt"), "hello", $Utf8NoBom)
    New-Item -ItemType Directory -Force -Path (Join-Path $Source "dir") | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $Source "dir\nested.txt"), "nested", $Utf8NoBom)
    $nativeReadSource = Join-Path $Source "native-read.bin"
    $nativeReadSize = 4 * 1024 * 1024
    $nativeReadPattern = New-Object byte[] (256 * 1024)
    for ($index = 0; $index -lt $nativeReadPattern.Length; $index++) {
        $nativeReadPattern[$index] = [byte]($index % 251)
    }
    $nativeReadStream = [System.IO.File]::Open(
        $nativeReadSource,
        [System.IO.FileMode]::Create,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $remaining = $nativeReadSize
        while ($remaining -gt 0) {
            $count = [Math]::Min($nativeReadPattern.Length, $remaining)
            $nativeReadStream.Write($nativeReadPattern, 0, $count)
            $remaining -= $count
        }
    } finally {
        $nativeReadStream.Dispose()
    }

    Invoke-Native -FilePath "git" -ArgumentList @("-C", $Source, "add", ".") -LogPath (Join-Path $LogDir "git-add.log")
    Invoke-Native -FilePath "git" -ArgumentList @("-C", $Source, "commit", "-m", "seed") -LogPath (Join-Path $LogDir "git-commit.log")

    $Source = (Resolve-Path $Source).Path
    Invoke-Native `
        -FilePath $CrabExe `
        -ArgumentList @("mount", "doctor", "--backend", "nfs", "--mountpoint", $Drive, "--json") `
        -LogPath $MountDoctorPath

    $MountAttempted = $true
    Invoke-Native `
        -FilePath $CrabExe `
        -ArgumentList @("mount", "--repo", $Source, "--mountpoint", $Drive, "--backend", "nfs", "--no-refresh") `
        -LogPath (Join-Path $LogDir "mount.log")

    Wait-ForPath (Join-Path $DriveRoot "hello.txt")
    $mountExeLog = Join-Path $LogDir "mount-exe-after-mount.txt"
    & $MountExe *> $mountExeLog

    Assert-FileText -Path (Join-Path $DriveRoot "hello.txt") -Expected "hello"
    Assert-FileText -Path (Join-Path $DriveRoot "dir\nested.txt") -Expected "nested"
    Assert-StartsWith -Path (Join-Path $DriveRoot ".git") -Prefix "gitdir:"

    $nativeReadPath = Join-Path $DriveRoot "native-read.bin"
    $readSize = 256 * 1024
    $passes = 2
    $reads = [int64]0
    $bytesReturned = [int64]0
    $buffer = New-Object byte[] $readSize
    $runtimeBefore = Get-NfsRuntimeSnapshot -CrabExe $CrabExe -Mountpoint $Drive
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    for ($pass = 0; $pass -lt $passes; $pass++) {
        $stream = [System.IO.File]::Open(
            $nativeReadPath,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite
        )
        try {
            while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $reads += 1
                $bytesReturned += $count
            }
        } finally {
            $stream.Dispose()
        }
    }
    $stopwatch.Stop()
    $runtimeAfter = Get-NfsRuntimeSnapshot -CrabExe $CrabExe -Mountpoint $Drive
    $protocolBefore = $runtimeBefore["protocol"]
    $protocolAfter = $runtimeAfter["protocol"]
    $readLeasesBefore = $runtimeBefore["read_leases"]
    $readLeasesAfter = $runtimeAfter["read_leases"]
    $vfsBefore = $runtimeBefore["vfs"]
    $vfsAfter = $runtimeAfter["vfs"]
    $hydrationBefore = $runtimeBefore["hydration"]
    $hydrationAfter = $runtimeAfter["hydration"]
    $protocolDelta = New-NfsCounterDelta -Before $protocolBefore -After $protocolAfter
    $readLeasesDelta = New-NfsCounterDelta -Before $readLeasesBefore -After $readLeasesAfter
    $vfsDelta = New-NfsCounterDelta -Before $vfsBefore -After $vfsAfter
    $hydrationDelta = New-NfsCounterDelta -Before $hydrationBefore -After $hydrationAfter
    $elapsedMs = [Math]::Max([int64]1, [int64]$stopwatch.ElapsedMilliseconds)
    $elapsedSeconds = [Math]::Max($stopwatch.Elapsed.TotalSeconds, 0.000001)
    $userMib = [Math]::Max($bytesReturned / 1MB, 0.000001)
    $efficiency = [ordered]@{
        requested_bytes_per_user_byte = [double]([int64]$protocolDelta.read_requested_bytes / [double]$bytesReturned)
        returned_bytes_per_user_byte = [double]([int64]$protocolDelta.read_returned_bytes / [double]$bytesReturned)
        read_rpcs_per_mib = [double]([int64]$protocolDelta.read_rpcs / $userMib)
    }
    $nativeReadBenchmark = [ordered]@{
        schema_version = 1
        suite = "nfs-native-read-benchmark"
        scenario = "native_sequential_read"
        path = $nativeReadPath
        mountpoint = $Drive
        file_size = [int64](Get-Item -LiteralPath $nativeReadPath).Length
        read_size = [int64]$readSize
        reads = $reads
        bytes_returned = $bytesReturned
        elapsed_ms = $elapsedMs
        mib_per_sec = [double](($bytesReturned / 1MB) / $elapsedSeconds)
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $nativeReadPath).Hash.ToLowerInvariant()
        nfs_protocol_before = $protocolBefore
        nfs_protocol_after = $protocolAfter
        nfs_protocol_delta = $protocolDelta
        nfs_read_leases_before = $readLeasesBefore
        nfs_read_leases_after = $readLeasesAfter
        nfs_read_leases_delta = $readLeasesDelta
        nfs_vfs_before = $vfsBefore
        nfs_vfs_after = $vfsAfter
        nfs_vfs_delta = $vfsDelta
        nfs_hydration_before = $hydrationBefore
        nfs_hydration_after = $hydrationAfter
        nfs_hydration_delta = $hydrationDelta
        efficiency = $efficiency
    }
    $nativeReadBenchmark | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $NativeReadBenchmarkPath -Encoding utf8

    $exclusivePath = Join-Path $DriveRoot "exclusive.txt"
    $exclusiveStream = [System.IO.File]::Open(
        $exclusivePath,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $bytes = $Utf8NoBom.GetBytes("exclusive")
        $exclusiveStream.Write($bytes, 0, $bytes.Length)
    } finally {
        $exclusiveStream.Dispose()
    }
    Expect-Failure -Message "exclusive recreate unexpectedly succeeded" -Script {
        $again = [System.IO.File]::Open(
            $exclusivePath,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $again.Dispose()
    }

    [System.IO.File]::WriteAllText((Join-Path $DriveRoot "created.txt"), "created", $Utf8NoBom)
    [System.IO.File]::AppendAllText((Join-Path $DriveRoot "hello.txt"), "++", $Utf8NoBom)
    New-Item -ItemType Directory -Force -Path (Join-Path $DriveRoot "newdir") | Out-Null
    Move-Item -LiteralPath (Join-Path $DriveRoot "created.txt") -Destination (Join-Path $DriveRoot "newdir\renamed.txt")
    Remove-Item -LiteralPath (Join-Path $DriveRoot "dir\nested.txt")
    Remove-Item -LiteralPath (Join-Path $DriveRoot "dir")

    Expect-Failure -Message "synthetic .git overwrite unexpectedly succeeded" -Script {
        [System.IO.File]::WriteAllText((Join-Path $DriveRoot ".git"), "bad", $Utf8NoBom)
    }

    $gitCandidate = Join-Path $DriveRoot "git-candidate.txt"
    [System.IO.File]::WriteAllText($gitCandidate, "candidate", $Utf8NoBom)
    Expect-Failure -Message "rename over synthetic .git unexpectedly succeeded" -Script {
        Move-Item -LiteralPath $gitCandidate -Destination (Join-Path $DriveRoot ".git") -ErrorAction Stop
    }

    Assert-FileText -Path (Join-Path $DriveRoot "newdir\renamed.txt") -Expected "created"
    Assert-FileText -Path (Join-Path $DriveRoot "exclusive.txt") -Expected "exclusive"
    Assert-FileText -Path (Join-Path $DriveRoot "hello.txt") -Expected "hello++"
    Assert-StartsWith -Path (Join-Path $DriveRoot ".git") -Prefix "gitdir:"
    if (Test-Path -LiteralPath (Join-Path $DriveRoot "dir")) {
        throw "removed directory is still visible"
    }

    $mountListPath = Join-Path $RunRoot "mount-list.json"
    $mountListOutput = & $CrabExe mount list --json 2>&1
    $mountListExit = $LASTEXITCODE
    $mountListText = ($mountListOutput | Out-String).Trim()
    if ($mountListExit -ne 0) {
        $mountListText | Set-Content -LiteralPath $mountListPath
        throw "crab mount list --json failed; see $mountListPath"
    }

    $entries = @($mountListText | ConvertFrom-Json)
    foreach ($entry in $entries) {
        if ($entry.PSObject.Properties.Name -contains "control_endpoint") {
            $entry.control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$entry.control_endpoint))
        }
    }
    ConvertTo-Json -InputObject $entries -Depth 6 | Set-Content -LiteralPath $mountListPath -Encoding utf8
    if ($entries.Count -eq 0) {
        throw "mount registry did not include the NFS mount"
    }

    $entry = $entries | Where-Object {
        [System.StringComparer]::OrdinalIgnoreCase.Equals($_.source, $Source)
    } | Select-Object -First 1
    if (-not $entry) {
        throw "mount registry did not include source $Source"
    }
    if (-not ([string]$entry.state).StartsWith("running", [System.StringComparison]::Ordinal)) {
        throw "mount is not running: $($entry.state)"
    }

    $mountStatusOutput = & $CrabExe mount status --mountpoint $Drive --json 2>&1
    $mountStatusExit = $LASTEXITCODE
    $mountStatusText = ($mountStatusOutput | Out-String).Trim()
    if ($mountStatusExit -ne 0) {
        $mountStatusText | Set-Content -LiteralPath $MountStatusPath
        throw "crab mount status --json failed; see $MountStatusPath"
    }
    $mountStatus = $mountStatusText | ConvertFrom-Json
    $mountStatus.control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$mountStatus.control_endpoint))
    $sanitizedMountStatus = ConvertTo-Json -InputObject $mountStatus -Depth 8
    $sanitizedMountStatus | Set-Content -LiteralPath $MountStatusPath -Encoding utf8
    $controlStatusOutput = & $CrabExe mount status --mountpoint $Drive --live-only --json 2>&1
    $controlStatusExit = $LASTEXITCODE
    $controlStatusText = ($controlStatusOutput | Out-String).Trim()
    if ($controlStatusExit -ne 0) {
        $controlStatusText | Set-Content -LiteralPath $ControlStatusPath
        throw "crab mount status --live-only --json failed; see $ControlStatusPath"
    }
    $controlStatus = $controlStatusText | ConvertFrom-Json
    $controlStatus.control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$controlStatus.control_endpoint))
    ConvertTo-Json -InputObject $controlStatus -Depth 8 | Set-Content -LiteralPath $ControlStatusPath -Encoding utf8
    if (-not $mountStatus.nfs_runtime) {
        throw "mount status did not include live NFS runtime counters"
    }
    if ([int64]$mountStatus.nfs_runtime.protocol.read_rpcs -le 0) {
        throw "NFS runtime did not record read RPCs"
    }
    if (-not $mountStatus.nfs_runtime.lifecycle) {
        throw "NFS runtime did not include lifecycle counters"
    }
    foreach ($key in @("server_bind_ms", "native_mount_ms", "startup_ms")) {
        if (-not ($mountStatus.nfs_runtime.lifecycle.PSObject.Properties.Name -contains $key)) {
            throw "NFS lifecycle counter $key is missing"
        }
        $value = [int64]$mountStatus.nfs_runtime.lifecycle.$key
        if ($value -lt 0) {
            throw "NFS lifecycle counter $key must be a non-negative integer"
        }
    }
    if ([int64]$mountStatus.nfs_runtime.lifecycle.startup_ms -lt [int64]$mountStatus.nfs_runtime.lifecycle.server_bind_ms) {
        throw "NFS lifecycle startup_ms must cover server_bind_ms"
    }
    if ([int64]$mountStatus.nfs_runtime.lifecycle.startup_ms -lt [int64]$mountStatus.nfs_runtime.lifecycle.native_mount_ms) {
        throw "NFS lifecycle startup_ms must cover native_mount_ms"
    }
    if (-not $mountStatus.nfs_runtime.read_leases) {
        throw "NFS runtime did not include read lease counters"
    }
    foreach ($key in @("entries", "max_entries", "estimated_bytes", "max_estimated_bytes", "pinned_entries", "active_pins", "temporary_overflows", "hits", "misses", "evictions", "stale_retries")) {
        if (-not ($mountStatus.nfs_runtime.read_leases.PSObject.Properties.Name -contains $key)) {
            throw "NFS read lease counter $key is missing"
        }
        $value = [int64]$mountStatus.nfs_runtime.read_leases.$key
        if ($value -lt 0) {
            throw "NFS read lease counter $key must be a non-negative integer"
        }
    }
    foreach ($key in @("max_entries", "max_estimated_bytes")) {
        if ([int64]$mountStatus.nfs_runtime.read_leases.$key -le 0) {
            throw "NFS read lease budget $key must be positive"
        }
    }
    if ([int64]$mountStatus.nfs_runtime.read_leases.hits -le 0) {
        throw "NFS read lease hits must be positive"
    }
    if ([int64]$mountStatus.nfs_runtime.read_leases.misses -le 0) {
        throw "NFS read lease misses must be positive"
    }
    if (-not $mountStatus.nfs_runtime.directory_pages) {
        throw "NFS runtime did not include directory page cache counters"
    }
    foreach ($key in @("entries", "max_entries", "estimated_bytes", "max_estimated_bytes", "hits", "misses", "evictions", "stale_evictions")) {
        if (-not ($mountStatus.nfs_runtime.directory_pages.PSObject.Properties.Name -contains $key)) {
            throw "NFS directory page cache counter $key is missing"
        }
        $value = [int64]$mountStatus.nfs_runtime.directory_pages.$key
        if ($value -lt 0) {
            throw "NFS directory page cache counter $key must be a non-negative integer"
        }
    }
    if (-not $mountStatus.nfs_runtime.write_journal) {
        throw "NFS runtime did not include write journal counters"
    }
    foreach ($key in @("sync_attempts", "sync_successes", "sync_failures", "total_sync_latency_ms")) {
        if (-not ($mountStatus.nfs_runtime.write_journal.PSObject.Properties.Name -contains $key)) {
            throw "NFS write journal counter $key is missing"
        }
        $value = [int64]$mountStatus.nfs_runtime.write_journal.$key
        if ($value -lt 0) {
            throw "NFS write journal counter $key must be a non-negative integer"
        }
    }
    if ([int64]$mountStatus.nfs_runtime.write_journal.sync_successes + [int64]$mountStatus.nfs_runtime.write_journal.sync_failures -gt [int64]$mountStatus.nfs_runtime.write_journal.sync_attempts) {
        throw "NFS write journal sync successes and failures must not exceed attempts"
    }
    $WritebackCheck = [ordered]@{
        schema_version = 1
        action = "writeback"
        mountpoint = [string]$mountStatus.mountpoint
        source = [string]$mountStatus.source
        state = [string]$mountStatus.state
        pid = [int64]$mountStatus.pid
        control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$mountStatus.control_endpoint))
        log_path = [string]$mountStatus.log_path
        content_checks = [ordered]@{
            hello_appended = $true
            renamed_file_created = $true
            exclusive_file_created = $true
            gitdir_preserved = $true
            gitdir_overwrite_rejected = $true
            gitdir_rename_rejected = $true
            removed_directory_absent = $true
        }
    }
    $WritebackCheck | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $WritebackCheckPath -Encoding utf8

    Invoke-Native `
        -FilePath $CrabExe `
        -ArgumentList @("unmount", "--mountpoint", $Drive) `
        -LogPath (Join-Path $LogDir "unmount.log")

    Wait-ForDriveGone $DriveRoot
    $MountAttempted = $false
    $UnmountCheck = [ordered]@{
        schema_version = 1
        action = "control_shutdown"
        mountpoint = [string]$mountStatus.mountpoint
        source = [string]$mountStatus.source
        pid = [int64]$mountStatus.pid
        control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$mountStatus.control_endpoint))
        log_path = [string]$mountStatus.log_path
        mounted_after = $false
    }
    $UnmountCheck | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $UnmountCheckPath -Encoding utf8
    $UnmountCheck | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $ControlShutdownPath -Encoding utf8

    $MountAttempted = $true
    Invoke-Native `
        -FilePath $CrabExe `
        -ArgumentList @("mount", "--repo", $Source, "--mountpoint", $Drive, "--backend", "nfs", "--no-refresh") `
        -LogPath (Join-Path $LogDir "remount.log")

    Wait-ForPath (Join-Path $DriveRoot "hello.txt")
    Assert-FileText -Path (Join-Path $DriveRoot "hello.txt") -Expected "hello++"
    Assert-FileText -Path (Join-Path $DriveRoot "newdir\renamed.txt") -Expected "created"
    Assert-FileText -Path (Join-Path $DriveRoot "exclusive.txt") -Expected "exclusive"
    Assert-StartsWith -Path (Join-Path $DriveRoot ".git") -Prefix "gitdir:"
    if (Test-Path -LiteralPath (Join-Path $DriveRoot "dir")) {
        throw "removed directory is visible after remount"
    }
    $remountStatusOutput = & $CrabExe mount status --mountpoint $Drive --json 2>&1
    $remountStatusExit = $LASTEXITCODE
    $remountStatusText = ($remountStatusOutput | Out-String).Trim()
    if ($remountStatusExit -ne 0) {
        throw "crab mount status --json failed after remount: $remountStatusText"
    }
    $remountStatus = $remountStatusText | ConvertFrom-Json
    $remountStatus.control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$remountStatus.control_endpoint))
    $RemountCheck = [ordered]@{
        schema_version = 1
        action = "remount"
        mountpoint = [string]$remountStatus.mountpoint
        source = [string]$remountStatus.source
        state = [string]$remountStatus.state
        pid = [int64]$remountStatus.pid
        control_endpoint = (Redact-ControlEndpoint -Endpoint ([string]$remountStatus.control_endpoint))
        log_path = [string]$remountStatus.log_path
        mounted_after = $true
        content_checks = [ordered]@{
            hello_preserved = $true
            renamed_file_preserved = $true
            exclusive_file_preserved = $true
            gitdir_preserved = $true
            removed_directory_absent = $true
        }
    }
    $RemountCheck | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $RemountCheckPath -Encoding utf8

    Invoke-Native `
        -FilePath $CrabExe `
        -ArgumentList @("unmount", "--mountpoint", $Drive) `
        -LogPath (Join-Path $LogDir "remount-unmount.log")

    Wait-ForDriveGone $DriveRoot
    $MountAttempted = $false

    $ReportPath = Join-Path $RunRoot "nfs-smoke-report.json"
    $Report = [ordered]@{
        schema_version = 1
        suite = "mount-nfs-windows"
        platform = "windows"
        status = "ok"
        backend = "nfs"
        run_id = $RunId
        git_commit = $GitCommit
        artifact_root = $RunRoot
        crab_version = ([System.IO.File]::ReadAllText((Join-Path $RunRoot "crab-version.txt"))).Trim()
        helper_version = ([System.IO.File]::ReadAllText((Join-Path $RunRoot "crab-nfs-mount-version.txt"))).Trim()
        checks = @(
            "build",
            "helper_version",
            "mount_doctor",
            "initial_read",
            "native_read_benchmark",
            "writeback",
            "mount_list",
            "mount_status",
            "control_status",
            "unmount",
            "control_shutdown",
            "remount"
        )
        artifacts = [ordered]@{
            mount_list = $mountListPath
            mount_doctor = $MountDoctorPath
            mount_status = $MountStatusPath
            control_status = $ControlStatusPath
            native_read_benchmark = $NativeReadBenchmarkPath
            writeback_check = $WritebackCheckPath
            unmount_check = $UnmountCheckPath
            control_shutdown = $ControlShutdownPath
            remount_check = $RemountCheckPath
        }
    }
    $Report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $ReportPath -Encoding utf8
    Invoke-PythonVerifier `
        -ScriptPath (Join-Path $CrabDir "scripts\verify-nfs-smoke-report.py") `
        -ReportPath $ReportPath `
        -ExpectedGitCommit $GitCommit `
        -LogPath (Join-Path $LogDir "verify-nfs-smoke-report.log")
    Write-Host "nfs_smoke_report=$ReportPath"
    Write-Host "windows_nfs_mount_smoke=ok"
} finally {
    Cleanup-Mount
}
