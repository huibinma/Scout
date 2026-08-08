# BETA-41 文件层 fixture 一次性生成脚本（Windows PowerShell 5.1+）。
# 产出入仓，脚本仅为可复现记录——换机重跑字体渲染有像素差、OCR 结果可能漂移，以入仓生成物为准。
#
# 生成三类文件到 packages/evals/fixtures/enterprise-recall/files/：
#   1. 扫描版 PDF（image-only：System.Drawing 渲染合成文本 → JPEG → 手工组装 DCTDecode PDF，零外部依赖）
#   2. 文本层 PDF 对照（英文 Helvetica，验证 BETA-27 原路径不回归）
#   3. eml 邮件（RFC 5322 + base64 MIME，中文正文 UTF-8，含附件 part）
#
# 用法：powershell -ExecutionPolicy Bypass -File scripts/gen-enterprise-file-fixtures.ps1
# 全部文本为合成虚构（与 corpus.json 对应 doc_id 同文），零 PII。

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Join-Path $PSScriptRoot '..\packages\evals\fixtures\enterprise-recall\files'
foreach ($sub in 'lawfirm', 'audit', 'offboarding') {
    New-Item -ItemType Directory -Force (Join-Path $root $sub) | Out-Null
}

# ---------- 1. 页图渲染（模拟 150 DPI A4 扫描） ----------

$PageW = 1240
$PageH = 1754

function New-PageJpeg {
    param([string]$Title, [string]$Body, [int]$Quality)
    $bmp = New-Object System.Drawing.Bitmap($PageW, $PageH)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::White)
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias
    $titleFont = New-Object System.Drawing.Font('Microsoft YaHei', 30, [System.Drawing.FontStyle]::Bold)
    $bodyFont = New-Object System.Drawing.Font('Microsoft YaHei', 22)
    $black = [System.Drawing.Brushes]::Black
    if ($Title -ne '') {
        $tRect = New-Object System.Drawing.RectangleF(110, 120, ($PageW - 220), 160)
        $fmt = New-Object System.Drawing.StringFormat
        $fmt.Alignment = [System.Drawing.StringAlignment]::Center
        $g.DrawString($Title, $titleFont, $black, $tRect, $fmt)
    }
    $bRect = New-Object System.Drawing.RectangleF(110, 300, ($PageW - 220), ($PageH - 420))
    $g.DrawString($Body, $bodyFont, $black, $bRect)
    $g.Dispose()

    $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() |
        Where-Object { $_.MimeType -eq 'image/jpeg' }
    $ep = New-Object System.Drawing.Imaging.EncoderParameters(1)
    $ep.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]$Quality)
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, $codec, $ep)
    $bmp.Dispose()
    return $ms.ToArray()
}

# ---------- 2. image-only PDF 组装（DCTDecode，PDF 1.4） ----------

function New-ImageOnlyPdf {
    param([string]$OutPath, [System.Collections.ArrayList]$JpegPages)
    $enc = [System.Text.Encoding]::ASCII
    $ms = New-Object System.IO.MemoryStream
    $offsets = New-Object System.Collections.Generic.List[long]

    $wStr = { param($s) $b = $enc.GetBytes($s); $ms.Write($b, 0, $b.Length) }

    # header + 二进制标记行
    & $wStr "%PDF-1.4`n"
    $bin = [byte[]](0x25, 0xE2, 0xE3, 0xCF, 0xD3, 0x0A)
    $ms.Write($bin, 0, $bin.Length)

    $n = $JpegPages.Count
    # 对象号：1=Catalog 2=Pages；页 i（0 起）：Page=3+3i Content=4+3i Image=5+3i
    $kids = (0..($n - 1) | ForEach-Object { "$(3 + 3 * $_) 0 R" }) -join ' '

    $offsets.Add($ms.Position); & $wStr "1 0 obj`n<< /Type /Catalog /Pages 2 0 R >>`nendobj`n"
    $offsets.Add($ms.Position); & $wStr "2 0 obj`n<< /Type /Pages /Kids [$kids] /Count $n >>`nendobj`n"

    # 页尺寸：150 DPI → pt = px * 72 / 150
    $wPt = [math]::Round($PageW * 72.0 / 150.0, 2)
    $hPt = [math]::Round($PageH * 72.0 / 150.0, 2)

    for ($i = 0; $i -lt $n; $i++) {
        $pageObj = 3 + 3 * $i; $contObj = 4 + 3 * $i; $imgObj = 5 + 3 * $i
        $jpeg = [byte[]]$JpegPages[$i]

        $offsets.Add($ms.Position)
        & $wStr "$pageObj 0 obj`n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 $wPt $hPt] /Resources << /XObject << /Im$i $imgObj 0 R >> >> /Contents $contObj 0 R >>`nendobj`n"

        $content = "q $wPt 0 0 $hPt 0 0 cm /Im$i Do Q"
        $offsets.Add($ms.Position)
        & $wStr "$contObj 0 obj`n<< /Length $($content.Length) >>`nstream`n$content`nendstream`nendobj`n"

        $offsets.Add($ms.Position)
        & $wStr "$imgObj 0 obj`n<< /Type /XObject /Subtype /Image /Width $PageW /Height $PageH /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length $($jpeg.Length) >>`nstream`n"
        $ms.Write($jpeg, 0, $jpeg.Length)
        & $wStr "`nendstream`nendobj`n"
    }

    $xrefPos = $ms.Position
    $total = 2 + 3 * $n + 1
    & $wStr "xref`n0 $total`n0000000000 65535 f `n"
    foreach ($off in $offsets) { & $wStr ("{0:0000000000} 00000 n `n" -f $off) }
    & $wStr "trailer`n<< /Size $total /Root 1 0 R >>`nstartxref`n$xrefPos`n%%EOF`n"

    [System.IO.File]::WriteAllBytes($OutPath, $ms.ToArray())
    Write-Host ("{0}  ({1:N0} bytes, {2} page)" -f (Split-Path $OutPath -Leaf), $ms.Length, $n)
}

# ---------- 3. 文本层 PDF 对照（英文 Helvetica，BETA-27 原路径） ----------

function New-TextPdf {
    param([string]$OutPath, [string]$Title, [string[]]$Lines)
    $enc = [System.Text.Encoding]::ASCII
    $ms = New-Object System.IO.MemoryStream
    $offsets = New-Object System.Collections.Generic.List[long]
    $wStr = { param($s) $b = $enc.GetBytes($s); $ms.Write($b, 0, $b.Length) }

    & $wStr "%PDF-1.4`n"
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append("BT /F1 16 Tf 72 760 Td ($Title) Tj ET`n")
    $y = 720
    foreach ($line in $Lines) {
        $esc = $line -replace '\\', '\\\\' -replace '\(', '\(' -replace '\)', '\)'
        [void]$sb.Append("BT /F1 11 Tf 72 $y Td ($esc) Tj ET`n")
        $y -= 18
    }
    $content = $sb.ToString()

    $offsets.Add($ms.Position); & $wStr "1 0 obj`n<< /Type /Catalog /Pages 2 0 R >>`nendobj`n"
    $offsets.Add($ms.Position); & $wStr "2 0 obj`n<< /Type /Pages /Kids [3 0 R] /Count 1 >>`nendobj`n"
    $offsets.Add($ms.Position); & $wStr "3 0 obj`n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>`nendobj`n"
    $offsets.Add($ms.Position); & $wStr "4 0 obj`n<< /Length $($content.Length) >>`nstream`n$content`nendstream`nendobj`n"
    $offsets.Add($ms.Position); & $wStr "5 0 obj`n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>`nendobj`n"

    $xrefPos = $ms.Position
    & $wStr "xref`n0 6`n0000000000 65535 f `n"
    foreach ($off in $offsets) { & $wStr ("{0:0000000000} 00000 n `n" -f $off) }
    & $wStr "trailer`n<< /Size 6 /Root 1 0 R >>`nstartxref`n$xrefPos`n%%EOF`n"

    [System.IO.File]::WriteAllBytes($OutPath, $ms.ToArray())
    Write-Host ("{0}  ({1:N0} bytes, text layer)" -f (Split-Path $OutPath -Leaf), $ms.Length)
}

# ---------- 4. eml 生成（headers + base64 UTF-8 正文，含附件 part） ----------

function B64 { param([string]$s)
    $b64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($s))
    # 76 列折行（RFC 2045）
    ($b64 -split '(.{76})' | Where-Object { $_ -ne '' }) -join "`r`n"
}

function New-Eml {
    param([string]$OutPath, [string]$From, [string]$To, [string]$Subject, [string]$Body,
        [string]$AttachName = '', [string]$AttachBody = '')
    $subjB64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($Subject))
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append("From: $From`r`n")
    [void]$sb.Append("To: $To`r`n")
    [void]$sb.Append("Date: Thu, 2 Jul 2026 10:00:00 +0800`r`n")
    # 注意 ${} 包裹：裸写 "$subjB64?=" 时 PowerShell 会把 ? 贪婪并入变量名（$subjB64? 未定义 → 空）。
    [void]$sb.Append("Subject: =?UTF-8?B?${subjB64}?=`r`n")
    [void]$sb.Append("MIME-Version: 1.0`r`n")
    if ($AttachName -ne '') {
        $bd = 'scout-fixture-boundary'
        [void]$sb.Append("Content-Type: multipart/mixed; boundary=`"$bd`"`r`n`r`n")
        [void]$sb.Append("--$bd`r`n")
        [void]$sb.Append("Content-Type: text/plain; charset=utf-8`r`n")
        [void]$sb.Append("Content-Transfer-Encoding: base64`r`n`r`n")
        [void]$sb.Append((B64 $Body) + "`r`n")
        [void]$sb.Append("--$bd`r`n")
        [void]$sb.Append("Content-Type: text/plain; charset=utf-8; name=`"$AttachName`"`r`n")
        [void]$sb.Append("Content-Transfer-Encoding: base64`r`n")
        [void]$sb.Append("Content-Disposition: attachment; filename=`"$AttachName`"`r`n`r`n")
        [void]$sb.Append((B64 $AttachBody) + "`r`n")
        [void]$sb.Append("--$bd--`r`n")
    }
    else {
        [void]$sb.Append("Content-Type: text/plain; charset=utf-8`r`n")
        [void]$sb.Append("Content-Transfer-Encoding: base64`r`n`r`n")
        [void]$sb.Append((B64 $Body) + "`r`n")
    }
    [System.IO.File]::WriteAllText($OutPath, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
    Write-Host ("{0}  (eml)" -f (Split-Path $OutPath -Leaf))
}

# ---------- 5. 语料文本（与 corpus.json 对应 doc_id 同文） ----------

$judgment1 = @'
示例区人民法院
民事判决书

判决如下：
一、被告蓝湾贸易有限公司于本判决生效后十日内向原告北岭机械制造
有限公司支付迟延交货违约金，以合同总价为基数按日万分之五计算
九十三日；
二、驳回原告解除合同的诉讼请求。
'@

$judgment2 = @'
本院认为，技术图纸变更经双方邮件确认顺延三十日，其余迟延无正当
理由，被告应承担违约责任。案件受理费由被告负担约七成。

如不服本判决，可在判决书送达之日起十五日内向本院递交上诉状。

审判长（签名章）  书记员（签名章）
（本件与原本核对无异）
'@

$transcript = @'
北岭机械制造有限公司诉蓝湾贸易有限公司买卖合同纠纷一案，于虚构
日期在示例区人民法院第三法庭公开开庭审理。原告称被告未按约定时
间交付定制冲压设备，迟延超过九十日；被告辩称迟延系原告变更技术
图纸所致。双方围绕交货时间的约定与变更是否有效展开质证，审判长
归纳争议焦点为迟延责任的归属与违约金计算基数。
'@

$contract1 = @'
出卖人：蓝湾贸易有限公司
买受人：北岭机械制造有限公司

标的为定制冲压设备两台，货款支付方式为分期支付，签约后支付约三
成预付款，验收合格后支付尾款。
'@

$contract2 = @'
交货期为合同生效后一百二十日内，逾期每日按合同总价万分之五计付
违约金。

双方签章页（略）
'@

$invoice = @'
增值税专用发票

销售方：晨星办公用品有限公司
购买方：示例科技有限公司
货物名称：办公一体机  数量：二十台
发票号码：INV-示例-2201
备注：猎户座采购项目
'@

$goodsReceipt = @'
入库验收单

验收物资：办公一体机。应到二十台，实到十八台，缺两台由供应商出
具欠货说明、承诺两周内补齐。
验收结论：合格（按实到数量）。
仓管员与使用部门代表签字。
'@

$nda = @'
保密与知识产权协议（签署页）

员工承诺在职期间及离职后两年内对公司技术资料、客户信息、经营数
据负保密义务；因职务产生的成果归公司所有。

员工签名：李示例    公司（人事章）
'@

$handoverConfirm = @'
离职交接确认单

文档交接、系统权限移交、设备归还、财务无欠款四项均已完成。
交接人：李示例  接收人：王样本
备注：遗留缺陷三项已列入接收人工作计划，不影响离职手续办理。
'@

# ---------- 6. 产出 ----------

# 扫描版 PDF（image-only）。近重复组 g-law-01：同文三份、扫描参数不同。
$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '民事判决书（扫描）' -Body $judgment1 -Quality 85))
[void]$p.Add((New-PageJpeg -Title '' -Body $judgment2 -Quality 85))
New-ImageOnlyPdf (Join-Path $root 'lawfirm\e00005-judgment-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '民事判决书（扫描）' -Body $judgment1 -Quality 60))
[void]$p.Add((New-PageJpeg -Title '' -Body $judgment2 -Quality 60))
New-ImageOnlyPdf (Join-Path $root 'lawfirm\e00006-judgment-rescan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '民事判决书（复印再扫描）' -Body $judgment1 -Quality 45))
[void]$p.Add((New-PageJpeg -Title '' -Body $judgment2 -Quality 45))
New-ImageOnlyPdf (Join-Path $root 'lawfirm\e00007-judgment-copy-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '第一次开庭庭审笔录' -Body $transcript -Quality 80))
New-ImageOnlyPdf (Join-Path $root 'lawfirm\e00001-hearing-transcript-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '设备买卖合同' -Body $contract1 -Quality 85))
[void]$p.Add((New-PageJpeg -Title '' -Body $contract2 -Quality 85))
New-ImageOnlyPdf (Join-Path $root 'lawfirm\e00002-contract-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '增值税专用发票' -Body $invoice -Quality 85))
New-ImageOnlyPdf (Join-Path $root 'audit\e00041-invoice-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '入库验收单' -Body $goodsReceipt -Quality 80))
New-ImageOnlyPdf (Join-Path $root 'audit\e00043-goods-receipt-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '保密协议（签署页）' -Body $nda -Quality 85))
New-ImageOnlyPdf (Join-Path $root 'offboarding\e00078-nda-scan.pdf') $p

$p = New-Object System.Collections.ArrayList
[void]$p.Add((New-PageJpeg -Title '离职交接确认单' -Body $handoverConfirm -Quality 85))
New-ImageOnlyPdf (Join-Path $root 'offboarding\e00079-handover-confirm-scan.pdf') $p

# 文本层 PDF 对照（BETA-27 原路径不回归；英文 Helvetica）
New-TextPdf (Join-Path $root 'lawfirm\e00019-supply-agreement-summary.pdf') 'Supply Agreement Summary (Northridge Machinery)' @(
    'Summary prepared for the file: Northridge Machinery Ltd. purchased two',
    'custom stamping machines from Bluebay Trading Co. Delivery was due within',
    'one hundred and twenty days of signing, with liquidated damages accruing',
    'daily for late delivery. A thirty-day extension was agreed by email after',
    'the buyer revised the technical drawings. The remaining delay of roughly',
    'three months is the core of the dispute.'
)

# eml（正文与 corpus.json 对应 doc_id 同文；两封带附件 part）
New-Eml (Join-Path $root 'audit\e00035-approval-chain.eml') 'procurement@example.com' 'manager@example.com' `
    '猎户座采购项目——办公一体机与耗材采购审批' `
    '拟向晨星办公用品有限公司采购一体机二十台及全年耗材，报价见附件对比表，预算内。请各审批人在系统内三个工作日完成会签。后附三级审批的同意回复记录。' `
    'e00038-procurement-contract.txt' `
    '买方为公司行政部，卖方为晨星办公用品有限公司。标的：办公一体机二十台及全年耗材包。合同价款按报价单执行，货到验收合格后三十日内付款至合同指定账户。卖方承诺两年质保、次日上门维修。合同变更须双方书面确认，任何口头承诺不构成合同内容。'

New-Eml (Join-Path $root 'audit\e00036-payment-hold.eml') 'finance@example.com' 'procurement@example.com' `
    '关于猎户座项目第二笔付款的疑点' `
    '本笔付款申请的收款账户与合同约定账户不一致，且未附入库验收单。另外单价较上季度同类采购上浮约一成五。在补齐验收材料并说明账户变更原因前，暂缓付款。请经办人书面回复。'

New-Eml (Join-Path $root 'audit\e00050-tipline.eml') 'tipline@example.com' 'audit@example.com' `
    '关于行政采购的情况反映' `
    '反映采购经办与晨星办公用品的销售负责人过从甚密，多次私下聚餐；本次一体机采购价格明显偏高，且验收数量与合同不符仍照常付款。请审计部门核实。附言称必要时可提供聚餐时间地点线索。'

New-Eml (Join-Path $root 'audit\e00037-vendor-quotation.eml') 'sales@example.com' 'procurement@example.com' `
    'Quotation for Project Orion office equipment' `
    'Please find our quotation for twenty all-in-one machines plus a twelve-month consumables package. The unit price includes delivery and on-site setup. This offer is valid for thirty days. We can shorten the lead time to two weeks if the purchase order is confirmed within this week.'

New-Eml (Join-Path $root 'offboarding\e00074-handover.eml') 'lishili@example.com' 'team@example.com' `
    '离职交接安排' `
    '本人最后工作日为月底，交接按三条线进行：灯塔项目文档与代码由王样本接手，鲲鹏结算值班从下周起移交，外部联系人本周内拉群介绍。知识库已按目录整理，遗留问题单独成文。感谢大家。' `
    'e00076-db-account-list.txt' `
    '鲲鹏结算相关账号清单：应用连接账号两个（读写与只读）、巡检账号一个、报表拉数账号一个。清单只列账号名与用途，不含任何口令；口令一律走密码保管系统移交，交接完成后原持有人权限由 IT 回收。个人名下不得留存共享账号。'

New-Eml (Join-Path $root 'offboarding\e00093-open-bugs.eml') 'lishili@example.com' 'wangyangben@example.com' `
    '离职前遗留问题说明' `
    '三个未结缺陷——一、对账文件偶发迟到时告警重复发送，烦扰值班；二、限流规则热更新在极端并发下有一次未生效记录，未复现；三、报表库一张宽表查询慢，需加索引但要等月结窗口后操作。明细与复现步骤见附件。' `
    'e00094-open-bugs-detail.txt' `
    '缺陷一：告警重复——对账文件迟到触发的告警未做收敛，同一批次可重复发送多条，建议按批次号去重；缺陷二：限流热更新偶发未生效——疑似配置下发与规则加载竞态；缺陷三：报表宽表慢查询——执行计划全表扫描，建议对账期字段建复合索引，改动需在月结窗口外执行。'

Write-Host "`n全部生成完毕 → $root"
