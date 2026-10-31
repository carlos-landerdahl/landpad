# Monta o pacote MSIX para enviar à Microsoft Store. Os três valores vêm do Partner Center,
# em Gerenciamento de produto > Identidade do produto. O pacote sai sem assinatura: a loja
# assina ao publicar. Passo a passo em LEIA-ME.md.
#
#   .\packaging\msix\build.ps1 -IdentityName "12345Carlos.Landpad" `
#       -Publisher "CN=ABCD1234-..." -PublisherDisplayName "Carlos Landerdahl"

param(
    [Parameter(Mandatory)] [string] $IdentityName,
    [Parameter(Mandatory)] [string] $Publisher,
    [Parameter(Mandatory)] [string] $PublisherDisplayName
)

$ErrorActionPreference = 'Stop'
Push-Location (Resolve-Path "$PSScriptRoot\..\..")
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo build falhou" }
    $version = (cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages[0].version

    $out = "target\msix"
    $stage = "$out\stage"
    Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
    New-Item -ItemType Directory "$stage\Assets" | Out-Null
    Copy-Item target\release\landpad.exe "$stage\Landpad.exe"

    # O Landpad é um app de janela: sem -Wait o PowerShell não espera ele terminar.
    Start-Process -Wait -NoNewWindow -FilePath target\release\landpad.exe -ArgumentList '--export-assets', "$stage\Assets"
    Remove-Item "$stage\Assets\icon-128.png", "$stage\Assets\landpad.ico"

    $manifest = (Get-Content packaging\msix\AppxManifest.xml -Raw).
        Replace('{{IDENTITY_NAME}}', $IdentityName).
        Replace('{{PUBLISHER}}', $Publisher).
        Replace('{{PUBLISHER_DISPLAY_NAME}}', $PublisherDisplayName).
        Replace('{{VERSION}}', "$version.0")
    Set-Content "$stage\AppxManifest.xml" $manifest -Encoding utf8

    $makeappx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Filter makeappx.exe -Recurse |
        Where-Object FullName -match '\\x64\\' | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $makeappx) { throw "makeappx.exe não encontrado: instale o Windows SDK" }

    $package = "$out\Landpad-$version.msix"
    & $makeappx.FullName pack /d $stage /p $package /o
    if ($LASTEXITCODE -ne 0) { throw "makeappx falhou" }
    "Pacote pronto: $package"
}
finally {
    Pop-Location
}
