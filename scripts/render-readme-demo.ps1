[CmdletBinding()]
param(
    [string]$OutputPath = "assets\demo.gif",
    [ValidateRange(100, 200)]
    [int]$Columns = 120,
    [ValidateRange(30, 70)]
    [int]$Rows = 40
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$output = if ([IO.Path]::IsPathRooted($OutputPath)) {
    $OutputPath
} else {
    Join-Path $repositoryRoot $OutputPath
}
$output = [IO.Path]::GetFullPath($output)

$targetRoot = if ($env:CARGO_TARGET_DIR) {
    [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
} else {
    Join-Path $repositoryRoot "target"
}
$gallery = Join-Path $targetRoot "debug\examples\ui_gallery.exe"

Push-Location $repositoryRoot
try {
    & cargo build --locked --example ui_gallery
    if ($LASTEXITCODE -ne 0) {
        throw "ui_gallery build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

if (-not (Test-Path -LiteralPath $gallery -PathType Leaf)) {
    throw "ui_gallery executable was not found at $gallery"
}

Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase

$culture = [Globalization.CultureInfo]::InvariantCulture
$typeface = [Windows.Media.Typeface]::new("Cascadia Mono")
$fontSize = 13.0
$pixelsPerDip = 1.0
$sample = [Windows.Media.FormattedText]::new(
    "M",
    $culture,
    [Windows.FlowDirection]::LeftToRight,
    $typeface,
    $fontSize,
    [Windows.Media.Brushes]::White,
    $pixelsPerDip
)
$cellWidth = [Math]::Ceiling($sample.WidthIncludingTrailingWhitespace)
$lineHeight = [Math]::Ceiling($sample.Height * 1.18)
$padding = 18
$chromeHeight = 34
$bitmapWidth = [int]($padding * 2 + $cellWidth * $Columns)
$bitmapHeight = [int]($padding * 2 + $chromeHeight + $lineHeight * $Rows)

$backgroundBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(11, 15, 20))
$chromeBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(24, 29, 36))
$textBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(207, 216, 227))
$brightBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(241, 245, 249))
$accentBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(65, 214, 208))
$mutedBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(117, 129, 145))
$redBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(255, 107, 107))
$yellowBrush = [Windows.Media.SolidColorBrush]::new([Windows.Media.Color]::FromRgb(250, 204, 21))

function New-Text {
    param(
        [string]$Value,
        [Windows.Media.Brush]$Brush
    )

    [Windows.Media.FormattedText]::new(
        $Value,
        $culture,
        [Windows.FlowDirection]::LeftToRight,
        $typeface,
        $fontSize,
        $Brush,
        $pixelsPerDip
    )
}

function Get-LineBrush {
    param(
        [string]$Line,
        [int]$Index
    )

    if ($Line -match '(?i)error|failed') {
        return $redBrush
    }
    if ($Line -match '(?i)thinking|Pixel|MCP|LSP|DEcode|agent') {
        return $accentBrush
    }
    if ($Line -match '(?i)approval|review|warning|confirm') {
        return $yellowBrush
    }
    if ($Index -lt 2) {
        return $brightBrush
    }
    if ([string]::IsNullOrWhiteSpace($Line)) {
        return $mutedBrush
    }
    return $textBrush
}

function New-GalleryBitmap {
    param(
        [string]$Screen,
        [string]$Locale,
        [string]$Caption
    )

    $raw = & $gallery $Columns $Rows $Screen $Locale dark
    if ($LASTEXITCODE -ne 0) {
        throw "ui_gallery failed for $Screen/$Locale with exit code $LASTEXITCODE"
    }
    $lines = @($raw | Select-Object -Skip 1)

    $visual = [Windows.Media.DrawingVisual]::new()
    $drawing = $visual.RenderOpen()
    try {
        $drawing.DrawRectangle($backgroundBrush, $null, [Windows.Rect]::new(0, 0, $bitmapWidth, $bitmapHeight))
        $drawing.DrawRectangle($chromeBrush, $null, [Windows.Rect]::new(0, 0, $bitmapWidth, $chromeHeight))
        $drawing.DrawEllipse([Windows.Media.Brushes]::IndianRed, $null, [Windows.Point]::new(18, 17), 5, 5)
        $drawing.DrawEllipse([Windows.Media.Brushes]::Goldenrod, $null, [Windows.Point]::new(34, 17), 5, 5)
        $drawing.DrawEllipse([Windows.Media.Brushes]::MediumSeaGreen, $null, [Windows.Point]::new(50, 17), 5, 5)
        $drawing.DrawText((New-Text $Caption $brightBrush), [Windows.Point]::new(68, 8))

        for ($index = 0; $index -lt $Rows; $index++) {
            $line = if ($index -lt $lines.Count) { [string]$lines[$index] } else { "" }
            if ($line.Length -gt $Columns) {
                $line = $line.Substring(0, $Columns)
            }
            $brush = Get-LineBrush $line $index
            $drawing.DrawText(
                (New-Text $line $brush),
                [Windows.Point]::new($padding, $chromeHeight + $padding + $index * $lineHeight)
            )
        }
    } finally {
        $drawing.Close()
    }

    $bitmap = [Windows.Media.Imaging.RenderTargetBitmap]::new(
        $bitmapWidth,
        $bitmapHeight,
        96,
        96,
        [Windows.Media.PixelFormats]::Pbgra32
    )
    $bitmap.Render($visual)
    $bitmap.Freeze()
    return $bitmap
}

$scenes = @(
    @{ Screen = "chat"; Locale = "en"; Caption = "DEcode - coding session"; Delay = 110 },
    @{ Screen = "mcp-add"; Locale = "en"; Caption = "DEcode - MCP configuration"; Delay = 100 },
    @{ Screen = "lsp-add"; Locale = "en"; Caption = "DEcode - language intelligence"; Delay = 100 },
    @{ Screen = "mcp"; Locale = "en"; Caption = "DEcode - managed connections"; Delay = 110 }
)

$encoder = [Windows.Media.Imaging.GifBitmapEncoder]::new()
for ($index = 0; $index -lt $scenes.Count; $index++) {
    $scene = $scenes[$index]
    $bitmap = New-GalleryBitmap $scene.Screen $scene.Locale $scene.Caption
    $metadata = [Windows.Media.Imaging.BitmapMetadata]::new("gif")
    $metadata.SetQuery("/grctlext/Delay", [UInt16]$scene.Delay)
    $metadata.SetQuery("/grctlext/Disposal", [byte]2)
    $encoder.Frames.Add([Windows.Media.Imaging.BitmapFrame]::Create($bitmap, $null, $metadata, $null))
}

$outputDirectory = Split-Path -Parent $output
[IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
$stream = [IO.File]::Open($output, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $encoder.Save($stream)
} finally {
    $stream.Dispose()
}

$gifBytes = [IO.File]::ReadAllBytes($output)
$frameIndex = 0
for ($offset = 0; $offset -le $gifBytes.Length - 8; $offset++) {
    $isGraphicControl =
        $gifBytes[$offset] -eq 0x21 -and
        $gifBytes[$offset + 1] -eq 0xF9 -and
        $gifBytes[$offset + 2] -eq 0x04
    if (-not $isGraphicControl) {
        continue
    }

    if ($frameIndex -ge $scenes.Count) {
        throw "GIF contains more frames than expected"
    }
    $delay = [UInt16]$scenes[$frameIndex].Delay
    $gifBytes[$offset + 3] = [byte]0x08
    $gifBytes[$offset + 4] = [byte]($delay -band 0xFF)
    $gifBytes[$offset + 5] = [byte](($delay -shr 8) -band 0xFF)
    $frameIndex++
}

if ($frameIndex -ne $scenes.Count) {
    throw "GIF contains $frameIndex frames; expected $($scenes.Count)"
}

$loopExtension = [byte[]](
    @(0x21, 0xFF, 0x0B) +
    [Text.Encoding]::ASCII.GetBytes("NETSCAPE2.0") +
    @(0x03, 0x01, 0x00, 0x00, 0x00)
)
$extensionOffset = 13
if (($gifBytes[10] -band 0x80) -ne 0) {
    $colorCount = 1 -shl (($gifBytes[10] -band 0x07) + 1)
    $extensionOffset += 3 * $colorCount
}

$patched = [IO.MemoryStream]::new($gifBytes.Length + $loopExtension.Length)
try {
    $patched.Write($gifBytes, 0, $extensionOffset)
    $patched.Write($loopExtension, 0, $loopExtension.Length)
    $patched.Write($gifBytes, $extensionOffset, $gifBytes.Length - $extensionOffset)
    [IO.File]::WriteAllBytes($output, $patched.ToArray())
} finally {
    $patched.Dispose()
}

$decoder = [Windows.Media.Imaging.GifBitmapDecoder]::new(
    [Uri]::new($output),
    [Windows.Media.Imaging.BitmapCreateOptions]::PreservePixelFormat,
    [Windows.Media.Imaging.BitmapCacheOption]::OnLoad
)
if ($decoder.Frames.Count -ne $scenes.Count) {
    throw "Saved GIF contains $($decoder.Frames.Count) frames; expected $($scenes.Count)"
}
foreach ($frame in $decoder.Frames) {
    if ([int]$frame.Metadata.GetQuery("/grctlext/Delay") -le 0) {
        throw "Saved GIF contains a frame without a display delay"
    }
}

Write-Output "Wrote $output ($bitmapWidth x $bitmapHeight, $($scenes.Count) frames)"
