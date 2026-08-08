# Scout BETA-64 T8：Windows.Media.Ocr 常驻 worker（经 PowerShell WinRT）。
# 与 win_ocr.ps1 做同样的类型加载 + OCR，但类型加载只在进程启动时做一次——
# PowerShell 冷启动 + WinRT 程序集/类型解析是每张图片重复付出的固定开销主因
# （B4），常驻进程把这份开销从「每图一次」降到「每轮索引一次」。
#
# 协议（逐行，UTF-8，无状态、无并发——调用方保证同一时刻只有一个未完成请求）：
#   请求：stdin 每行一个图片绝对路径（不含首尾空白），stdin 关闭（EOF）→ 优雅退出。
#   响应：stdout 每行恰好对应一次请求——
#     成功：`OK:<base64(UTF-8 识别文字)>`
#     失败：`ERR:<原因，已去除内部换行，单行>`
#   响应正文 base64 编码是关键设计：识别文字可能含任意 Unicode（含换行），若不编码，
#   跨行文本会破坏"每行一响应"的 framing 协议；base64 输出恒不含换行，framing 永远
#   明确，Rust 端 `BufRead::lines()` 逐行读取零歧义。
#
# 【关键结构约束】同 win_ocr.ps1：WinRT 类型加载必须是顶层语句（PowerShell 逐条编译
# 执行顶层语句，混进 try{} 块会导致类型字面量在 Add-Type 之前被解析、报
# "Unable to find type"），故启动期错误用 trap（顶层）捕获；循环体内单图错误改用
# try/catch（此时类型已加载完毕，可以安全用，且必须精确到单图——不能让一张坏图
# 杀死整个常驻进程，那样反而比 T8 之前的一次性进程更脆弱）。
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

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
$maxDim = [Windows.Media.Ocr.OcrEngine]::MaxImageDimension

function Recognize-One([string]$img) {
    if (-not (Test-Path -LiteralPath $img)) { throw "image not found: $img" }
    $file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($img)) ([Windows.Storage.StorageFile])
    $stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
    $decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])

    # 超大图（宽或高 > OcrEngine.MaxImageDimension）RecognizeAsync 会直接报
    # "The parameter is incorrect."。等比缩到上限内再识别，而不是整图计 failed
    # （与 win_ocr.ps1 单图版逻辑逐字节一致）。
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
    return $result.Text
}

while ($true) {
    $line = [Console]::In.ReadLine()
    if ($null -eq $line) { break }  # stdin EOF（调用方 drop 了 ChildStdin）→ 优雅退出
    # 不对 $line 做 .Trim()——ReadLine() 本身已经剥掉了行终止符（CR/LF/CRLF），
    # 剩下的就是 Rust 端 writeln! 写入的原始路径；对它再 Trim 会把路径末尾合法的
    # 空格字符（罕见但确实存在，如非 Explorer API 创建的文件）当协议空白一起吃掉，
    # 导致 Test-Path 用被裁剪过的路径去找一个不存在的文件、误报"image not found"
    # ——与一次性版本 win_ocr.ps1（直接读 $env:SCOUT_OCR_IMAGE、不 Trim）行为不一致。
    $img = $line
    if ([string]::IsNullOrEmpty($img)) { continue }
    try {
        $text = Recognize-One $img
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($text)
        $b64 = [Convert]::ToBase64String($bytes)
        [Console]::Out.WriteLine("OK:$b64")
    } catch {
        # 单图失败只回一行 ERR、继续循环处理下一张——不能让一张坏图（畸形文件 /
        # 权限问题 / OneDrive 占位符等）杀死整个常驻进程。
        $msg = ($_.Exception.Message -replace "`r`n|`n|`r", ' ')
        [Console]::Out.WriteLine("ERR:$msg")
    }
    [Console]::Out.Flush()
}
exit 0
