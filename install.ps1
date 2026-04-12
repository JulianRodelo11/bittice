# Bittice Installer for Windows
$repo = "JulianRodelo11/bittice"
$binaryName = "bittice.exe"
$installDir = "$HOME\AppData\Local\Microsoft\WindowsApps"

Write-Host "--- Instalador de Bittice para Windows ---" -ForegroundColor Blue

# 1. Detectar Arquitectura
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") { "x86_64" } else { "aarch64" }
$target = "bittice-windows-$arch.exe"

# 2. Obtener última versión
Write-Host "Buscando la última versión en GitHub..."
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
$latestTag = $release.tag_name

if (-not $latestTag) {
    Write-Host "No se encontró ninguna versión publicada." -ForegroundColor Red
    exit
}

Write-Host "Instalando versión $latestTag ($arch)..." -ForegroundColor Green

# 3. Descargar
$downloadUrl = "https://github.com/$repo/releases/download/$latestTag/$target"
$tempFile = "$env:TEMP\bittice_temp.exe"

Invoke-WebRequest -Uri $downloadUrl -OutFile $tempFile

# 4. Instalar
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force
}

Move-Item -Path $tempFile -Destination "$installDir\$binaryName" -Force

Write-Host "¡Bittice ($latestTag) instalado correctamente!" -ForegroundColor Green
Write-Host "Reinicia tu terminal y escribe 'bittice --help' para comenzar." -ForegroundColor Blue
