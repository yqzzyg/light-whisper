//! 超长会议录音的切片支持。
//!
//! 背景：会议录音可能长达数小时。整段交给本地引擎时，Python 端要把整个文件
//! 读进内存（1 小时约 115MB PCM，转 f32 后翻倍），而且一次 IPC 调用全程没有
//! 进度可言。本模块把一段录音按时长切成若干块，交给调用方逐块转写。
//!
//! **只切「一段录音的内部」**：不产生新的 `note_recordings`，不多写历史，
//! 用户看到的仍是一段录音、一段转录。这与「用户手动停一下」形成的物理分段
//! 是两件不同的事（后者每停一次就是一条段记录）。
//!
//! 切点不追求绝对精确：常在 30:00 处硬切会把一个词劈成两半，所以在目标位置
//! 前后各找一个「最安静」的 100ms 窗口落刀。找不到真正静音（连续讲话）时
//! 退回目标位置硬切，宁可断一个词也不让单块无限变长。

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::utils::AppError;

/// 超过这个时长才切片。短录音整段转更省事，也免得白搭几次 IPC 开销。
pub const CHUNK_THRESHOLD_SECS: f64 = 2.0 * 60.0 * 60.0;
/// 每块的目标时长。30 分钟 PCM 约 57.6MB，转 f32 后约 115MB，是内存与进度的折中。
pub const CHUNK_SECS: f64 = 30.0 * 60.0;

/// 切点前后各找多久的静音。
const SILENCE_SEARCH_SECS: f64 = 3.0;
/// 比较能量用的窗口长度。
const SILENCE_WINDOW_SECS: f64 = 0.1;

/// 这段音频是否值得切片。
///
/// 时长未知（`None`）一律不切：宁可沿用整段转写，也不要按猜测动手。
pub fn should_chunk(duration_sec: Option<f64>) -> bool {
    duration_sec.is_some_and(|secs| secs.is_finite() && secs >= CHUNK_THRESHOLD_SECS)
}

/// 估算块数，只用于进度展示；真实块数由 [`WavChunker`] 的实际切分决定。
pub fn chunk_count(duration_sec: f64) -> usize {
    if !duration_sec.is_finite() || duration_sec <= 0.0 {
        return 1;
    }
    ((duration_sec / CHUNK_SECS).ceil() as usize).max(1)
}

/// 在 `target` 附近挑一个振幅最小的窗口，返回落刀位置（样本下标）。
///
/// `samples` 必须已经包含 `target + radius` 之后的数据，否则会在结尾处提前
/// 收窄搜索范围。返回值保证落在 `1..=samples.len()`，避免切出空块。
fn snap_to_silence(samples: &[i16], target: usize, radius: usize, window: usize) -> usize {
    if samples.is_empty() {
        return 0;
    }
    let window = window.max(1);
    let target = target.min(samples.len());
    let lo = target.saturating_sub(radius);
    // 最后一个候选窗口必须完整，否则能量不可比。
    let hi = (target + radius).min(samples.len().saturating_sub(window));

    if hi <= lo {
        return target.max(1).min(samples.len());
    }

    let mut best = target;
    let mut best_energy = i64::MAX;
    let mut start = lo;
    while start <= hi {
        let end = (start + window).min(samples.len());
        let center = start + (end - start) / 2;
        // 用绝对值和当能量：i16 平方累加在长窗口上会溢出，峰值和已经够分辨静音。
        let energy: i64 = samples[start..end]
            .iter()
            .map(|sample| i64::from(*sample).abs())
            .sum();
        // 能量打平（例如全程等幅）时选离目标最近的候选，避免切点被拖到搜索区边界。
        let closer = (center as i64 - target as i64).abs() < (best as i64 - target as i64).abs();
        if energy < best_energy || (energy == best_energy && closer) {
            best_energy = energy;
            best = center;
        }
        start += window;
    }
    best.max(1).min(samples.len())
}

/// 流式读取一段 WAV，按块产出 PCM。
///
/// 只支持「16bit 单声道 PCM」——这正是本应用会议录音的落盘格式（见
/// `wav_writer`）。遇到其它格式返回错误，调用方应回退整段转写，不要猜。
pub struct WavChunker {
    reader: hound::WavReader<BufReader<File>>,
    sample_rate: u32,
    samples_per_chunk: usize,
    silence_radius: usize,
    silence_window: usize,
    /// 已读入但还没切出去的数据。长度始终不超过 `samples_per_chunk + radius`。
    pending: Vec<i16>,
    exhausted: bool,
}

impl WavChunker {
    /// 打开录音文件；`chunk_secs` 由调用方给（生产固定 30 分钟，测试用短值）。
    pub fn open(path: &Path, chunk_secs: f64) -> Result<Self, AppError> {
        let reader = hound::WavReader::open(path)
            .map_err(|error| AppError::Audio(format!("打开录音文件失败: {error}")))?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
            || spec.sample_rate == 0
        {
            return Err(AppError::Audio(format!(
                "录音格式不支持切片（{} 声道 / {} bit），需要 16bit 单声道 WAV",
                spec.channels, spec.bits_per_sample
            )));
        }

        let sample_rate = spec.sample_rate;
        let samples_per_chunk = (f64::from(sample_rate) * chunk_secs).round().max(1.0) as usize;
        // 搜索半径不超过块长的一半，否则切点会飘到相邻块里去。
        let silence_radius =
            ((f64::from(sample_rate) * SILENCE_SEARCH_SECS) as usize).min(samples_per_chunk / 2);
        let silence_window = ((f64::from(sample_rate) * SILENCE_WINDOW_SECS) as usize).max(1);

        Ok(Self {
            reader,
            sample_rate,
            samples_per_chunk,
            silence_radius,
            silence_window,
            pending: Vec::with_capacity(samples_per_chunk + silence_radius),
            exhausted: false,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 取出下一块；读完返回 `None`。
    pub fn next_chunk(&mut self) -> Result<Option<Vec<i16>>, AppError> {
        // 多读一个搜索半径，才能让切点在目标位置前后挑静音。
        let fill_to = self.samples_per_chunk + self.silence_radius;
        while !self.exhausted && self.pending.len() < fill_to {
            self.read_more(fill_to - self.pending.len())?;
        }

        if self.pending.is_empty() {
            return Ok(None);
        }
        // 已经读到文件结尾：剩下的就是最后一块，不必再找静音。
        if self.exhausted {
            let chunk = std::mem::take(&mut self.pending);
            return Ok(Some(chunk));
        }

        let cut = snap_to_silence(
            &self.pending,
            self.samples_per_chunk,
            self.silence_radius,
            self.silence_window,
        );
        let rest = self.pending.split_off(cut);
        Ok(Some(std::mem::replace(&mut self.pending, rest)))
    }

    fn read_more(&mut self, want: usize) -> Result<(), AppError> {
        let mut read = 0;
        let mut samples = self.reader.samples::<i16>();
        while read < want {
            match samples.next() {
                Some(Ok(value)) => {
                    self.pending.push(value);
                    read += 1;
                }
                Some(Err(error)) => {
                    return Err(AppError::Audio(format!("读取录音数据失败: {error}")));
                }
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }
        Ok(())
    }
}

/// 把各块转写结果拼回一段文本。
///
/// 与 `notes_service` 把多段录音拼成笔记转录的规则一致（空行分隔、丢空块），
/// 这样「自动切片」与「手动分段」在只读渲染下看起来是同一个东西。
pub fn join_transcripts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::audio_service::encode_wav;
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
                "minutes-chunk-{}-{nonce}-{name}.wav",
                std::process::id()
            ));
            Self { path }
        }

        fn write(&self, samples: &[i16], sample_rate: u32) {
            let bytes = encode_wav(samples, sample_rate).unwrap();
            std::fs::write(&self.path, bytes).unwrap();
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn short_and_unknown_audio_are_not_chunked() {
        assert!(!should_chunk(None));
        assert!(!should_chunk(Some(0.0)));
        assert!(!should_chunk(Some(60.0 * 60.0)));
        assert!(!should_chunk(Some(f64::NAN)));
        assert!(!should_chunk(Some(f64::INFINITY)));
        // 差一秒到两小时也不切。
        assert!(!should_chunk(Some(CHUNK_THRESHOLD_SECS - 1.0)));
        // 正好两小时开始切。
        assert!(should_chunk(Some(CHUNK_THRESHOLD_SECS)));
        assert!(should_chunk(Some(3.0 * 60.0 * 60.0)));
    }

    #[test]
    fn chunk_count_rounds_up_and_never_returns_zero() {
        assert_eq!(chunk_count(0.0), 1);
        assert_eq!(chunk_count(-5.0), 1);
        assert_eq!(chunk_count(30.0), 1);
        assert_eq!(chunk_count(CHUNK_SECS), 1);
        assert_eq!(chunk_count(CHUNK_SECS + 1.0), 2);
        assert_eq!(chunk_count(2.0 * 60.0 * 60.0), 4);
        assert_eq!(chunk_count(3.0 * 60.0 * 60.0), 6);
    }

    #[test]
    fn silence_snapping_moves_the_cut_into_the_quiet_gap() {
        // 1 秒满幅噪声，中间 400..420 是静音；目标切点设在 415。
        let mut samples = vec![10_000i16; 1000];
        for sample in samples.iter_mut().take(420).skip(400) {
            *sample = 0;
        }
        let cut = snap_to_silence(&samples, 415, 50, 10);
        assert!(
            (400..=420).contains(&cut),
            "cut should land inside the quiet gap, got {cut}"
        );
    }

    #[test]
    fn silence_snapping_falls_back_to_target_when_everything_is_loud() {
        // 全程等幅，没有可挑的静音：不强行偏移，留在目标附近切。
        let samples = vec![9_000i16; 1000];
        let cut = snap_to_silence(&samples, 500, 100, 10);
        assert!(
            (490..=510).contains(&cut),
            "cut should stay near the target, got {cut}"
        );
    }

    #[test]
    fn silence_snapping_never_returns_an_empty_or_out_of_range_cut() {
        assert_eq!(snap_to_silence(&[], 10, 5, 2), 0);
        assert_eq!(snap_to_silence(&[0i16; 1], 10, 5, 2), 1);
        let cut = snap_to_silence(&[0i16; 4], 0, 100, 8);
        assert!((1..=4).contains(&cut), "cut out of range: {cut}");
    }

    #[test]
    fn chunker_splits_into_contiguous_blocks_without_losing_samples() {
        let temp = TempWav::new("split");
        let sample_rate = 16_000u32;
        let rate = sample_rate as usize;
        let total_samples = 10 * rate; // 10 秒
        let mut samples = vec![1_000i16; total_samples];
        // 每 2 秒插入一段静音，切点应该往这些位置靠。
        for second in (2..10).step_by(2) {
            let at = second * rate;
            for sample in samples.iter_mut().take(at + 400).skip(at - 400) {
                *sample = 0;
            }
        }
        temp.write(&samples, sample_rate);

        let mut chunker = WavChunker::open(&temp.path, 2.0).unwrap();
        assert_eq!(chunker.sample_rate(), sample_rate);

        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk().unwrap() {
            assert!(!chunk.is_empty(), "chunker must not emit empty chunks");
            chunks.push(chunk);
        }

        assert!(
            chunks.len() >= 4,
            "10s at 2s per chunk should yield at least 4 blocks, got {}",
            chunks.len()
        );
        let produced: usize = chunks.iter().map(Vec::len).sum();
        assert_eq!(
            produced, total_samples,
            "chunking must not drop or duplicate samples"
        );
        // 除最后一块外，每块都应在目标附近（±搜索半径）。
        let target = 2 * rate;
        let radius = ((sample_rate as f64 * SILENCE_SEARCH_SECS) as usize).min(target / 2);
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(
                chunk.len() <= target + radius && chunk.len() >= target.saturating_sub(radius),
                "block length {} outside expected range",
                chunk.len()
            );
        }
    }

    #[test]
    fn chunker_rejects_formats_it_cannot_split() {
        let temp = TempWav::new("stereo");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&temp.path, spec).unwrap();
        for _ in 0..100 {
            writer.write_sample(0i16).unwrap();
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        let error = WavChunker::open(&temp.path, 1.0)
            .err()
            .expect("stereo must be rejected");
        assert!(
            error.to_string().contains("不支持切片"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn joined_transcripts_skip_blank_parts_and_use_blank_lines() {
        let parts = vec![
            " 第一段 ".to_string(),
            "   ".to_string(),
            "".to_string(),
            "第二段".to_string(),
        ];
        assert_eq!(join_transcripts(&parts), "第一段\n\n第二段");
        assert_eq!(join_transcripts(&[]), "");
        assert_eq!(join_transcripts(&[" ".to_string()]), "");
    }
}
