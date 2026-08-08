//! 标签提取：[`lofty`] 适配（artist / title / album / duration / format / bitrate）。

use std::path::Path;

use lofty::file::FileType;
use lofty::prelude::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use crate::model::MusicEntry;
use crate::IndexError;

/// 从单个音频文件提取 metadata。失败（损坏 / 非音频 / 不支持）返回 [`IndexError::Tag`]。
pub fn extract_metadata(path: &Path, modified_time: i64) -> Result<MusicEntry, IndexError> {
    let tagged = lofty::read_from_path(path).map_err(|e| IndexError::Tag {
        path: path.to_string_lossy().into_owned(),
        detail: e.to_string(),
    })?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let (artist, title, album) = tag.map_or((None, None, None), |t| {
        (
            t.artist().map(|c| c.to_string()),
            t.title().map(|c| c.to_string()),
            t.album().map(|c| c.to_string()),
        )
    });

    let props = tagged.properties();
    let secs = props.duration().as_secs_f64();
    let duration_secs = if secs > 0.0 { Some(secs) } else { None };
    let bitrate = props.audio_bitrate();
    let format = Some(format_name(tagged.file_type()));

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    Ok(MusicEntry {
        path: path.to_string_lossy().into_owned(),
        file_name,
        artist,
        title,
        album,
        duration_secs,
        format,
        bitrate,
        modified_time,
    })
}

/// 把 [`FileType`] 映射为短名。
fn format_name(ft: FileType) -> String {
    match ft {
        FileType::Mpeg => "MP3".to_string(),
        FileType::Flac => "FLAC".to_string(),
        FileType::Mp4 => "MP4".to_string(),
        FileType::Vorbis => "Vorbis".to_string(),
        FileType::Opus => "Opus".to_string(),
        FileType::Wav => "WAV".to_string(),
        FileType::Aiff => "AIFF".to_string(),
        FileType::Ape => "APE".to_string(),
        FileType::WavPack => "WavPack".to_string(),
        FileType::Speex => "Speex".to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    /// 写一个最小合法的 PCM WAV（8kHz / 单声道 / 16bit / 0.5s 静音）。
    fn write_silent_wav(path: &Path) {
        const SAMPLE_RATE: u32 = 8000;
        const CHANNELS: u16 = 1;
        const BITS: u16 = 16;
        let num_samples: u32 = SAMPLE_RATE / 2; // 0.5s
        let data_len: u32 = num_samples * u32::from(CHANNELS) * u32::from(BITS / 8);
        let byte_rate: u32 = SAMPLE_RATE * u32::from(CHANNELS) * u32::from(BITS / 8);
        let block_align: u16 = CHANNELS * (BITS / 8);

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&CHANNELS.to_le_bytes());
        buf.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&BITS.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend(std::iter::repeat_n(0u8, data_len as usize)); // 静音

        std::fs::write(path, &buf).unwrap();
    }

    #[test]
    fn extract_wav_round_trip() {
        use lofty::config::WriteOptions;
        use lofty::tag::{Tag, TagType};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.wav");
        write_silent_wav(&path);

        // 用 lofty 写入 artist/title/album（RIFF INFO 为 WAV 原生 tag），再读回。
        let mut tagged = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist("周华健".to_string());
        tag.set_title("朋友".to_string());
        tag.set_album("试音".to_string());
        tagged.insert_tag(tag);
        tagged.save_to_path(&path, WriteOptions::default()).unwrap();

        let entry = extract_metadata(&path, 1234).unwrap();
        assert_eq!(entry.artist.as_deref(), Some("周华健"));
        assert_eq!(entry.title.as_deref(), Some("朋友"));
        assert_eq!(entry.album.as_deref(), Some("试音"));
        assert_eq!(entry.format.as_deref(), Some("WAV"));
        assert_eq!(entry.file_name, "song.wav");
        assert_eq!(entry.modified_time, 1234);
        assert!(entry.duration_secs.unwrap_or(0.0) > 0.0, "时长应 > 0");
    }

    #[test]
    fn extract_nonexistent_path_errors() {
        let err = extract_metadata(&PathBuf::from("/no/such/file.mp3"), 0);
        assert!(matches!(err, Err(IndexError::Tag { .. })));
    }

    #[test]
    fn extract_non_audio_content_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.mp3");
        std::fs::write(&path, b"this is plain text not audio").unwrap();
        let err = extract_metadata(&path, 0);
        assert!(matches!(err, Err(IndexError::Tag { .. })));
    }
}
