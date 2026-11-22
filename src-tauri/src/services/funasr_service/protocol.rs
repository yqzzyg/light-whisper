//! FunASR stdin/stdout protocol types and response parsing.

use crate::utils::AppError;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// 发送给 Python 服务器的命令
///
/// Python 端期望的 JSON 格式是扁平的：
/// - `{"action": "status"}`
/// - `{"action": "transcribe", "audio_path": "/path/to/file.wav"}`
/// - `{"action": "exit"}`
///
/// 使用 `#[serde(tag = "action")]` 生成带 `action` 字段的扁平 JSON，
/// `rename_all = "snake_case"` 将变体名转为小写下划线格式。
#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ServerCommand {
    StreamStart {
        session_id: u64,
        context: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hot_words: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    StreamFeed {
        session_id: u64,
        offset: usize,
        audio_base64: String,
        audio_format: String,
        sample_rate: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        preview: Option<bool>,
    },
    StreamFinish {
        session_id: u64,
    },
    StreamCancel {
        session_id: u64,
    },
    /// 转写音频文件
    Transcribe {
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<crate::state::user_profile::R2T2Config>,
        /// 音频文件的路径
        #[serde(skip_serializing_if = "Option::is_none")]
        audio_path: Option<String>,
        /// 内存音频负载（Base64）
        #[serde(skip_serializing_if = "Option::is_none")]
        audio_base64: Option<String>,
        /// 内存音频编码格式
        #[serde(skip_serializing_if = "Option::is_none")]
        audio_format: Option<String>,
        /// 内存音频采样率
        #[serde(skip_serializing_if = "Option::is_none")]
        sample_rate: Option<u32>,
        /// 热词列表（可选）
        #[serde(skip_serializing_if = "Option::is_none")]
        hot_words: Option<Vec<String>>,
    },
    /// 查询服务器状态
    Status,
    /// 更新 GPU 空闲卸载秒数。0 表示关闭，不重启进程。
    SetGpuIdle {
        seconds: u64,
    },
    SetAudioProcessing {
        enabled: bool,
    },
    /// 退出服务器
    Exit,
}

/// Python 服务器的 JSON 响应
///
/// 这个结构体对应 Python 服务器返回的 JSON 格式。
/// `Option<T>` 表示字段可能存在也可能不存在。
#[derive(Clone, Debug, Deserialize)]
pub(super) struct ServerResponse {
    pub(super) tentative_text: Option<String>,
    pub(super) session_id: Option<u64>,
    pub(super) sample_count: Option<usize>,
    #[serde(rename = "final")]
    pub(super) is_final: Option<bool>,
    /// 请求 ID；新协议用于丢弃取消/超时后迟到的旧响应
    pub(super) request_id: Option<u64>,
    /// 操作是否成功
    pub(super) success: Option<bool>,
    /// 状态标识
    pub(super) status: Option<String>,
    /// 转写得到的文本
    pub(super) text: Option<String>,
    /// 音频时长
    pub(super) duration: Option<f64>,
    /// 错误信息
    pub(super) error: Option<String>,
    /// 检测到的语言
    pub(super) language: Option<String>,
    /// 附加消息
    pub(super) message: Option<String>,
    /// 模型是否已加载
    pub(super) model_loaded: Option<bool>,
    /// 模型是否已初始化（Python status 返回）
    pub(super) initialized: Option<bool>,
    /// 模型加载状态
    pub(super) models: Option<ServerModelStatus>,
    /// 设备信息
    pub(super) device: Option<String>,
    /// GPU 名称
    pub(super) gpu_name: Option<String>,
    /// GPU 总显存（GB）
    pub(super) gpu_memory_total: Option<f64>,
    /// 当前引擎
    pub(super) engine: Option<String>,
    /// 服务端实际采用的输入模式（memory/path）
    pub(super) input_mode: Option<String>,
}

/// Python status 返回的模型状态
#[derive(Debug, Deserialize, Clone)]
pub(super) struct ServerModelStatus {
    pub(super) asr: Option<bool>,
    pub(super) vad: Option<bool>,
    pub(super) punc: Option<bool>,
}

impl ServerResponse {
    pub(super) fn is_model_loaded(&self) -> bool {
        self.model_loaded.unwrap_or_else(|| {
            self.models
                .as_ref()
                .map(|m| {
                    m.asr.unwrap_or(false) && m.vad.unwrap_or(false) && m.punc.unwrap_or(false)
                })
                .unwrap_or(false)
        })
    }
}

/// 读取一行 JSON，超时窗口在每次收到 stdout 数据时重新开始计时。
///
/// 本地引擎推理期间会发 `{"type":"progress", ...}` 保活消息；这里的 timeout
/// 是无进展窗口：持续有输出就继续等，只有卡死才在窗口内报错。
pub(super) async fn read_json_response<T, R>(
    reader: &mut R,
    idle_timeout: Duration,
    context: &str,
) -> Result<T, AppError>
where
    T: for<'de> Deserialize<'de>,
    R: AsyncBufRead + Unpin,
{
    read_json_response_matching(reader, idle_timeout, context, |_| true).await
}

pub(super) async fn read_json_response_matching<T, R>(
    reader: &mut R,
    idle_timeout: Duration,
    context: &str,
    mut accept: impl FnMut(&T) -> bool,
) -> Result<T, AppError>
where
    T: for<'de> Deserialize<'de>,
    R: AsyncBufRead + Unpin,
{
    let mut line_bytes = Vec::new();
    let mut last_activity = Instant::now();

    loop {
        let remaining = idle_timeout.saturating_sub(last_activity.elapsed());
        if remaining.is_zero() {
            return Err(AppError::Asr(format!("{}超时", context)));
        }

        line_bytes.clear();
        let read_result =
            tokio::time::timeout(remaining, reader.read_until(b'\n', &mut line_bytes)).await;

        match read_result {
            Ok(Ok(0)) => {
                return Err(AppError::Asr(format!("{}失败：stdout 已关闭", context)));
            }
            Ok(Ok(_)) => {
                // 任何一行 stdout 都算「有进展」，包括保活消息。
                last_activity = Instant::now();

                let line = match std::str::from_utf8(&line_bytes) {
                    Ok(line) => std::borrow::Cow::Borrowed(line),
                    Err(err) => {
                        log::warn!(
                            "{}阶段收到非 UTF-8 输出，已按损坏文本容错处理: {}",
                            context,
                            err
                        );
                        String::from_utf8_lossy(&line_bytes)
                    }
                };

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // 先在 JSON 值层面识别保活消息：它带 request_id，若直接反序列化成
                // ServerResponse 会被误当成当前请求的最终响应。
                let value = parse_json_line(trimmed);
                let Some(value) = value else {
                    log::warn!(
                        "{}阶段收到非JSON输出 ({}字符)",
                        context,
                        trimmed.chars().count()
                    );
                    continue;
                };
                if value.get("type").and_then(serde_json::Value::as_str) == Some("progress") {
                    log::debug!("{}阶段收到引擎进度消息，重置无进展计时", context);
                    continue;
                }

                match serde_json::from_value::<T>(value) {
                    Ok(response) => {
                        if accept(&response) {
                            return Ok(response);
                        }
                        log::warn!("{}阶段丢弃了不匹配的旧 JSON 响应", context);
                    }
                    Err(err) => {
                        log::warn!("{}阶段收到无法解析的 JSON 响应: {}", context, err);
                    }
                }
            }
            Ok(Err(e)) => {
                return Err(AppError::Asr(format!("{}失败：{}", context, e)));
            }
            Err(_) => {
                return Err(AppError::Asr(format!("{}超时", context)));
            }
        }
    }
}

/// 解析一行 JSON；兼容第三方库把噪音与 JSON 挤在同一行的情况（取首个 `{` 到
/// 末尾 `}`）。返回 None 表示这行不是可用的 JSON。
fn parse_json_line(trimmed: &str) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(value);
    }
    let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) else {
        return None;
    };
    if start >= end {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]).ok()
}
