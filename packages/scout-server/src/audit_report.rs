//! BETA-43 验收 ③：`audit.jsonl` → 人读合规报告（Markdown / CSV）。
//!
//! 客户合规人员不必自行 parse jsonl：`GET /admin/audit/report`（admin token）
//! 按 subject / collection / 时间范围过滤后直接产出可归档的报告文本。
//! 渲染是纯函数（entries 进、String 出），不触文件系统——读文件由
//! [`crate::audit::AuditSink::read_all`] 负责。

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

/// 报告格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    /// Markdown（含统计摘要 + 明细表）。
    Markdown,
    /// CSV（RFC 4180 引号转义，供表格工具二次处理）。
    Csv,
}

impl ReportFormat {
    /// 从查询参数解析；未知值 → `None`（调用方回 400）。缺省 Markdown。
    #[must_use]
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s {
            None | Some("md" | "markdown") => Some(Self::Markdown),
            Some("csv") => Some(Self::Csv),
            Some(_) => None,
        }
    }
}

/// 过滤条件（全部可选，AND 语义）。
#[derive(Debug, Default)]
pub struct ReportFilter {
    /// 精确匹配 `subject`。
    pub subject: Option<String>,
    /// 记录的 `collections` 数组含此 id。
    pub collection: Option<String>,
    /// 时间下界（含）。
    pub from: Option<DateTime<Utc>>,
    /// 时间上界（含）。
    pub to: Option<DateTime<Utc>>,
}

/// 解析时间参数：RFC 3339 全量，或 `YYYY-MM-DD` 短格式（`end_of_day=false` →
/// 当日 00:00:00 UTC；true → 23:59:59 UTC，用于 `to` 的"含当日"直觉）。
/// 解析失败 → `None`（调用方回 400）。
#[must_use]
pub fn parse_time_param(s: &str, end_of_day: bool) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let time = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Some(time.and_utc())
}

/// 记录时间戳；解析失败 → `None`。
fn entry_ts(e: &Value) -> Option<DateTime<Utc>> {
    e["ts"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// 应用过滤条件。设了时间范围时，`ts` 不可解析的记录**排除**（合规报告
/// 宁缺毋滥；此类记录只能来自文件外部篡改）。
#[must_use]
pub fn filter_entries(entries: Vec<Value>, filter: &ReportFilter) -> Vec<Value> {
    entries
        .into_iter()
        .filter(|e| {
            if let Some(subject) = &filter.subject {
                if e["subject"].as_str() != Some(subject.as_str()) {
                    return false;
                }
            }
            if let Some(cid) = &filter.collection {
                let in_collections = e["collections"]
                    .as_array()
                    .is_some_and(|arr| arr.iter().any(|c| c.as_str() == Some(cid.as_str())));
                if !in_collections {
                    return false;
                }
            }
            if filter.from.is_some() || filter.to.is_some() {
                let Some(ts) = entry_ts(e) else {
                    return false;
                };
                if filter.from.is_some_and(|from| ts < from) {
                    return false;
                }
                if filter.to.is_some_and(|to| ts > to) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// 明细列（md 与 csv 共用同一取数逻辑）。
fn entry_columns(e: &Value) -> [String; 8] {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let collections = e["collections"].as_array().map_or_else(String::new, |arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    });
    // query 明文或降级长度（`[audit] log_query=false` 时只有 query_len）。
    let query = if e["query"].is_string() {
        s(&e["query"])
    } else if let Some(n) = e["query_len"].as_u64() {
        format!("<len={n}>")
    } else {
        String::new()
    };
    let results = e["results"]
        .as_u64()
        .map_or_else(String::new, |n| n.to_string());
    [
        s(&e["ts"]),
        s(&e["subject"]),
        s(&e["action"]),
        collections,
        query,
        results,
        format!("{}{}", s(&e["path"]), {
            let m = s(&e["read_mode"]);
            if m.is_empty() {
                String::new()
            } else {
                format!(" ({m})")
            }
        }),
        s(&e["denied_reason"]),
    ]
}

/// 渲染 Markdown 报告：过滤条件 + 按 action / subject 统计 + 明细表。
#[must_use]
pub fn render_markdown(entries: &[Value], filter: &ReportFilter) -> String {
    use std::collections::BTreeMap;
    use std::fmt::Write;

    let mut out = String::new();
    out.push_str("# Scout 检索审计报告\n\n");
    let fmt_opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "（全部）".to_string());
    let fmt_time =
        |o: &Option<DateTime<Utc>>| o.map_or_else(|| "（不限）".to_string(), |t| t.to_rfc3339());
    let _ = writeln!(out, "- 过滤：subject = {}", fmt_opt(&filter.subject));
    let _ = writeln!(out, "- 过滤：collection = {}", fmt_opt(&filter.collection));
    let _ = writeln!(
        out,
        "- 过滤：时间范围 = {} ～ {}",
        fmt_time(&filter.from),
        fmt_time(&filter.to)
    );
    let _ = writeln!(out, "- 记录数：{}\n", entries.len());

    let mut by_action: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_subject: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries {
        *by_action
            .entry(e["action"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
        *by_subject
            .entry(e["subject"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
    }
    out.push_str("## 统计\n\n| 动作 | 条数 |\n|---|---|\n");
    for (k, v) in &by_action {
        let _ = writeln!(out, "| {} | {} |", md_escape(k), v);
    }
    out.push_str("\n| 主体 | 条数 |\n|---|---|\n");
    for (k, v) in &by_subject {
        let _ = writeln!(out, "| {} | {} |", md_escape(k), v);
    }

    out.push_str(
        "\n## 明细\n\n| 时间 | 主体 | 动作 | 集合 | 查询 | 命中数 | 读取路径 | 拒绝原因 |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for e in entries {
        let cols = entry_columns(e);
        let _ = writeln!(
            out,
            "| {} |",
            cols.iter()
                .map(|c| md_escape(c))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    out
}

/// 渲染 CSV（表头 + 明细，RFC 4180）。
#[must_use]
pub fn render_csv(entries: &[Value]) -> String {
    let mut out =
        String::from("ts,subject,action,collections,query,results,path,denied_reason\r\n");
    for e in entries {
        let cols = entry_columns(e);
        let line = cols
            .iter()
            .map(|c| csv_escape(c))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out
}

/// Markdown 表格单元转义：管道与换行会破坏表结构。
fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// RFC 4180 字段转义：含逗号/引号/换行时包引号、内部引号翻倍。
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    fn sample_entries() -> Vec<Value> {
        vec![
            json!({"ts": "2026-07-01T08:00:00+00:00", "subject": "zhang.san", "action": "search",
                   "collections": ["case-a"], "query": "采购合同", "results": 3}),
            json!({"ts": "2026-07-02T09:30:00+00:00", "subject": "li.si", "action": "denied",
                   "collections": ["case-b"], "denied_reason": "collection 'case-b'"}),
            json!({"ts": "2026-07-03T10:00:00+00:00", "subject": "zhang.san", "action": "read",
                   "collections": ["case-a"], "path": "C:/archive/合同,附件.pdf", "read_mode": "snippets"}),
        ]
    }

    #[test]
    fn parse_time_param_rfc3339_and_date_shorthand() {
        assert_eq!(
            parse_time_param("2026-07-01T08:00:00+08:00", false)
                .unwrap()
                .to_rfc3339(),
            "2026-07-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_time_param("2026-07-01", false).unwrap().to_rfc3339(),
            "2026-07-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_time_param("2026-07-01", true).unwrap().to_rfc3339(),
            "2026-07-01T23:59:59+00:00"
        );
        assert!(parse_time_param("yesterday", false).is_none());
    }

    #[test]
    fn filter_by_subject_collection_and_time() {
        let entries = sample_entries();
        let by_subject = filter_entries(
            entries.clone(),
            &ReportFilter {
                subject: Some("zhang.san".into()),
                ..Default::default()
            },
        );
        assert_eq!(by_subject.len(), 2);

        let by_collection = filter_entries(
            entries.clone(),
            &ReportFilter {
                collection: Some("case-b".into()),
                ..Default::default()
            },
        );
        assert_eq!(by_collection.len(), 1);
        assert_eq!(by_collection[0]["action"], "denied");

        let by_time = filter_entries(
            entries,
            &ReportFilter {
                from: parse_time_param("2026-07-02", false),
                to: parse_time_param("2026-07-02", true),
                ..Default::default()
            },
        );
        assert_eq!(by_time.len(), 1);
        assert_eq!(by_time[0]["subject"], "li.si");
    }

    #[test]
    fn time_filter_excludes_unparseable_ts() {
        let entries = vec![json!({"ts": "not-a-time", "subject": "s", "action": "search"})];
        let out = filter_entries(
            entries.clone(),
            &ReportFilter {
                from: parse_time_param("2026-01-01", false),
                ..Default::default()
            },
        );
        assert!(out.is_empty(), "设时间范围时坏 ts 应被排除");
        // 未设时间范围 → 保留。
        assert_eq!(filter_entries(entries, &ReportFilter::default()).len(), 1);
    }

    #[test]
    fn markdown_report_contains_stats_and_rows() {
        let md = render_markdown(&sample_entries(), &ReportFilter::default());
        assert!(md.contains("# Scout 检索审计报告"));
        assert!(md.contains("| search | 1 |"));
        assert!(md.contains("| zhang.san | 2 |"));
        assert!(md.contains("采购合同"));
        assert!(md.contains("(snippets)"), "读取模式应进明细：{md}");
        assert!(md.contains("collection 'case-b'"));
    }

    #[test]
    fn query_len_degraded_entries_render_len_marker() {
        let entries = vec![json!({"ts": "2026-07-01T08:00:00+00:00", "subject": "s",
                                  "action": "search", "collections": [], "query_len": 5})];
        let md = render_markdown(&entries, &ReportFilter::default());
        assert!(md.contains("<len=5>"), "{md}");
    }

    #[test]
    fn csv_escapes_commas_and_quotes() {
        let csv = render_csv(&sample_entries());
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            "ts,subject,action,collections,query,results,path,denied_reason"
        );
        assert!(
            csv.contains("\"C:/archive/合同,附件.pdf (snippets)\""),
            "含逗号字段应被引号包裹：{csv}"
        );
        assert!(csv.contains("collection 'case-b'"));
    }

    #[test]
    fn report_format_parse() {
        assert_eq!(ReportFormat::parse(None), Some(ReportFormat::Markdown));
        assert_eq!(
            ReportFormat::parse(Some("md")),
            Some(ReportFormat::Markdown)
        );
        assert_eq!(ReportFormat::parse(Some("csv")), Some(ReportFormat::Csv));
        assert_eq!(ReportFormat::parse(Some("pdf")), None);
    }
}
