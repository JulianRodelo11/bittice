# Bittice installer for Windows
# Env (optional): BITTICE_VERSION (e.g. v0.1.64), BITTICE_INSTALL_DIR (full path for bittice.exe)
#
# One-liner (cmd.exe — keep | iex INSIDE the quotes):
#   powershell -c "irm https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.ps1 | iex"
# If ExecutionPolicy blocks iex:
#   powershell -NoProfile -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.ps1 | iex"

$repo = "JulianRodelo11/bittice"
$binaryName = "bittice.exe"
$installDir = if ($env:BITTICE_INSTALL_DIR) {
    $env:BITTICE_INSTALL_DIR.Trim().TrimEnd('\')
} else {
    Join-Path $env:LOCALAPPDATA "Programs\Bittice"
}

function Notify-EnvironmentChange {
    try {
        if (-not ('BitticeInstallerEnvNotify' -as [type])) {
            Add-Type @'
public class BitticeInstallerEnvNotify {
    [System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Auto)]
    public static extern System.IntPtr SendMessageTimeout(
        System.IntPtr hWnd, int Msg, System.IntPtr wParam, string lParam,
        int fuFlags, int uTimeout, out System.IntPtr lpdwResult);
}
'@
        }
        $HWND_BROADCAST = [IntPtr]0xffff
        $WM_SETTINGCHANGE = 0x001a
        [IntPtr]$res = [IntPtr]::Zero
        [void][BitticeInstallerEnvNotify]::SendMessageTimeout(
            $HWND_BROADCAST, $WM_SETTINGCHANGE, [IntPtr]::Zero,
            "Environment", 2, 5000, [ref]$res)
    } catch {
        # Non-fatal; new shells may still need logoff/on
    }
}

function Add-BitticeToUserPath {
    param([string]$BinDir)
    $add = $BinDir.TrimEnd('\')
    try {
        if (Test-Path -LiteralPath $add) {
            $add = (Resolve-Path -LiteralPath $add).Path.TrimEnd('\')
        }
    } catch { }

    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $pr = New-Object Security.Principal.WindowsPrincipal($id)
    if ($pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Host "Running as Administrator: user PATH is for $($id.Name). Prefer a normal shell if you usually work without admin." -ForegroundColor Yellow
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $parts = @(
        $userPath -split ';' |
        ForEach-Object { $_.Trim() } |
        Where-Object { $_ }
    )
    foreach ($p in $parts) {
        $cmp = $p.TrimEnd('\')
        try {
            if (Test-Path -LiteralPath $cmp) { $cmp = (Resolve-Path -LiteralPath $cmp).Path.TrimEnd('\') }
        } catch { }
        if ([string]::Equals($add, $cmp, [StringComparison]::OrdinalIgnoreCase)) {
            Write-Host "Install directory already on your user PATH." -ForegroundColor DarkGray
            $env:Path = "$add;$env:Path"
            return
        }
    }

    $joined = if ($parts.Count -gt 0) { "$userPath;$add" } else { $add }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')

    # Mirror User PATH as REG_EXPAND_SZ (some profiles only read this reliably)
    try {
        Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name 'Path' -Value $joined -Type ExpandString -Force
    } catch {
        Write-Host "Could not write HKCU:\Environment\Path (try signing out and in)." -ForegroundColor Yellow
    }

    Notify-EnvironmentChange

    $snap = [Environment]::GetEnvironmentVariable('Path', 'User')
    $ok = $false
    foreach ($segment in ($snap -split ';')) {
        $t = $segment.Trim().TrimEnd('\')
        if ([string]::Equals($t, $add, [StringComparison]::OrdinalIgnoreCase)) { $ok = $true; break }
    }
    if (-not $ok) {
        Write-Host "PATH may not have saved correctly. Add this folder in Settings: $add" -ForegroundColor Red
    } else {
        Write-Host "Added to user PATH: $add" -ForegroundColor Green
    }
    $env:Path = "$add;$env:Path"
}

Write-Host "--- Bittice installer (Windows) ---" -ForegroundColor Blue

# 1. Architecture (standalone asset name; bundle currently ships x86_64 bittice.exe)
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") { "x86_64" } elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
$standaloneTarget = "bittice-windows-x86_64.exe"

# 2. Release tag: pinned via BITTICE_VERSION or latest from GitHub
$latestTag = $null
if ($env:BITTICE_VERSION) {
    $latestTag = $env:BITTICE_VERSION.Trim()
    Write-Host "Using release tag from BITTICE_VERSION: $latestTag" -ForegroundColor Cyan
} else {
    Write-Host "Fetching latest release from GitHub..."
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
    $latestTag = $release.tag_name
}

if (-not $latestTag) {
    Write-Host "No published release found." -ForegroundColor Red
    exit 1
}

Write-Host "Installing $latestTag (prefer OS bundle; arch hint: $arch)..." -ForegroundColor Green

# 3. Download: try per-OS zip first, then legacy standalone .exe
$tempFile = "$env:TEMP\bittice_temp.exe"
$bundle = "bittice-$latestTag-windows.zip"
$bundleUrl = "https://github.com/$repo/releases/download/$latestTag/$bundle"
$usedBundle = $false

try {
    $zipPath = "$env:TEMP\bittice_windows_bundle.zip"
    Invoke-WebRequest -Uri $bundleUrl -OutFile $zipPath -ErrorAction Stop
    $expand = "$env:TEMP\bittice_windows_extract"
    if (Test-Path $expand) { Remove-Item -Recurse -Force $expand }
    Expand-Archive -Path $zipPath -DestinationPath $expand -Force
    $exePath = Join-Path $expand "bittice.exe"
    if (-not (Test-Path $exePath)) { throw "bittice.exe not in bundle" }
    Copy-Item -Path $exePath -Destination $tempFile -Force
    Remove-Item -Recurse -Force $expand -ErrorAction SilentlyContinue
    Remove-Item $zipPath -ErrorAction SilentlyContinue
    $usedBundle = $true
} catch {
    Write-Host "Bundle not available ($bundle); using standalone $standaloneTarget ..." -ForegroundColor Yellow
    $downloadUrl = "https://github.com/$repo/releases/download/$latestTag/$standaloneTarget"
    Invoke-WebRequest -Uri $downloadUrl -OutFile $tempFile
}

# 4. Install
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

Move-Item -Path $tempFile -Destination (Join-Path $installDir $binaryName) -Force

Add-BitticeToUserPath -BinDir $installDir

Write-Host "Bittice ($latestTag) installed successfully." -ForegroundColor Green
Write-Host "Installed to: $(Join-Path $installDir $binaryName)" -ForegroundColor DarkGray
if ($usedBundle) { Write-Host "Installed from OS bundle: $bundle" -ForegroundColor DarkGray }
Write-Host "Close this window, open a new terminal, then run: bittice --help" -ForegroundColor Blue
Write-Host "(If cmd still does not find it, sign out and back in once so PATH refreshes.)" -ForegroundColor DarkGray
