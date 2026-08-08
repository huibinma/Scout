from __future__ import annotations

import csv
import os
import textwrap
import zipfile
from datetime import datetime
from pathlib import Path
from xml.sax.saxutils import escape

from PIL import Image, ImageDraw, ImageFont
from reportlab.lib.pagesizes import A4
from reportlab.lib.units import mm
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas


ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / "test-materials" / "enterprise-scenarios-raw"
OUT = BASE / "real-formats"


def ensure_parent(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)


def write_zip(path: Path, files: dict[str, str | bytes]) -> None:
    ensure_parent(path)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in files.items():
            zf.writestr(name, data)


def docx(path: Path, title: str, paragraphs: list[str]) -> None:
    body = []
    body.append(
        "<w:p><w:r><w:rPr><w:b/><w:sz w:val=\"32\"/></w:rPr>"
        f"<w:t>{escape(title)}</w:t></w:r></w:p>"
    )
    for para in paragraphs:
        body.append(f"<w:p><w:r><w:t>{escape(para)}</w:t></w:r></w:p>")
    body.append("<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/></w:sectPr>")
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f"<w:body>{''.join(body)}</w:body></w:document>"
    )
    write_zip(
        path,
        {
            "[Content_Types].xml": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
                "</Types>"
            ),
            "_rels/.rels": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
                "</Relationships>"
            ),
            "word/document.xml": document,
        },
    )


def xlsx(path: Path, sheet_name: str, rows: list[list[str]]) -> None:
    strings: list[str] = []
    index: dict[str, int] = {}

    def sid(value: str) -> int:
        if value not in index:
            index[value] = len(strings)
            strings.append(value)
        return index[value]

    def col_name(n: int) -> str:
        out = ""
        while n:
            n, rem = divmod(n - 1, 26)
            out = chr(65 + rem) + out
        return out

    row_xml = []
    for r_idx, row in enumerate(rows, start=1):
        cells = []
        for c_idx, value in enumerate(row, start=1):
            ref = f"{col_name(c_idx)}{r_idx}"
            cells.append(f'<c r="{ref}" t="s"><v>{sid(str(value))}</v></c>')
        row_xml.append(f'<row r="{r_idx}">{"".join(cells)}</row>')
    shared = "".join(f"<si><t>{escape(s)}</t></si>" for s in strings)
    workbook = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        f'<sheets><sheet name="{escape(sheet_name)}" sheetId="1" r:id="rId1"/></sheets></workbook>'
    )
    worksheet = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        f'<sheetData>{"".join(row_xml)}</sheetData></worksheet>'
    )
    write_zip(
        path,
        {
            "[Content_Types].xml": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
                '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
                '<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>'
                "</Types>"
            ),
            "_rels/.rels": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>'
                "</Relationships>"
            ),
            "xl/_rels/workbook.xml.rels": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>'
                '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>'
                "</Relationships>"
            ),
            "xl/workbook.xml": workbook,
            "xl/worksheets/sheet1.xml": worksheet,
            "xl/sharedStrings.xml": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{len(strings)}" uniqueCount="{len(strings)}">{shared}</sst>'
            ),
        },
    )


def pptx(path: Path, title: str, bullets: list[str]) -> None:
    bullet_xml = "".join(
        f'<a:p><a:r><a:rPr lang="zh-CN" sz="2200"/><a:t>{escape(b)}</a:t></a:r></a:p>'
        for b in bullets
    )
    slide = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" '
        'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">'
        "<p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr><a:xfrm><a:off x="685800" y="457200"/><a:ext cx="7772400" cy="800000"/></a:xfrm></p:spPr>'
        f'<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="zh-CN" sz="3600" b="1"/><a:t>{escape(title)}</a:t></a:r></a:p></p:txBody></p:sp>'
        '<p:sp><p:nvSpPr><p:cNvPr id="3" name="Content"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr><a:xfrm><a:off x="914400" y="1600200"/><a:ext cx="7315200" cy="3500000"/></a:xfrm></p:spPr>'
        f'<p:txBody><a:bodyPr/><a:lstStyle/>{bullet_xml}</p:txBody></p:sp>'
        "</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )
    write_zip(
        path,
        {
            "[Content_Types].xml": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>'
                '<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>'
                "</Types>"
            ),
            "_rels/.rels": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>'
                "</Relationships>"
            ),
            "ppt/_rels/presentation.xml.rels": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>'
                "</Relationships>"
            ),
            "ppt/presentation.xml": (
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
                'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" '
                'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">'
                '<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>'
                '<p:sldSz cx="9144000" cy="5143500" type="screen4x3"/>'
                '<p:notesSz cx="6858000" cy="9144000"/></p:presentation>'
            ),
            "ppt/slides/slide1.xml": slide,
        },
    )


def pdf_text(path: Path, title: str, paragraphs: list[str]) -> None:
    ensure_parent(path)
    pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
    c = canvas.Canvas(str(path), pagesize=A4)
    width, height = A4
    c.setFont("STSong-Light", 16)
    c.drawString(22 * mm, height - 25 * mm, title)
    y = height - 40 * mm
    c.setFont("STSong-Light", 10)
    for para in paragraphs:
        for line in textwrap.wrap(para, width=46):
            c.drawString(22 * mm, y, line)
            y -= 6 * mm
            if y < 25 * mm:
                c.showPage()
                c.setFont("STSong-Light", 10)
                y = height - 25 * mm
        y -= 3 * mm
    c.save()


def find_font() -> str | None:
    candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ]
    for candidate in candidates:
        if os.path.exists(candidate):
            return candidate
    return None


def image_file(path: Path, title: str, lines: list[str], size=(1400, 900)) -> None:
    ensure_parent(path)
    img = Image.new("RGB", size, "white")
    draw = ImageDraw.Draw(img)
    font_path = find_font()
    title_font = ImageFont.truetype(font_path, 46) if font_path else ImageFont.load_default()
    body_font = ImageFont.truetype(font_path, 30) if font_path else ImageFont.load_default()
    draw.rectangle([35, 35, size[0] - 35, size[1] - 35], outline=(40, 40, 40), width=3)
    draw.text((70, 70), title, fill=(20, 20, 20), font=title_font)
    y = 150
    for line in lines:
        draw.text((80, y), line, fill=(35, 35, 35), font=body_font)
        y += 48
    img.save(path)


def scanned_pdf(path: Path, title: str, lines: list[str]) -> None:
    tmp = path.with_suffix(".scan-page.png")
    image_file(tmp, title, lines, size=(1240, 1754))
    ensure_parent(path)
    c = canvas.Canvas(str(path), pagesize=A4)
    c.drawImage(str(tmp), 0, 0, width=A4[0], height=A4[1])
    c.save()
    tmp.unlink(missing_ok=True)


def write_readme() -> None:
    readme = OUT / "README.md"
    ensure_parent(readme)
    readme.write_text(
        """# 真实格式测试材料

本目录补充 `enterprise-scenarios-raw` 的真实文件格式版本，用于验证 PDF、DOCX、PPTX、XLSX、JPG、PNG、扫描式 PDF、EML 等解析链路。

这些文件与上层 `source`/原始文本材料表达同一批虚构业务事实：

- `lawfirm/`：合同、判决书、庭审扫描件、调解材料、损失计算表、证据照片。
- `audit/`：合同、扫描凭证、报价表、审计汇报 PPT、邮件仍沿用上层 `mailbox/*.eml`。
- `offboarding/`：技术交接 DOCX、架构 PPTX、账号清单 XLSX、HR 扫描 PDF、系统截图 PNG。

注意：本目录的 Office 文件是用于索引器测试的最小 OOXML fixture，不追求正式排版；扫描式 PDF 是 image-only PDF，用来触发 OCR 路径。
""",
        encoding="utf-8",
    )


def main() -> None:
    write_readme()

    docx(
        OUT / "lawfirm" / "contract-supply-equipment.docx",
        "设备供货合同节选",
        [
            "北岭机械有限公司与蓝湾贸易有限公司约定，蓝湾贸易应在三月十五日前完成第一批设备交付。",
            "迟延交付时，守约方有权要求继续履行并主张迟延履行违约金。",
            "设备质量争议不影响交货期限条款的独立适用。",
        ],
    )
    pdf_text(
        OUT / "lawfirm" / "blueharbor-judgment-text-layer.pdf",
        "蓝湾贸易合同纠纷案一审判决书",
        [
            "本院认为，蓝湾贸易未能在合同约定期限内完成交货，构成迟延履行。",
            "判决如下：蓝湾贸易继续交付设备，并向北岭机械支付迟延履行违约金。",
        ],
    )
    scanned_pdf(
        OUT / "lawfirm" / "blueharbor-judgment-scanned.pdf",
        "扫描判决书",
        ["蓝湾贸易合同纠纷案", "判决如下：继续履行交货义务", "支付迟延履行违约金", "驳回其他诉讼请求"],
    )
    scanned_pdf(
        OUT / "lawfirm" / "trial-transcript-scanned.pdf",
        "扫描庭审笔录",
        ["争点一：交货时间是否固定", "争点二：规格变更是否顺延", "争点三：迟延损失计算依据"],
    )
    xlsx(
        OUT / "lawfirm" / "delay-loss-calculation.xlsx",
        "迟延损失",
        [
            ["项目", "期间", "依据", "金额说明"],
            ["生产线空置", "2026-03-16 至 2026-03-31", "调试计划延期", "约整数占位"],
            ["仓储费用", "2026-03-16 至 2026-04-15", "临时存储", "约整数占位"],
            ["检测改期", "2026-03-20", "第三方检测改期", "约整数占位"],
        ],
    )
    image_file(
        OUT / "lawfirm" / "evidence-delivery-site.jpg",
        "现场交付证据照片",
        ["设备包装未到齐", "仓库验收区", "用于测试 JPG 图片索引"],
    )
    pptx(
        OUT / "lawfirm" / "settlement-brief.pptx",
        "蓝湾案调解简报",
        ["第一批设备四月底前交付", "违约金可适当让步但不得低于底线", "质量索赔权利不放弃"],
    )

    docx(
        OUT / "audit" / "orion-procurement-contract.docx",
        "猎户座采购项目合同",
        [
            "晨星办公用品有限公司向示例集团提供办公终端设备、显示器和安装调试服务。",
            "合同约定，收款账户变更须提交加盖公章的书面说明。",
            "验收合格后方可支付尾款。",
        ],
    )
    scanned_pdf(
        OUT / "audit" / "invoice-morningstar-scanned.pdf",
        "扫描发票",
        ["晨星办公用品有限公司", "猎户座采购项目办公设备", "发票用于首批设备款申请"],
    )
    scanned_pdf(
        OUT / "audit" / "bank-payment-receipt-scanned.pdf",
        "扫描银行回单",
        ["付款方：示例集团", "收款方：晨星办公用品", "用途：猎户座采购项目首期款"],
    )
    scanned_pdf(
        OUT / "audit" / "acceptance-shortage-scanned.pdf",
        "扫描验收单",
        ["到货数量少于采购申请数量", "建议暂缓尾款支付", "待供应商补齐后重新验收"],
    )
    xlsx(
        OUT / "audit" / "quotation-scoring.xlsx",
        "报价评分",
        [
            ["供应商", "项目", "价格评分", "服务评分", "风险备注"],
            ["晨星办公用品", "Project Orion", "高", "中", "收款账户需复核"],
            ["海棠设备服务", "Project Orion", "中", "高", "交付周期较长"],
            ["远航办公集成", "Project Orion", "中", "中", "无明显异常"],
        ],
    )
    pptx(
        OUT / "audit" / "audit-fieldwork-brief.pptx",
        "行政采购专项审计汇报",
        ["审批补签发生在合同流转之后", "付款账户与合同附件不一致", "验收数量不符但付款流程已启动"],
    )
    image_file(
        OUT / "audit" / "warehouse-acceptance-photo.png",
        "入库验收照片",
        ["显示器支架缺少", "验收备注：数量不符", "用于测试 PNG 图片索引"],
    )

    docx(
        OUT / "offboarding" / "kunpeng-api-authentication.docx",
        "鲲鹏结算系统接口鉴权说明",
        [
            "对外接口采用网关层和业务层双层鉴权。",
            "调用方需要提供 X-App-Id、X-Signature、X-Timestamp。",
            "签名密钥不在本文档保存，需从公司密码保管系统申请。",
        ],
    )
    docx(
        OUT / "offboarding" / "kunpeng-oncall-runbook.docx",
        "鲲鹏结算系统值班手册",
        [
            "值班人员每天检查夜间批处理、重复扣减保护告警和对账文件。",
            "遇到额度重复扣减告警时，先暂停后续批次，确认幂等键状态。",
        ],
    )
    pptx(
        OUT / "offboarding" / "lighthouse-architecture.pptx",
        "Lighthouse 项目架构设计",
        ["数据采集服务同步脱敏指标", "指标计算服务每天凌晨计算 T-1 数据", "报表前端只读取聚合结果"],
    )
    xlsx(
        OUT / "offboarding" / "database-account-permissions.xlsx",
        "账号权限",
        [
            ["账号名", "系统", "用途", "权限范围", "口令说明"],
            ["kunpeng_readonly", "Kunpeng Settlement", "值班查询日志和批处理状态", "read-only", "不保存口令"],
            ["kunpeng_batch_ops", "Kunpeng Settlement", "暂停或恢复批处理任务", "limited-ops", "不保存口令"],
            ["lighthouse_report_reader", "Lighthouse", "查看月度报表聚合结果", "read-only", "不保存口令"],
        ],
    )
    scanned_pdf(
        OUT / "offboarding" / "hr-nda-scanned.pdf",
        "扫描保密协议",
        ["员工：李示例", "离职后继续承担保密义务", "技术继任者不应看到本文件"],
    )
    scanned_pdf(
        OUT / "offboarding" / "exit-confirmation-scanned.pdf",
        "扫描离职确认单",
        ["工牌与设备已交回", "账号停用流程已发起", "交接材料已由接收人确认"],
    )
    image_file(
        OUT / "offboarding" / "kunpeng-dashboard-screenshot.png",
        "鲲鹏值班看板截图",
        ["夜间批处理：完成", "重复扣减保护：无新增告警", "对账文件：已到达"],
    )
    image_file(
        OUT / "offboarding" / "lighthouse-report-screenshot.jpg",
        "Lighthouse 报表截图",
        ["月度报表口径", "有效订单数", "结算成功率"],
    )

    manifest = OUT / "manifest.tsv"
    with manifest.open("w", encoding="utf-8", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["path", "scenario", "format", "purpose"])
        for file in sorted(OUT.rglob("*")):
            if file.is_file() and file.name not in {"manifest.tsv"}:
                rel = file.relative_to(OUT).as_posix()
                scenario = rel.split("/")[0]
                writer.writerow([rel, scenario, file.suffix.lower().lstrip("."), "real-format fixture"])

    print(f"generated real-format materials under {OUT}")
    print(f"generated_at={datetime.now().isoformat(timespec='seconds')}")


if __name__ == "__main__":
    main()
