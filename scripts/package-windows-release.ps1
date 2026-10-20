param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,

    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"

if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "Invalid package version: $Version"
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$binary = (Resolve-Path -LiteralPath $BinaryPath).Path
$output = [System.IO.Path]::GetFullPath($OutputDirectory)
$assetName = "qin-v" + $Version + "-windows-x86_64"
$archive = Join-Path $output "$assetName.zip"
$stagingRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("qin-windows-package-" + [guid]::NewGuid().ToString("N"))
$packageRoot = Join-Path $stagingRoot $assetName

New-Item -ItemType Directory -Force -Path $output, $packageRoot | Out-Null

try {
    Copy-Item -LiteralPath $binary -Destination (Join-Path $packageRoot "qin.exe")
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "README.md") -Destination $packageRoot
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "LICENSE") -Destination $packageRoot
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "SECURITY.md") -Destination $packageRoot
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "assets/config.example.toml") -Destination (Join-Path $packageRoot "config.toml.example")

    Compress-Archive -LiteralPath $packageRoot -DestinationPath $archive -CompressionLevel Optimal -Force
    if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "Windows archive was not created: $archive"
    }
}
finally {
    Remove-Item -LiteralPath $stagingRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Created $archive"
