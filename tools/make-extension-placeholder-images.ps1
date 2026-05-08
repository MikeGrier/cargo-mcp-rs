# Generate placeholder PNGs for the VS Code extension Marketplace listing.
# Replace these with real screenshots / icon before publishing for production.
#
# Outputs:
#   crates/cargo-mcp/extension/images/icon.png                (128x128 listing thumbnail)
#   crates/cargo-mcp/extension/images/streaming-build.png     (1200x600 screenshot)
#   crates/cargo-mcp/extension/images/clippy-suggestions.png  (1200x600 screenshot)
#   crates/cargo-mcp/extension/images/diagnostics.png         (1200x600 screenshot)

param(
    [string]$OutDir = "crates/cargo-mcp/extension/images"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not (Test-Path $OutDir)) {
    New-Item -ItemType Directory -Path $OutDir | Out-Null
}

function New-PlaceholderPng {
    param(
        [string]$Path,
        [int]$Width,
        [int]$Height,
        [string]$Title,
        [string]$Subtitle = "",
        [string]$BgColor = "#2D2D30",
        [string]$AccentColor = "#CE422B"
    )
    $bmp = New-Object System.Drawing.Bitmap($Width, $Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit

    $bg = [System.Drawing.ColorTranslator]::FromHtml($BgColor)
    $g.Clear($bg)

    $accent = [System.Drawing.ColorTranslator]::FromHtml($AccentColor)
    $stripeBrush = New-Object System.Drawing.SolidBrush($accent)
    $stripeHeight = [Math]::Max(4, [int]($Height * 0.04))
    $g.FillRectangle($stripeBrush, 0, 0, $Width, $stripeHeight)

    $titleSize = [Math]::Max(14, [int]($Height * 0.10))
    $titleFont = New-Object System.Drawing.Font("Segoe UI", $titleSize, [System.Drawing.FontStyle]::Bold)
    $titleBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
    $sf = New-Object System.Drawing.StringFormat
    $sf.Alignment = [System.Drawing.StringAlignment]::Center
    $sf.LineAlignment = [System.Drawing.StringAlignment]::Center

    $titleY = if ($Subtitle) { [int]($Height * 0.40) } else { [int]($Height * 0.50) }
    $titleRect = New-Object System.Drawing.RectangleF([float]0, [float]($titleY - $titleSize), [float]$Width, [float]($titleSize * 2))
    $g.DrawString($Title, $titleFont, $titleBrush, $titleRect, $sf)

    if ($Subtitle) {
        $subSize = [Math]::Max(10, [int]($Height * 0.05))
        $subFont = New-Object System.Drawing.Font("Segoe UI", $subSize, [System.Drawing.FontStyle]::Regular)
        $subBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(180, 180, 180))
        $subRect = New-Object System.Drawing.RectangleF([float]0, [float]([int]($Height * 0.62)), [float]$Width, [float]($subSize * 3))
        $g.DrawString($Subtitle, $subFont, $subBrush, $subRect, $sf)
        $subFont.Dispose()
        $subBrush.Dispose()
    }

    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $titleFont.Dispose()
    $titleBrush.Dispose()
    $stripeBrush.Dispose()
    $g.Dispose()
    $bmp.Dispose()
    Write-Host ("Wrote {0} ({1}x{2})" -f $Path, $Width, $Height)
}

New-PlaceholderPng -Path (Join-Path $OutDir "icon.png") -Width 128 -Height 128 -Title "MCP" -Subtitle "cargo"
New-PlaceholderPng -Path (Join-Path $OutDir "streaming-build.png") -Width 1200 -Height 600 -Title "Streaming build" -Subtitle "screenshot placeholder"
New-PlaceholderPng -Path (Join-Path $OutDir "clippy-suggestions.png") -Width 1200 -Height 600 -Title "Clippy suggestions" -Subtitle "screenshot placeholder"
New-PlaceholderPng -Path (Join-Path $OutDir "diagnostics.png") -Width 1200 -Height 600 -Title "Precise diagnostics" -Subtitle "screenshot placeholder"
