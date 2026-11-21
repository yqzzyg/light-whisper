use crate::utils::AppError;

// ---------- WAV 编码 ----------

pub fn encode_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, AppError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|e| AppError::Audio(format!("WAV writer creation failed: {}", e)))?;
        for &s in samples {
            writer
                .write_sample(s)
                .map_err(|e| AppError::Audio(format!("WAV sample write failed: {}", e)))?;
        }
        writer
            .finalize()
            .map_err(|e| AppError::Audio(format!("WAV finalize failed: {}", e)))?;
    }
    Ok(cursor.into_inner())
}

// ---------- WAV 时长 ----------

/// 从 RIFF/WAVE 头部推算音频时长（秒）。
///
/// 只解析头部，不读取采样数据，所以对大文件也是常数开销。头部不完整或不是
/// WAV 时返回 `None`，调用方应回退到默认超时。
pub fn wav_duration_secs(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }

    let mut offset = 12usize;
    let mut byte_rate = 0u32;
    let mut data_bytes: Option<usize> = None;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;

        if id == b"fmt " {
            if body + 16 > bytes.len() {
                return None;
            }
            let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?) as u32;
            let sample_rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            let block_align =
                u16::from_le_bytes(bytes[body + 12..body + 14].try_into().ok()?) as u32;
            byte_rate = if block_align > 0 {
                sample_rate.saturating_mul(block_align)
            } else {
                let bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().ok()?) as u32;
                sample_rate
                    .saturating_mul(channels)
                    .saturating_mul(bits / 8)
            };
        } else if id == b"data" {
            // 异常中断的录音可能留下占位长度，按真实剩余字节裁剪。
            let available = bytes.len() - body;
            data_bytes = Some(size.min(available));
            // 拿到 fmt 后立刻可以算时长；长度越界说明后面不会再有可信的 chunk，
            // 直接收尾，免得用越界 size 推进 offset 反而丢掉已知的 data 长度。
            if byte_rate > 0 || size > available {
                break;
            }
        }

        offset = body.checked_add(size)?.checked_add(size & 1)?;
    }

    let data_bytes = data_bytes?;
    if byte_rate == 0 {
        return None;
    }
    Some(data_bytes as f64 / byte_rate as f64)
}

// ---------- WAV 解码 ----------

/// 从内存 WAV 取出 16bit 单声道样本。
///
/// 只接受已经是 16k 单声道的 WAV——上传路径的 `import_audio_to_wav` 保证这一点。
/// 有了它，流式转写可以直接从字节取 PCM，不必为了拿样本先写盘再解码一遍；
/// 这里也不做重采样，免得在转写热路径上再养一套转换逻辑。
pub fn wav_pcm16_samples(bytes: &[u8]) -> Result<Vec<i16>, AppError> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))
        .map_err(|e| AppError::Audio(format!("WAV 解析失败: {e}")))?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.bits_per_sample != 16 {
        return Err(AppError::Audio(format!(
            "WAV 需为 16bit 单声道，当前为 {}bit {} 声道",
            spec.bits_per_sample, spec.channels
        )));
    }
    reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::Audio(format!("WAV 采样读取失败: {e}")))
}

// ---------- 单元测试 ----------
//
// TDD red-state tests for `encode_wav`.
//
// Current signature is `fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8>`
// with three internal `.expect(...)` calls that panic on error. The contract we
// want to lock in is a fallible signature — `Result<Vec<u8>, AppError>` — so that
// hound write failures bubble up instead of panicking.
//
// These tests use `?` against the return value, which forces the function to
// return a `Result<_, AppError>`. That means **they are expected to fail to
// compile** until `encode_wav` is migrated to the fallible signature — exactly
// the red state we want.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::error::AppError;

    #[test]
    fn test_encode_wav_returns_ok_for_normal_samples() -> Result<(), AppError> {
        let bytes = encode_wav(&[0i16, 1, -1, 32767, -32768], 16000)?;
        assert!(!bytes.is_empty(), "encoded WAV bytes must be non-empty");
        assert!(
            bytes.len() >= 4,
            "encoded WAV must include at least the RIFF header"
        );
        assert_eq!(&bytes[..4], b"RIFF", "WAV file must start with 'RIFF'");
        Ok(())
    }

    #[test]
    fn test_encode_wav_handles_empty_samples() -> Result<(), AppError> {
        // Empty sample slice must not panic — it should still produce a well-formed
        // (header-only) WAV buffer.
        let _bytes = encode_wav(&[], 16000)?;
        Ok(())
    }

    #[test]
    fn test_wav_duration_reads_header_only() -> Result<(), AppError> {
        let samples = vec![0i16; 48_000]; // 3s of 16k mono
        let bytes = encode_wav(&samples, 16000)?;
        let duration = wav_duration_secs(&bytes).expect("a well-formed wav must yield a duration");
        assert!(
            (duration - 3.0).abs() < 0.001,
            "unexpected duration: {duration}"
        );
        Ok(())
    }

    #[test]
    fn test_wav_duration_rejects_non_wav_and_truncated_headers() {
        assert_eq!(wav_duration_secs(b""), None);
        assert_eq!(wav_duration_secs(b"RIFF"), None);
        assert_eq!(wav_duration_secs(&[0u8; 64]), None);

        let bytes = encode_wav(&[0i16; 1600], 16000).unwrap();
        assert_eq!(
            wav_duration_secs(&bytes[..30]),
            None,
            "a truncated header must not be guessed at"
        );
    }

    #[test]
    fn test_wav_duration_clips_a_placeholder_data_length() {
        // 录制中断的 WAV 可能留下未回填的 data 长度，按真实字节裁剪而不是放弃解析。
        let mut bytes = encode_wav(&vec![0i16; 32_000], 16000).unwrap(); // 2s
        let data_at = bytes
            .windows(4)
            .position(|window| window == b"data")
            .expect("encoded wav must contain a data chunk");
        bytes[data_at + 4..data_at + 8].copy_from_slice(&u32::MAX.to_le_bytes());

        let duration =
            wav_duration_secs(&bytes).expect("a placeholder length must still be usable");
        assert!(
            (duration - 2.0).abs() < 0.001,
            "unexpected duration: {duration}"
        );
    }

    #[test]
    fn test_wav_pcm16_samples_round_trips_encoded_wav() {
        // 流式转写靠它从内存 WAV 直接取样本，必须与 encode_wav 严格互逆。
        let samples = [0i16, 1, -1, 32_767, -32_768, 12_345];
        let bytes = encode_wav(&samples, 16_000).unwrap();
        assert_eq!(wav_pcm16_samples(&bytes).unwrap(), samples.to_vec());
    }

    #[test]
    fn test_wav_pcm16_samples_rejects_multichannel() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..8 {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        // 立体声不做静默混音：上游必须已经转成单声道，否则出错更该暴露。
        assert!(wav_pcm16_samples(&cursor.into_inner()).is_err());
    }
}
