//! ASR 分派：所有语音转文字都从这里进来。
//!
//! 只有一个入口 [`transcribe`]，按「输入形态 + 时长」选路：
//!
//! | 场景 | 策略 |
//! |---|---|
//! | 在线引擎（GLM / DashScope） | 整段字节，超限先拦 |
//! | 本地引擎 + ≥2 小时文件 | 逐 30 分钟切片 |
//! | 本地引擎 + 其余 | 整段，路径直传或内存字节 |
//!
//! 上传与笔记录音都先把音频落盘、再以 [`AudioInput::File`] 进来，因此长音频都能
//! 享受切片；只有历史重识别这类手上只有字节的调用点走 [`AudioInput::WavBytes`]。
//!
//! 调用方只负责落库：上传写 `mode=import` 并保留音频，笔记写 `mode=note` 且不持有
//! 音频文件。归档语义不属于转写，所以不放进这一层。
//!
//! 逐字预览由实时听写单独负责；上传页与笔记页只显示转写状态。

use std::path::Path;

use crate::state::AppState;
use crate::utils::{paths, AppError};

use super::audio_chunk_service::{self, WavChunker, CHUNK_SECS};
use super::funasr_service::{self, TranscriptionResult};
use super::transcription_events::{self, Preview};
use super::{alibaba_asr_service, audio_service, glm_asr_service};

// 引擎限制常量，与各 ASR 服务保持一致。
const GLM_MAX_DURATION_SECS: f64 = 30.0;
const GLM_MAX_BYTES: usize = 25 * 1024 * 1024;
const DASHSCOPE_MAX_BASE64_BYTES: usize = 10 * 1024 * 1024;

/// 音频来源。
pub enum AudioInput<'a> {
    /// 内存里的 16k 单声道 16bit WAV 字节（上传解码结果、历史重识别）。
    WavBytes(&'a [u8]),
    /// 磁盘上的音频文件。上传支持 mp3/m4a 等格式，由解码层统一转 16k 单声道。
    File(&'a Path),
}

/// 一次转写的不变参数。
pub struct TranscribeOptions<'a> {
    /// 已知时长。`WavBytes` 留空时从 WAV 头补，在线引擎上限与切片阈值都依赖它。
    pub duration_sec: Option<f64>,
    /// 错误文案前缀，例如「导入转写失败」。
    pub error_prefix: &'a str,
    pub preview: Preview,
}

impl<'a> TranscribeOptions<'a> {
    /// 默认：自动切片、不推送预览。
    pub fn new(error_prefix: &'a str) -> Self {
        Self {
            duration_sec: None,
            error_prefix,
            preview: Preview::None,
        }
    }

    pub fn with_duration(mut self, duration_sec: Option<f64>) -> Self {
        self.duration_sec = duration_sec;
        self
    }

    pub fn with_preview(mut self, preview: Preview) -> Self {
        self.preview = preview;
        self
    }
}

/// 一次转写要走的路，以及这条路需要的入参。
///
/// 两步分开（先算计划、再拿锁执行）是为了让 `match` 时不会出现「路选对了但路径
/// 没拿到」这种半成品状态。
enum Plan<'a> {
    /// 整段交给引擎；走路径还是内存字节由入参的变体决定。
    Whole,
    /// 逐 30 分钟切片，需要能读盘的文件路径（切片要按静音找落刀点）。
    Chunked(&'a Path),
}

/// 当前引擎是否跑在本地（qwen3 / r2t2）。
///
/// 只有本地引擎支持路径直传、长音频分块与原生流式；在线引擎必须拿到完整字节。
pub fn is_local_engine(engine: &str) -> bool {
    !matches!(engine, "alibaba-asr" | "glm-asr")
}

/// 在线引擎的时长 / 大小上限。
///
/// 本地引擎直接放行（底层已内置长音频自动分块）。文案在导入与笔记两个入口保持一致。
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

/// 转写一段音频。
///
/// 本地引擎的所有策略共用同一个 Python 进程，整轮执行期间必须独占：锁只在这里
/// 拿一次，下面调用的 `*_locked` 变体不再重入（`tokio::sync::Mutex` 不可重入）。
pub async fn transcribe(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    input: AudioInput<'_>,
    options: TranscribeOptions<'_>,
) -> Result<TranscriptionResult, AppError> {
    let engine = paths::read_engine_config();
    let duration_sec = options.duration_sec.or_else(|| match &input {
        AudioInput::WavBytes(bytes) => audio_service::wav_duration_secs(bytes),
        AudioInput::File(_) => None,
    });
    let options = TranscribeOptions {
        duration_sec,
        ..options
    };
    let error_prefix = options.error_prefix;

    if !is_local_engine(&engine) {
        ensure_online_limit(&input, duration_sec).await?;
        return dispatch_online(&engine, state, input, error_prefix).await;
    }

    let plan = plan_transcription(&input, &options);

    let _asr_guard = state.engine.native_asr_owner.lock().await;
    match plan {
        Plan::Whole => transcribe_whole(app_handle, state, &input, error_prefix).await,
        Plan::Chunked(path) => {
            transcribe_chunked(
                app_handle,
                state,
                path,
                duration_sec,
                options.preview,
                error_prefix,
            )
            .await
        }
    }
}

/// 按 WAV 字节转写。给历史重识别等只有字节的调用点用。
pub async fn transcribe_wav_bytes(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    audio: Vec<u8>,
    error_prefix: &str,
) -> Result<TranscriptionResult, String> {
    transcribe(
        app_handle,
        state,
        AudioInput::WavBytes(&audio),
        TranscribeOptions::new(error_prefix),
    )
    .await
    .map_err(|error| error.message())
}

/// 选路是纯函数，便于单测锁住「什么时候切片」。
///
/// 切片依赖 `WavChunker` 在文件里找静音落刀，所以只对路径输入生效；内存字节没有
/// 可切分的位置信息，再长也只能整段交出去。
fn plan_transcription<'a>(input: &'a AudioInput<'a>, options: &TranscribeOptions<'_>) -> Plan<'a> {
    if audio_chunk_service::should_chunk(options.duration_sec) {
        if let AudioInput::File(path) = input {
            return Plan::Chunked(path);
        }
    }
    Plan::Whole
}

/// 整段转写：文件走路径直传（不读进内存、不落临时副本），字节直接交给引擎。
///
/// 引擎失败时返回的是 `Ok(success: false)`（而非 `Err`），所以这里必须再校验一次：
/// 否则调用方会把「引擎报错、文本为空」当成转写成功记进历史，上传侧的失败回滚也不会触发。
async fn transcribe_whole(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    input: &AudioInput<'_>,
    error_prefix: &str,
) -> Result<TranscriptionResult, AppError> {
    let result = match input {
        AudioInput::File(path) => {
            funasr_service::transcribe_path_locked(state, path, app_handle).await
        }
        // 引擎只收字节：临时文件的读写由 `transcribe_wav_bytes_via_path` 负责。
        AudioInput::WavBytes(bytes) => {
            funasr_service::transcribe_locked(state, bytes.to_vec(), app_handle).await
        }
    };
    result
        .map_err(|error| prefixed(error, error_prefix))
        .and_then(|result| require_success(result, error_prefix))
}

/// 逐 30 分钟切片转写，最后拼成一段文本。
///
/// 中间任何一块失败都整体失败：半截转录比没有更糟，用户会以为录丢了。
async fn transcribe_chunked(
    app_handle: &tauri::AppHandle,
    state: &AppState,
    path: &Path,
    duration_sec: Option<f64>,
    preview: Preview,
    error_prefix: &str,
) -> Result<TranscriptionResult, AppError> {
    let mut chunker = WavChunker::open(path, CHUNK_SECS)
        .map_err(|error| AppError::Asr(format!("{error_prefix}：{error}")))?;
    let sample_rate = chunker.sample_rate();
    // 只用于进度文案；静音吸附会让实际块数略有出入，别拿它当循环上界。
    let estimated = audio_chunk_service::chunk_count(duration_sec.unwrap_or(0.0));

    let mut parts: Vec<String> = Vec::new();
    let mut language: Option<String> = None;
    let mut index = 0usize;

    while let Some(samples) = chunker
        .next_chunk()
        .map_err(|error| AppError::Asr(format!("{error_prefix}：{error}")))?
    {
        index += 1;
        transcription_events::emit_chunk_progress(app_handle, preview, index, estimated);

        let result =
            funasr_service::transcribe_pcm16_locked(state, &samples, sample_rate, app_handle)
                .await
                .map_err(|error| {
                    AppError::Asr(format!(
                        "{error_prefix}：第 {index} 块转写失败：{}",
                        error.message()
                    ))
                })?;
        if !result.success {
            let reason = result.error.unwrap_or_else(|| "未知错误".into());
            return Err(AppError::Asr(format!(
                "{error_prefix}：第 {index} 块转写失败：{reason}"
            )));
        }
        if language.is_none() {
            language = result.language.filter(|value| !value.trim().is_empty());
        }
        parts.push(result.text);
    }

    if parts.is_empty() {
        return Err(AppError::Asr(format!(
            "{error_prefix}：录音文件里没有可用的音频数据"
        )));
    }

    Ok(TranscriptionResult {
        text: audio_chunk_service::join_transcripts(&parts),
        duration: duration_sec,
        success: true,
        error: None,
        language,
    })
}

/// 在线引擎只收完整字节，且限流判据是「文件有多大」——先看元数据再决定读不读，
/// 免得为了一句超限提示把上百兆读进内存。
async fn ensure_online_limit(
    input: &AudioInput<'_>,
    duration_sec: Option<f64>,
) -> Result<(), AppError> {
    let wav_len = match input {
        AudioInput::WavBytes(bytes) => bytes.len(),
        AudioInput::File(path) => tokio::fs::metadata(path)
            .await
            .map_err(|error| AppError::Asr(format!("读取音频文件失败: {error}")))?
            .len()
            .min(usize::MAX as u64) as usize,
    };
    check_engine_limit(wav_len, duration_sec.unwrap_or(0.0))
}

/// 在线引擎：读成字节后按厂商分派。它们没有本地进程，不参与 `native_asr_owner`。
async fn dispatch_online(
    engine: &str,
    state: &AppState,
    input: AudioInput<'_>,
    error_prefix: &str,
) -> Result<TranscriptionResult, AppError> {
    let audio = match input {
        AudioInput::WavBytes(bytes) => bytes.to_vec(),
        AudioInput::File(path) => tokio::fs::read(path)
            .await
            .map_err(|error| AppError::Asr(format!("{error_prefix}：读取录音文件失败: {error}")))?,
    };
    let result = if engine == "alibaba-asr" {
        alibaba_asr_service::transcribe(state, audio).await
    } else {
        glm_asr_service::transcribe(state, audio).await
    };
    result
        .map_err(|error| prefixed(error, error_prefix))
        .and_then(|result| require_success(result, error_prefix))
}

fn require_success(
    result: TranscriptionResult,
    error_prefix: &str,
) -> Result<TranscriptionResult, AppError> {
    if result.success {
        Ok(result)
    } else {
        Err(AppError::Asr(
            result.error.unwrap_or_else(|| error_prefix.to_string()),
        ))
    }
}

/// 给错误加一句场景前缀。内层前缀由 `AppError::message` 剥掉，避免
/// 「导入转写失败：ASR错误: ...」这种嵌套。
fn prefixed(error: AppError, prefix: &str) -> AppError {
    AppError::Asr(format!("{prefix}：{}", error.message()))
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

    #[test]
    fn long_files_are_chunked() {
        let input = AudioInput::File(Path::new("meeting.wav"));
        let options = TranscribeOptions::new("转写失败")
            .with_duration(Some(audio_chunk_service::CHUNK_THRESHOLD_SECS));
        assert!(matches!(
            plan_transcription(&input, &options),
            Plan::Chunked(_)
        ));
    }

    #[test]
    fn short_files_take_the_whole_path() {
        let options = TranscribeOptions::new("转写失败").with_duration(Some(300.0));
        assert!(matches!(
            plan_transcription(&AudioInput::File(Path::new("clip.wav")), &options),
            Plan::Whole
        ));
    }

    #[test]
    fn bytes_never_chunk_however_long() {
        // 内存字节没有可切分的位置信息：宁可整段，也不假装切了。
        let input = AudioInput::WavBytes(&[0u8; 64]);
        let options = TranscribeOptions::new("转写失败")
            .with_duration(Some(audio_chunk_service::CHUNK_THRESHOLD_SECS * 2.0));
        assert!(matches!(plan_transcription(&input, &options), Plan::Whole));
    }
}
