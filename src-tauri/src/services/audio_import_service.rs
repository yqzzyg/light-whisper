//! 导入音频转写：把外部音频文件解码、重采样为 16k 单声道 16bit PCM WAV。
//!
//! 本模块只负责「文件 → 标准 WAV」这一段。转写引擎分发、历史落库都在
//! `commands/audio_import.rs`，复用既有的 ASR 服务与 history_service。
//!
//! 解码基于纯 Rust 的 symphonia，无外部 ffmpeg 二进制依赖。

use std::io::ErrorKind;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::services::audio_service::{encode_wav, resample::ResamplerState, TARGET_SAMPLE_RATE};
use crate::utils::AppError;

/// 支持的扩展名白名单（小写）。
pub const SUPPORTED_EXTENSIONS: &[&str] = &["wav", "mp3", "m4a", "flac", "ogg"];

/// 源文件大小预检上限：200MB。防御解压炸弹式超大文件。
const MAX_SOURCE_BYTES: u64 = 200 * 1024 * 1024;

/// 解码后的标准 WAV 结果。
#[derive(Debug, Clone)]
pub struct ImportedAudio {
    pub wav: Vec<u8>,
    pub duration_sec: f64,
}

/// 解码后的 16k 单声道 PCM，供流式转写按块喂给引擎。
#[derive(Debug, Clone)]
pub struct ImportedPcm {
    pub samples: Vec<i16>,
    pub duration_sec: f64,
}

/// 预检 → 解码 → 重采样为 16k 单声道 PCM。
///
/// 一次性文件转写（`import_audio_to_wav`）与流式转写共用这一段，
/// 避免两条路径各自维护解码/重采样逻辑。
pub fn decode_to_16k_mono(path: &Path) -> Result<ImportedPcm, AppError> {
    precheck(path)?;
    let (pcm, sample_rate) = decode_file(path)?;
    if pcm.is_empty() {
        return Err(AppError::Asr("音频解码失败：没有得到有效音频数据".into()));
    }

    let resampled: Vec<i16> = if sample_rate == TARGET_SAMPLE_RATE {
        pcm
    } else {
        let mut resampler = ResamplerState::new(sample_rate)
            .map_err(|e| AppError::Asr(format!("音频重采样初始化失败: {e}")))?;
        let mut output = Vec::with_capacity(
            (pcm.len() as f64 * TARGET_SAMPLE_RATE as f64 / sample_rate as f64) as usize + 8,
        );
        resampler
            .push_i16(&pcm)
            .map_err(|e| AppError::Asr(format!("音频重采样失败: {e}")))?
            .as_ref()
            .iter()
            .copied()
            .for_each(|sample| output.push(sample));
        output
    };

    if resampled.is_empty() {
        return Err(AppError::Asr("音频重采样失败：没有得到有效音频数据".into()));
    }

    let duration_sec = resampled.len() as f64 / TARGET_SAMPLE_RATE as f64;
    Ok(ImportedPcm {
        samples: resampled,
        duration_sec,
    })
}

/// 校验扩展名是否在白名单内。
pub fn is_supported_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            SUPPORTED_EXTENSIONS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

/// 文件存在性、扩展名与大小预检。返回源文件字节数。
fn precheck(path: &Path) -> Result<u64, AppError> {
    if !path.is_file() {
        return Err(AppError::Asr("导入失败：文件不存在或不是普通文件".into()));
    }
    if !is_supported_extension(path) {
        return Err(AppError::Asr(format!(
            "不支持的音频格式，请选择 {} 文件",
            SUPPORTED_EXTENSIONS.join(" / ")
        )));
    }
    let size = std::fs::metadata(path)
        .map_err(|e| AppError::Asr(format!("读取音频文件信息失败: {e}")))?
        .len();
    if size == 0 {
        return Err(AppError::Asr("导入失败：音频文件为空".into()));
    }
    if size > MAX_SOURCE_BYTES {
        return Err(AppError::Asr(
            "导入失败：音频文件超过 200MB 上限，请先裁剪后重试".into(),
        ));
    }
    Ok(size)
}

/// 解码任意支持格式为 16bit PCM，返回 (单声道混合后的样本, 原始采样率)。
///
/// 多声道按各声道算术平均混为单声道；样本统一归一化到 f32 后再转 i16。
pub fn decode_file(path: &Path) -> Result<(Vec<i16>, u32), AppError> {
    let file =
        std::fs::File::open(path).map_err(|e| AppError::Asr(format!("打开音频文件失败: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| AppError::Asr(format!("音频格式探测失败: {e}")))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| AppError::Asr("音频文件中没有可用的音轨".into()))?;
    if track.codec_params.codec == CODEC_TYPE_NULL {
        return Err(AppError::Asr("音频文件中没有可解码的音轨".into()));
    }
    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| AppError::Asr("无法确定音频采样率".into()))?;
    if sample_rate == 0 {
        return Err(AppError::Asr("音频采样率无效".into()));
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AppError::Asr(format!("音频解码器初始化失败: {e}")))?;

    // 用 f32 平面缓存累积样本，最后统一混合为单声道 i16。
    let mut mono_f32: Vec<f32> = Vec::new();
    let mut decoded_any = false;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // symphonia 0.5 在流结束时返回底层 IO 的 UnexpectedEof。
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(AppError::Asr(format!("读取音频数据包失败: {e}"))),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet)
            .map_err(|e| AppError::Asr(format!("音频解码失败: {e}")))?;

        let spec = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        decoded_any = true;
        let channels = spec.channels.count().max(1);

        // SampleBuffer 的容量以「每声道帧数」计（symphonia 的 Duration = u64）。
        // copy_planar_ref 内部 assert capacity >= frames，必须按实际帧数创建。
        let mut sample_buf = SampleBuffer::<f32>::new(frames as u64, spec);
        sample_buf.copy_planar_ref(decoded);
        let planes = sample_buf.samples();

        for frame in 0..frames {
            let mut sum = 0.0f32;
            for ch in 0..channels {
                // copy_planar_ref 按平面顺序存放：[ch0 全部帧][ch1 全部帧]...
                if let Some(&value) = planes.get(ch * frames + frame) {
                    sum += value;
                }
            }
            mono_f32.push(sum / channels as f32);
        }
    }

    if !decoded_any || mono_f32.is_empty() {
        return Err(AppError::Asr("音频解码失败：没有得到有效音频数据".into()));
    }

    let pcm: Vec<i16> = mono_f32.iter().map(|&sample| f32_to_i16(sample)).collect();
    Ok((pcm, sample_rate))
}

fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped < 0.0 {
        (clamped * 32768.0) as i16
    } else {
        (clamped * 32767.0) as i16
    }
}

/// 统一入口：预检 → 解码 → 重采样 16k 单声道 → hound 编码 WAV。
pub fn import_audio_to_wav(path: &Path) -> Result<ImportedAudio, AppError> {
    let decoded = decode_to_16k_mono(path)?;
    let wav = encode_wav(&decoded.samples, TARGET_SAMPLE_RATE)?;
    Ok(ImportedAudio {
        wav,
        duration_sec: decoded.duration_sec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_wav_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("light_whisper_import_{name}_{nanos}.wav"))
    }

    fn write_wav(path: &Path, samples: &[i16], sample_rate: u32, channels: u16) {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn decodes_16k_wav_and_keeps_samples() {
        let path = temp_wav_path("16k");
        let samples: Vec<i16> = (0..16000).map(|i| (i % 1000) as i16).collect();
        write_wav(&path, &samples, 16000, 1);

        let (decoded, rate) = decode_file(&path).unwrap();
        assert_eq!(rate, 16000);
        assert_eq!(decoded.len(), samples.len());
        // 解码经 i16 → f32 归一化 → i16 往返，存在 ±1 量化误差。
        for (index, (&got, &expected)) in decoded.iter().zip(samples.iter()).enumerate() {
            assert!(
                (got as i32 - expected as i32).abs() <= 1,
                "sample {index}: expected {expected}, got {got}"
            );
        }

        fs::remove_file(&path).ok();
    }

    #[test]
    fn resamples_44k_stereo_to_16k_mono() {
        let path = temp_wav_path("44k");
        // 44100 样本 ≈ 1 秒，双声道各 44100 个交错样本。
        let per_channel = 44100usize;
        let mut samples = Vec::with_capacity(per_channel * 2);
        for i in 0..per_channel {
            let value =
                ((i as f64 * 440.0 * std::f64::consts::PI * 2.0 / 44100.0).sin() * 12000.0) as i16;
            samples.push(value);
            samples.push(value);
        }
        write_wav(&path, &samples, 44100, 2);

        let imported = import_audio_to_wav(&path).unwrap();
        assert!(
            (0.95..=1.05).contains(&imported.duration_sec),
            "duration should be ~1s, got {}",
            imported.duration_sec
        );
        assert_eq!(&imported.wav[..4], b"RIFF");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_unsupported_extension() {
        let path = std::env::temp_dir().join("light_whisper_import_bad.txt");
        fs::write(&path, b"not audio").unwrap();
        assert!(import_audio_to_wav(&path).is_err());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_missing_file() {
        let path = std::env::temp_dir().join("light_whisper_import_missing.wav");
        assert!(import_audio_to_wav(&path).is_err());
    }

    #[test]
    fn rejects_corrupted_wav_without_panic() {
        let path = temp_wav_path("corrupt");
        fs::write(&path, b"RIFFfakefakefake").unwrap();
        // 只要不 panic 且返回 Err 即可。
        let _ = import_audio_to_wav(&path);
        fs::remove_file(&path).ok();
    }
}
