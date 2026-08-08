# Scout BETA-03：Windows.Media.Ocr 单图 OCR（经 PowerShell WinRT）。
# 图片路径读自环境变量 SCOUT_OCR_IMAGE（脚本不插值用户数据 → 杜绝注入）。
# 识别文字打印到 stdout（UTF-8）；任何失败写 stderr + 非 0 退出码。
#
# 【关键结构约束】类型加载（Add-Type / WinRT 累加器注册）必须是【顶层语句】，
# PowerShell 逐条编译执行顶层语句 → 每条类型加载先于后续语句编译。若把
# [System.WindowsRuntimeSystemExtensions] 等字面量与 Add-Type 放进同一个 try{} 块，
# 整块会一次性编译，导致类型字面量在 Add-Type 之前解析 → "Unable to find type"。
# 故错误处理用 trap（顶层）而非 try/catch。脚本经 Rust 端 -EncodedCommand 调用。
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$img = $env:SCOUT_OCR_IMAGE
if ([string]::IsNullOrEmpty($img)) { [Console]::Error.WriteLine('SCOUT_OCR_IMAGE not set'); exit 1 }
if (-not (Test-Path -LiteralPath $img)) { [Console]::Error.WriteLine("image not found: $img"); exit 1 }

# WinRT 异步 -> .NET Task 的 await 辅助（PS 5.1 通用写法）。
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Await($WinRtTask, $ResultType) {
    $asTask = $asTaskGeneric.MakeGenericMethod($ResultType)
    $netTask = $asTask.Invoke($null, @($WinRtTask))
    $netTask.Wait(-1) | Out-Null
    $netTask.Result
}

[Windows.Storage.StorageFile,Windows.Storage,ContentType=WindowsRuntime] | Out-Null
[Windows.Graphics.Imaging.BitmapDecoder,Windows.Graphics.Imaging,ContentType=WindowsRuntime] | Out-Null
[Windows.Graphics.Imaging.BitmapTransform,Windows.Graphics.Imaging,ContentType=WindowsRuntime] | Out-Null
[Windows.Graphics.Imaging.ExifOrientationMode,Windows.Graphics.Imaging,ContentType=WindowsRuntime] | Out-Null
[Windows.Graphics.Imaging.ColorManagementMode,Windows.Graphics.Imaging,ContentType=WindowsRuntime] | Out-Null
[Windows.Media.Ocr.OcrEngine,Windows.Media.Ocr,ContentType=WindowsRuntime] | Out-Null

$engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if ($null -eq $engine) { [Console]::Error.WriteLine('no OCR recognizer language available'); exit 1 }

$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($img)) ([Windows.Storage.StorageFile])
$stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
$decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])

# 超大图（宽或高 > OcrEngine.MaxImageDimension）RecognizeAsync 会直接报
# "The parameter is incorrect."。等比缩到上限内再识别，而不是整图计 failed。
$maxDim = [Windows.Media.Ocr.OcrEngine]::MaxImageDimension
$origW = $decoder.PixelWidth
$origH = $decoder.PixelHeight
if ($origW -gt $maxDim -or $origH -gt $maxDim) {
    $scale = [Math]::Min([double]$maxDim / $origW, [double]$maxDim / $origH)
    $transform = New-Object Windows.Graphics.Imaging.BitmapTransform
    $transform.ScaledWidth = [uint32][Math]::Max(1.0, [Math]::Floor($origW * $scale))
    $transform.ScaledHeight = [uint32][Math]::Max(1.0, [Math]::Floor($origH * $scale))
    $bitmap = Await ($decoder.GetSoftwareBitmapAsync(
        $decoder.BitmapPixelFormat,
        $decoder.BitmapAlphaMode,
        $transform,
        [Windows.Graphics.Imaging.ExifOrientationMode]::RespectExifOrientation,
        [Windows.Graphics.Imaging.ColorManagementMode]::DoNotColorManage
    )) ([Windows.Graphics.Imaging.SoftwareBitmap])
} else {
    $bitmap = Await ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
}

$result = Await ($engine.RecognizeAsync($bitmap)) ([Windows.Media.Ocr.OcrResult])
[Console]::Out.Write($result.Text)
exit 0
