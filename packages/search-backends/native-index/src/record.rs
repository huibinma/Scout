//! `USN_RECORD_V2` 原始字节解析（纯函数、无 I/O、无 `unsafe`）。
//!
//! 手动按已发布的 `ntifs.h` 字段偏移量解析，不依赖把字节切片强制转型为
//! `windows` crate 的 `USN_RECORD_V2` 结构体（该结构体含变长文件名的"柔性数组
//! 成员"，转型需要 `unsafe` 指针运算且要自行处理对齐；显式按偏移量读取更简单、
//! 更容易审计，且天然是安全代码）。

/// 从一条 `USN_RECORD_V2` 解析出的最小必要字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawUsnRecord {
    /// 文件引用号（NTFS File Reference Number，本条记录对应的文件/目录的唯一 id）。
    pub frn: u64,
    /// 父目录的文件引用号。
    pub parent_frn: u64,
    /// 本条记录对应的 USN（Journal 内的游标位置）。
    pub usn: i64,
    /// 变更原因位掩码（`USN_REASON_*`）。
    pub reason: u32,
    /// 文件属性位掩码（`FILE_ATTRIBUTE_*`）。
    pub attributes: u32,
    /// 文件/目录名（不含路径）。
    pub name: String,
}

/// `USN_RECORD_V2`/`V3` 公共头部长度（到 `FileNameOffset` 字段结束）。
const HEADER_LEN: usize = 60;

fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
    buf.get(offset..offset + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_ne_bytes)
}

fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    buf.get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_ne_bytes)
}

fn read_u64(buf: &[u8], offset: usize) -> Option<u64> {
    buf.get(offset..offset + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_ne_bytes)
}

fn read_i64(buf: &[u8], offset: usize) -> Option<i64> {
    buf.get(offset..offset + 8)
        .and_then(|s| s.try_into().ok())
        .map(i64::from_ne_bytes)
}

/// 解析单条 V2 记录（`record` 已按 `RecordLength` 切好边界）。
fn parse_v2(record: &[u8]) -> Option<RawUsnRecord> {
    let frn = read_u64(record, 8)?;
    let parent_frn = read_u64(record, 16)?;
    let usn = read_i64(record, 24)?;
    let reason = read_u32(record, 40)?;
    let attributes = read_u32(record, 52)?;
    let name_len = usize::from(read_u16(record, 56)?);
    let name_offset = usize::from(read_u16(record, 58)?);

    let name_bytes = record.get(name_offset..name_offset.checked_add(name_len)?)?;
    let utf16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&utf16);

    Some(RawUsnRecord {
        frn,
        parent_frn,
        usn,
        reason,
        attributes,
        name,
    })
}

/// 解析连续记录字节流（[`crate::sys::enum_usn_data`] / [`crate::sys::read_usn_journal`]
/// 返回的原始负载）为 [`RawUsnRecord`] 列表。
///
/// 每条记录按自身 `RecordLength` 步进；`MajorVersion != 2` 的记录（如 `ReFS` 128-bit
/// `FileId` 的 V3，NTFS 卷不会产生）与解析失败/越界的畸形记录一律跳过、不中断——
/// 单条记录异常不应丢失同一批次里其余记录。
pub(crate) fn parse_usn_records(mut buf: &[u8]) -> Vec<RawUsnRecord> {
    let mut out = Vec::new();
    while buf.len() >= HEADER_LEN {
        let Some(record_length) = read_u32(buf, 0).map(|v| v as usize) else {
            break;
        };
        if record_length < HEADER_LEN || record_length > buf.len() {
            break;
        }
        let Some(record) = buf.get(..record_length) else {
            break;
        };

        if read_u16(record, 4) == Some(2) {
            if let Some(parsed) = parse_v2(record) {
                out.push(parsed);
            }
        }

        buf = &buf[record_length..];
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 手工构造一条最小合法 `USN_RECORD_V2` 字节序列（文件名 "a.txt"）。
    fn build_record(
        frn: u64,
        parent_frn: u64,
        name: &str,
        attributes: u32,
        reason: u32,
    ) -> Vec<u8> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let name_bytes_len = name_utf16.len() * 2;
        let record_length = HEADER_LEN + name_bytes_len;
        // 真实 Win32 输出的记录 8 字节对齐，且 `RecordLength` 字段本身就是**含 padding**
        // 的步进长度（微软官方示例代码按 `RecordLength` 步进到下一条记录起始地址）——
        // 若字段只写未 padding 的长度，解析会踩进 padding 字节、错位读到下一条记录。
        let padded_length = record_length.div_ceil(8) * 8;

        let mut buf = vec![0u8; padded_length];
        buf[0..4].copy_from_slice(&u32::try_from(padded_length).unwrap().to_ne_bytes());
        buf[4..6].copy_from_slice(&2u16.to_ne_bytes()); // MajorVersion = 2
        buf[6..8].copy_from_slice(&0u16.to_ne_bytes()); // MinorVersion
        buf[8..16].copy_from_slice(&frn.to_ne_bytes());
        buf[16..24].copy_from_slice(&parent_frn.to_ne_bytes());
        buf[24..32].copy_from_slice(&1i64.to_ne_bytes()); // Usn
        buf[32..40].copy_from_slice(&0i64.to_ne_bytes()); // TimeStamp
        buf[40..44].copy_from_slice(&reason.to_ne_bytes());
        buf[44..48].copy_from_slice(&0u32.to_ne_bytes()); // SourceInfo
        buf[48..52].copy_from_slice(&0u32.to_ne_bytes()); // SecurityId
        buf[52..56].copy_from_slice(&attributes.to_ne_bytes());
        buf[56..58].copy_from_slice(&u16::try_from(name_bytes_len).unwrap().to_ne_bytes());
        buf[58..60].copy_from_slice(&u16::try_from(HEADER_LEN).unwrap().to_ne_bytes());
        for (i, unit) in name_utf16.iter().enumerate() {
            buf[HEADER_LEN + i * 2..HEADER_LEN + i * 2 + 2].copy_from_slice(&unit.to_ne_bytes());
        }
        buf
    }

    #[test]
    fn parses_single_record_roundtrip() {
        let buf = build_record(100, 5, "报告.docx", 0x20, 0x100);
        let records = parse_usn_records(&buf);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frn, 100);
        assert_eq!(records[0].parent_frn, 5);
        assert_eq!(records[0].name, "报告.docx");
        assert_eq!(records[0].attributes, 0x20);
        assert_eq!(records[0].reason, 0x100);
    }

    #[test]
    fn parses_multiple_consecutive_records() {
        let mut buf = build_record(1, 5, "a.txt", 0x20, 0x100);
        buf.extend(build_record(2, 5, "b目录", 0x10, 0x100));
        let records = parse_usn_records(&buf);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "a.txt");
        assert_eq!(records[1].name, "b目录");
        assert_eq!(records[1].attributes, 0x10);
    }

    #[test]
    fn empty_buffer_yields_no_records() {
        assert!(parse_usn_records(&[]).is_empty());
    }

    #[test]
    fn truncated_header_is_skipped_not_panicking() {
        let buf = vec![0xFFu8; 10];
        assert!(parse_usn_records(&buf).is_empty());
    }

    #[test]
    fn oversized_record_length_stops_gracefully() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&(9999u32).to_ne_bytes());
        assert!(parse_usn_records(&buf).is_empty());
    }

    #[test]
    fn non_v2_major_version_is_skipped() {
        let mut buf = build_record(1, 5, "a.txt", 0x20, 0x100);
        buf[4..6].copy_from_slice(&3u16.to_ne_bytes()); // MajorVersion = 3
        assert!(parse_usn_records(&buf).is_empty());
    }
}
