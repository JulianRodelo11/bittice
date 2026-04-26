# Bittice installer for Windows
$repo = "JulianRodelo11/bittice"
$binaryName = "bittice.exe"
$installDir = "$HOME\AppData\Local\Microsoft\WindowsApps"

Write-Host "--- Bittice installer (Windows) ---" -ForegroundColor Blue

# 1. Architecture (standalone asset name; bundle currently ships x86_64 bittice.exe)
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") { "x86_64" } elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
$standaloneTarget = "bittice-windows-x86_64.exe"

# 2. Latest release tag
Write-Host "Fetching latest release from GitHub..."
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
$latestTag = $release.tag_name

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

Move-Item -Path $tempFile -Destination "$installDir\$binaryName" -Force

Write-Host "Bittice ($latestTag) installed successfully." -ForegroundColor Green
if ($usedBundle) { Write-Host "Installed from OS bundle: $bundle" -ForegroundColor DarkGray }
Write-Host "Restart your terminal and run: bittice --help" -ForegroundColor Blue
