# 真实格式测试材料

本目录补充 `enterprise-scenarios-raw` 的真实文件格式版本，用于验证 PDF、DOCX、PPTX、XLSX、JPG、PNG、扫描式 PDF、EML 等解析链路。

这些文件与上层 `source`/原始文本材料表达同一批虚构业务事实：

- `lawfirm/`：合同、判决书、庭审扫描件、调解材料、损失计算表、证据照片。
- `audit/`：合同、扫描凭证、报价表、审计汇报 PPT、邮件仍沿用上层 `mailbox/*.eml`。
- `offboarding/`：技术交接 DOCX、架构 PPTX、账号清单 XLSX、HR 扫描 PDF、系统截图 PNG。

注意：本目录的 Office 文件是用于索引器测试的最小 OOXML fixture，不追求正式排版；扫描式 PDF 是 image-only PDF，用来触发 OCR 路径。
