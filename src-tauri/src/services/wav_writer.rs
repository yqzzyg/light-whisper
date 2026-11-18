//! 边录边写的 WAV 文件。
//!
//! 会议录音可能长达数小时，不能像听写那样攒在内存里再一次性编码，所以这里先写
//! 一个长度未知的 44 字节占位头，之后只追加 PCM 数据，结束（或崩溃后下次启动）
//! 再把真实长度回填进去。
//!
//! 占位值用 0：读者会算出 0 秒，而不是 4GB 的荒谬时长；`repair_header` 以
//! 「声明长度 ≠ 实际长度」为依据判定需要修复，0 和未回填的旧值都能命中。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const WAV_HEADER_LEN: u64 = 44;
/// 16bit 单声道：每秒字节数 = 采样率 × 2。
const BYTES_PER_SAMPLE: u64 = 2;

/// 追加式 WAV 写入器：创建时落占位头，`append` 只追加 PCM，`finish` 回填长度。
pub struct WavAppender {
    file: File,
    sample_rate: u32,
    data_bytes: u64,
}

impl WavAppender {
    pub fn create(path: &Path, sample_rate: u32) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("录音采样率不能为 0".to_string());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建笔记录音目录失败: {error}"))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|error| format!("创建录音文件失败: {error}"))?;
        write_header(&mut file, sample_rate, 0)?;
        file.flush()
            .map_err(|error| format!("写入录音头失败: {error}"))?;
        Ok(Self {
            file,
            sample_rate,
            data_bytes: 0,
        })
    }

    /// 追加一段 16bit 单声道 PCM。
    pub fn append(&mut self, samples: &[i16]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let mut buffer = Vec::with_capacity(samples.len() * BYTES_PER_SAMPLE as usize);
        for sample in samples {
            buffer.extend_from_slice(&sample.to_le_bytes());
        }
        self.file
            .write_all(&buffer)
            .map_err(|error| format!("写入录音数据失败（磁盘可能已满）: {error}"))?;
        self.data_bytes += buffer.len() as u64;
        Ok(())
    }

    /// 已写入音频的时长（按实际落盘字节算，不是墙钟时间）。
    pub fn duration_secs(&self) -> f64 {
        self.data_bytes as f64 / (self.sample_rate as f64 * BYTES_PER_SAMPLE as f64)
    }

    /// 回填长度并落盘，返回音频时长。调用后文件即可被播放器直接打开。
    pub fn finish(mut self) -> Result<f64, String> {
        let duration = self.duration_secs();
        self.file
            .flush()
            .map_err(|error| format!("刷新录音文件失败: {error}"))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("定位录音文件头失败: {error}"))?;
        write_header(&mut self.file, self.sample_rate, self.data_bytes)?;
        self.file
            .flush()
            .map_err(|error| format!("回填录音头失败: {error}"))?;
        // 停止录音后用户可能立刻重启应用，必须确保头部（而不只是数据）落盘。
        self.file
            .sync_all()
            .map_err(|error| format!("同步录音文件失败: {error}"))?;
        Ok(duration)
    }
}

fn write_header(file: &mut File, sample_rate: u32, data_bytes: u64) -> Result<(), String> {
    let data_bytes = u32::try_from(data_bytes).unwrap_or(u32::MAX);
    let byte_rate = sample_rate.saturating_mul(BYTES_PER_SAMPLE as u32);
    let riff_size = 36u32.saturating_add(data_bytes);

    let mut header = Vec::with_capacity(WAV_HEADER_LEN as usize);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes()); // block align
    header.extend_from_slice(&16u16.to_le_bytes()); // 位深
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());

    file.write_all(&header)
        .map_err(|error| format!("写入录音头失败: {error}"))
}

/// 修复被异常中断的录音：把占位/过期的长度改成实际字节数。
///
/// 返回音频时长；文件不存在、不是本模块写出的 WAV 或短于一个头时返回 `None`。
pub fn repair_header(path: &Path) -> Result<Option<f64>, String> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("打开录音文件失败: {error}")),
    };
    let total_len = file
        .metadata()
        .map_err(|error| format!("读取录音文件信息失败: {error}"))?
        .len();
    if total_len < WAV_HEADER_LEN {
        return Ok(None);
    }

    let mut header = [0u8; WAV_HEADER_LEN as usize];
    file.read_exact(&mut header)
        .map_err(|error| format!("读取录音文件头失败: {error}"))?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" || &header[12..16] != b"fmt " {
        return Ok(None);
    }
    if &header[36..40] != b"data" {
        return Ok(None);
    }
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits = u16::from_le_bytes([header[34], header[35]]);
    if channels != 1 || bits != 16 || sample_rate == 0 {
        return Ok(None);
    }

    let declared = u32::from_le_bytes([header[40], header[41], header[42], header[43]]);
    let actual = total_len - WAV_HEADER_LEN;
    if u64::from(declared) != actual {
        let actual_u32 = u32::try_from(actual).unwrap_or(u32::MAX);
        file.seek(SeekFrom::Start(4))
            .map_err(|error| format!("定位录音头长度字段失败: {error}"))?;
        file.write_all(&36u32.saturating_add(actual_u32).to_le_bytes())
            .map_err(|error| format!("回填 RIFF 长度失败: {error}"))?;
        file.seek(SeekFrom::Start(40))
            .map_err(|error| format!("定位 data 长度字段失败: {error}"))?;
        file.write_all(&actual_u32.to_le_bytes())
            .map_err(|error| format!("回填 data 长度失败: {error}"))?;
        file.flush()
            .map_err(|error| format!("刷新修复后的录音头失败: {error}"))?;
    }

    Ok(Some(
        actual as f64 / (sample_rate as f64 * BYTES_PER_SAMPLE as f64),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempWav {
        path: PathBuf,
    }

    impl TempWav {
        fn new(name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "minutes-wav-{}-{nonce}-{name}.wav",
                std::process::id()
            ));
            Self { path }
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn appended_samples_are_readable_after_finish() {
        let temp = TempWav::new("finish");
        let mut writer = WavAppender::create(&temp.path, 16_000).unwrap();
        writer.append(&vec![7i16; 16_000]).unwrap();
        writer.append(&vec![-7i16; 8_000]).unwrap();
        assert!((writer.duration_secs() - 1.5).abs() < 1e-9);
        let duration = writer.finish().unwrap();
        assert!((duration - 1.5).abs() < 1e-9);

        let bytes = std::fs::read(&temp.path).unwrap();
        assert_eq!(bytes.len(), WAV_HEADER_LEN as usize + 24_000 * 2);
        assert_eq!(&bytes[0..4], b"RIFF");
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(riff_size as usize, bytes.len() - 8);
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data_size as usize, 24_000 * 2);
    }

    #[test]
    fn placeholder_header_reports_zero_duration_until_finished() {
        let temp = TempWav::new("placeholder");
        let mut writer = WavAppender::create(&temp.path, 16_000).unwrap();
        writer.append(&vec![0i16; 16_000]).unwrap();
        // 不 finish：头部仍是 0 长度，模拟录制中进程被杀
        drop(writer);

        let bytes = std::fs::read(&temp.path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0);
    }

    #[test]
    fn repair_header_backfills_an_interrupted_recording() {
        let temp = TempWav::new("repair");
        let mut writer = WavAppender::create(&temp.path, 16_000).unwrap();
        writer.append(&vec![1i16; 32_000]).unwrap();
        drop(writer); // 头部未回填

        let duration = repair_header(&temp.path).unwrap().unwrap();
        assert!((duration - 2.0).abs() < 1e-9);

        let bytes = std::fs::read(&temp.path).unwrap();
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data_size, 64_000);
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(riff_size, 64_036);

        // 再修一次不应改变结果（幂等）
        assert!((repair_header(&temp.path).unwrap().unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn repair_header_ignores_missing_and_foreign_files() {
        let temp = TempWav::new("missing");
        assert!(repair_header(&temp.path).unwrap().is_none());

        std::fs::write(&temp.path, b"not a wav file at all, definitely long enough").unwrap();
        assert!(repair_header(&temp.path).unwrap().is_none());

        std::fs::write(&temp.path, b"RIFF").unwrap();
        assert!(repair_header(&temp.path).unwrap().is_none());
    }

    #[test]
    fn create_rejects_zero_sample_rate() {
        let temp = TempWav::new("zero-rate");
        assert!(WavAppender::create(&temp.path, 0).is_err());
    }
}
