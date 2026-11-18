//! ASR 分派：把一段音频交给当前引擎，并把各家错误收成同一种文案。
//!
//! 两个入口：
//!
//! - [`transcribe_wav_bytes`]：音频已经在内存里（听写重识别、导入、在线引擎）。
//! - [`transcribe_file`]：音频是一整段磁盘文件（笔记转写）。本地引擎直接把路径
//!   交给 Python 端读，**不把一小时会议读进内存、也不复制一份临时 WAV**；
//!   在线引擎只能收字节，才退化成读文件。
//!
//! `check_engine_limit` 放在这里而不是命令层：导入与笔记转写都要拦在线引擎的
//! 时长/大小上限，两处各写一份迟早会漂移。

use std::path::Path;

use crate::state::AppState;
use crate::utils::{paths, AppError};

use super::funasr_service::{self, TranscriptionResult};
use super::{alibaba_asr_service, glm_asr_service};

// 引擎限制常量，与各 ASR 服务保持一致。
const GLM_MAX_DURATION_SECS: f64 = 30.0;
const GLM_MAX_BYTES: usize = 25 * 1024 * 1024;
const DASHSCOPE_MAX_BASE64_BYTES: usize = 10 * 1024 * 1024;

/// 当前引擎是否跑在本地（qwen3 / r2t2）。
///
/// 只有本地引擎支持路径直传与长音频分块；在线引擎必须拿到完整字节。
pub fn is_local_engine(engine: &str) -> bool {
    !matches!(engine, "alibaba-asr" | "glm-asr")
}

/// 在线引擎的时长 / 大小上限。
///
/// 本地引擎直接放行（底层已内置长音频自动分块）。文案与导入转写保持一致，
/// 用户在不同入口看到的是同一条规则。
pub fn check_engine_limit(wav_len: usize, duration_sec: f64) -> Result<(), AppError> {
    let engine = paths::read_engine_config();
    match engine.as_str() {
        "glm-asr" => {
            if duration_sec > GLM_MAX_DURATION_SECS {
                return Err(AppError::Asr(format!(
                    "音频约 {:.0} 秒，超过 GLM-ASR 的 {} 秒上限，请裁剪后重试或切换本地引擎",
                    duration_sec, GLM_MAX_DURATION_SECS as u32
                )));
            }
            if wav_len > GLM_MAX_BYTES {
                return Err(AppError::Asr(
                    "音频文件超过 GLM-ASR 的 25MB 上限，请裁剪后重试".into(),
                ));
            }
        }
        "alibaba-asr" => {
            // base64 放大 4/3，按放大后长度比对 10MB 上限。
            if wav_len.saturating_mul(4) / 3 > DASHSCOPE_MAX_BASE64_BYTES {
                return Err(AppError::Asr(
                    "音频文件超过 DashScope 的 10MB 请求上限（约 3.9 分钟），请裁剪后重试或切换本地引擎".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// 按当前引擎配置把 WAV 字节分发给对应 ASR 服务。
///
/// 供历史重识别、导入转写与在线引擎的笔记转写共用，避免多处重复分发逻辑。
/// `error_prefix` 用于区分调用场景的错误文案。
pub async fn transcribe_wav_bytes(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    audio: Vec<u8>,
    error_prefix: &str,
) -> Result<TranscriptionResult, String> {
    let engine = paths::read_engine_config();
    let result = if is_local_engine(&engine) {
        funasr_service::transcribe(state, audio, app_handle).await
    } else if engine == "alibaba-asr" {
        alibaba_asr_service::transcribe(state, audio).await
    } else {
        glm_asr_service::transcribe(state, audio).await
    }
    .map_err(|error| format!("{error_prefix}: {error}"))?;
    if result.success {
        Ok(result)
    } else {
        Err(result.error.unwrap_or_else(|| error_prefix.to_string()))
    }
}

/// 转写磁盘上的 WAV 文件。
///
/// `duration_sec` 只用于两件事：在线引擎的时长上限、以及本地引擎的等待预算
/// （与导入转写同一条 `ceil(dur*0.1+120)` 曲线）。时长未知时退回默认预算，
/// 不会比现有行为更差。
pub async fn transcribe_file(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    path: &Path,
    duration_sec: Option<f64>,
    error_prefix: &str,
) -> Result<TranscriptionResult, String> {
    let engine = paths::read_engine_config();

    if is_local_engine(&engine) {
        // 路径直传：Rust 侧只做一次路径校验，音频不进内存也不落临时文件。
        return funasr_service::transcribe_path(state, path, duration_sec, app_handle)
            .await
            .map_err(|error| format!("{error_prefix}: {error}"))
            .and_then(|result| {
                if result.success {
                    Ok(result)
                } else {
                    Err(result.error.unwrap_or_else(|| error_prefix.to_string()))
                }
            });
    }

    // 在线引擎只能收字节，而且限流判据是「文件有多大」，先看元数据再决定读不读，
    // 免得为了一句超限提示把上百兆读进内存。
    let file_len = tokio::fs::metadata(path)
        .await
        .map_err(|error| format!("{error_prefix}: 读取录音文件失败: {error}"))?
        .len();
    check_engine_limit(file_len.min(usize::MAX as u64) as usize, duration_sec.unwrap_or(0.0))
        .map_err(|error| format!("{error_prefix}: {error}"))?;

    let audio = tokio::fs::read(path)
        .await
        .map_err(|error| format!("{error_prefix}: 读取录音文件失败: {error}"))?;
    transcribe_wav_bytes(app_handle, state, audio, error_prefix).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_engines_are_everything_but_the_two_online_ones() {
        assert!(is_local_engine("qwen3-asr-0.6b"));
        assert!(is_local_engine("confucius4-r2t2"));
        assert!(!is_local_engine("glm-asr"));
        assert!(!is_local_engine("alibaba-asr"));
    }
}
