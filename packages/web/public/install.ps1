# Crab CLI installer for Windows
#
# Usage:
#   irm https://crab.build/install.ps1 | iex
#
# Environment variables:
#   $env:CRAB_VERSION     — install a specific version (e.g. "v1.0.15"). Default: latest.
#   $env:CRAB_INSTALL_DIR — installation directory. Default: ~\.crab\bin.

$ErrorActionPreference = "Stop"

$Repo = "crabbuild/crab"
$InstallDir = if ($env:CRAB_INSTALL_DIR) { $env:CRAB_INSTALL_DIR } else { "$HOME\.crab\bin" }
$Version = if ($env:CRAB_VERSION) { $env:CRAB_VERSION } else { "latest" }

function Write-Info($msg) { Write-Host "==> $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "warning: $msg" -ForegroundColor Yellow }
function Write-Err($msg) { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Get-Target {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64"   { return "windows-x86_64" }
        "Arm64" { return "windows-aarch64" }
        default { Write-Err "Unsupported architecture: $arch" }
    }
}

function Resolve-Version {
    if ($script:Version -eq "latest") {
        $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
        $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ Accept = "application/vnd.github+json" }
        $script:Version = $release.tag_name
        if (-not $script:Version) {
            Write-Err "Failed to resolve latest version"
        }
    }
    if (-not $script:Version.StartsWith("v")) {
        $script:Version = "v$($script:Version)"
    }
}

function Invoke-Download($url, $output, $label) {
    try {
        Invoke-WebRequest -Uri $url -OutFile $output -UseBasicParsing
    } catch {
        Write-Err "Failed to download $label from $url"
    }
}

function Verify-Checksum($assetName, $assetPath, $checksumsPath) {
    $line = Get-Content -Path $checksumsPath |
        Where-Object {
            $parts = $_ -split "\s+"
            $parts.Count -ge 2 -and ($parts[1] -eq $assetName -or $parts[1] -eq "*$assetName")
        } |
        Select-Object -First 1

    if (-not $line) {
        Write-Err "SHA256SUMS.txt does not contain a checksum for $assetName"
    }

    $expected = (($line -split "\s+")[0]).ToLowerInvariant()
    $actual = (Get-FileHash -Path $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Write-Err "Checksum verification failed for $assetName`n  expected: $expected`n  actual:   $actual"
    }
}

function Verify-ZipLayout($zipPath, $assetName) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $entries = @($zip.Entries | Where-Object { $_.Name })
        $names = @($entries | ForEach-Object { $_.FullName })
        if ($names.Count -ne 2 -or -not ($names -contains "crab.exe") -or -not ($names -contains "crab-nfs-mount.exe")) {
            Write-Err "Unexpected archive layout in $assetName. Expected root-level crab.exe and crab-nfs-mount.exe binaries."
        }
    } finally {
        $zip.Dispose()
    }
}

function Install-Crab {
    $target = Get-Target
    Write-Host "`nInstalling Crab CLI`n" -NoNewline
    Write-Info "Platform: $target"

    Resolve-Version
    $zipName = "crab-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$Version/$zipName"
    $checksumsUrl = "https://github.com/$Repo/releases/download/$Version/SHA256SUMS.txt"

    Write-Info "Downloading crab $Version for $target"
    Write-Info "  $url"

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "crab-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    $zipPath = Join-Path $tmpDir $zipName
    $checksumsPath = Join-Path $tmpDir "SHA256SUMS.txt"
    $extractDir = Join-Path $tmpDir "extract"

    try {
        Invoke-Download $url $zipPath $zipName
        Invoke-Download $checksumsUrl $checksumsPath "SHA256SUMS.txt"
        Verify-Checksum $zipName $zipPath $checksumsPath
        Verify-ZipLayout $zipPath $zipName

        Write-Info "Installing to $InstallDir"
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        New-Item -ItemType Directory -Path $extractDir -Force | Out-Null
        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

        $sourceExe = Join-Path $extractDir "crab.exe"
        if (-not (Test-Path $sourceExe)) {
            Write-Err "crab.exe not found after extraction"
        }

        $crabExe = Join-Path $InstallDir "crab.exe"
        $stagedExe = Join-Path $InstallDir ".crab.tmp.$PID.exe"
        Copy-Item -Path $sourceExe -Destination $stagedExe -Force
        if (Test-Path $crabExe) {
            Remove-Item -Path $crabExe -Force
        }
        Move-Item -Path $stagedExe -Destination $crabExe -Force

        $sourceNfsMount = Join-Path $extractDir "crab-nfs-mount.exe"
        if (-not (Test-Path $sourceNfsMount)) {
            Write-Err "crab-nfs-mount.exe not found after extraction"
        }
        $nfsMountExe = Join-Path $InstallDir "crab-nfs-mount.exe"
        $stagedNfsMount = Join-Path $InstallDir ".crab-nfs-mount.tmp.$PID.exe"
        Copy-Item -Path $sourceNfsMount -Destination $stagedNfsMount -Force
        if (Test-Path $nfsMountExe) {
            Remove-Item -Path $nfsMountExe -Force
        }
        Move-Item -Path $stagedNfsMount -Destination $nfsMountExe -Force
        Write-Info "Installed crab-nfs-mount.exe"

        $helperExe = Join-Path $InstallDir "git-remote-crab.exe"
        Copy-Item -Path $crabExe -Destination $helperExe -Force
        $oldWrapper = Join-Path $InstallDir "git-remote-crab.cmd"
        if (Test-Path $oldWrapper) {
            Remove-Item -Path $oldWrapper -Force
        }
        Write-Info "Created helper: git-remote-crab.exe"
    } finally {
        if (Test-Path $tmpDir) {
            Remove-Item -Recurse -Force $tmpDir
        }
    }

    # Add to PATH
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($currentPath -notlike "*$InstallDir*") {
        $newPath = if ([string]::IsNullOrWhiteSpace($currentPath)) { $InstallDir } else { "$InstallDir;$currentPath" }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Info "Added $InstallDir to user PATH"
    } else {
        Write-Info "$InstallDir is already in PATH"
    }

    Write-Host ""
    Write-Host "Installed crab $Version" -ForegroundColor Green
    Write-Host ""
    Write-Host "Restart your terminal to pick up the PATH change, then run:"
    Write-Host "  crab version" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Get started:"
    Write-Host "  crab init       " -NoNewline -ForegroundColor Cyan; Write-Host "— initialize a new repository"
    Write-Host "  crab clone      " -NoNewline -ForegroundColor Cyan; Write-Host "— clone an existing repository"
    Write-Host "  crab --help     " -NoNewline -ForegroundColor Cyan; Write-Host "— see all commands"
}

Install-Crab
