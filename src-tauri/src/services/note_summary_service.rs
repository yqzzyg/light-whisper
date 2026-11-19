//! AI 摘要：把一篇笔记的转录（加手写正文）压成结构化 markdown 纪要。
//!
//! 两步走，只为绕开上下文上限：
//!
//! 1. **分块提炼**：按段落/句子边界把素材切成不超过 `MAX_PART_TOKENS` 的块，
//!    每块只提炼本段要点（不带标题、不做全文总结）。
//! 2. **合并成稿**：把各块要点（加手写正文）交给模型，输出固定章节的纪要。
//!
//! 短素材（只有一块）跳过第 1 步，直接出终稿——一次调用省一次往返，也避免
//! 「先摘要再摘要」的信息损耗。
//!
//! 边界：这里只管「生成文本」，写库由命令层做；失败或取消时不产出任何半成品，
//! 旧摘要保持原样（用户点重新生成失败，不该丢掉上一版）。

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tauri::Emitter;

use crate::services::llm_provider::LlmEndpoint;
use crate::services::{codex_oauth_service, llm_client, llm_provider, notes_service};
use crate::state::AppState;

/// 取消用的哨兵错误：命令层靠它把「用户取消」和「真失败」分开。
pub const CANCELLED: &str = "__note_summary_cancelled__";

/// 单块素材的 token 预算。中文 1 字≈1 token、ASCII 4 字符≈1 token，
/// 留出提示词与输出的余量后取 12k。
///
/// 这是软预算：单段超长时先退到句子边界，仍超则硬切，硬切阈值是 1.25 倍，
/// 所以单块实际峰值约 15k tokens。别按「绝不超 12k」去推下游的上下文限制。
const MAX_PART_TOKENS: usize = 12_288;

/// 进度事件名。命令层发终态事件时复用，避免同一个字符串在两处各写一遍。
pub(crate) const EVENT: &str = "note-summary-status";
/// 单块请求的可见输出停滞上限：长块也可能几十秒没 token，给足余量。
const STREAM_PROGRESS_TIMEOUT_SECS: u64 = 240;
const STREAM_TOTAL_TIMEOUT_SECS: u64 = 1_800;

/// 内置的分段提炼提示词。`{index}` / `{total}` 会被替换成段号与总段数；
/// 用户可在设置里整段替换（`UserProfile::note_summary_part_prompt`）。
pub const DEFAULT_PART_PROMPT: &str =
    "你是会议纪要助手。用户会给出同一场会议转录的第 {index}/{total} 部分。\
只提炼这一部分里真正出现的信息，用不超过 8 条项目符号写：讨论的主题与关键细节、已达成的结论或决定、\
明确的待办（负责人与时间点）、尚未解决的问题。\
规则：不得编造人名、数字、日期；没有对应内容就不写；不要写导语、标题或「本部分」之类的话；\
使用素材本身的语言。";

/// 内置的最终成稿提示词。这是「恢复默认」的唯一事实源：
/// 前端通过 `get_note_summary_prompt_defaults` 读取，不在 i18n 里另抄一份。
pub const DEFAULT_MERGE_PROMPT: &str =
    "你是会议纪要助手。用户会给出同一场会议的分段要点，可能还有手写的笔记正文。\
请合并成一份 markdown 纪要，使用且只使用以下二级标题（没有内容的章节整节省略，顺序不变）：\
## 概要 / ## 讨论 / ## 决定 / ## 待办。\
要求：概要 3-5 句说清会议目的与最重要的结论；讨论按主题归并，不要按时间流水账罗列；\
决定与待办用列表，待办尽量带负责人与时间（素材里有才写）；\
不编造任何素材里没有的人名、数字、日期；标题跟随素材语言，英文素材用 \
Summary / Discussion / Decisions / Action items；直接输出 markdown，不要代码块包裹，不要额外解释。";

/// 自定义提示词的长度上限（按字符计）。够写清格式和语气要求，
/// 又不至于把 `user_profile.json` 撑成几 MB。
pub const MAX_PROMPT_CHARS: usize = 8_000;

/// 生成结果：正文、模型名、素材指纹。
pub struct SummaryOutcome {
    pub summary: String,
    pub model: String,
    pub source_hash: String,
}

/// 按 `token` 口径估算长度：非 ASCII 1 字算 1 token，ASCII 4 字符算 1 token。
///
/// 只有测试需要这个「人类可读」口径；生产路径直接用 `units` 比较预算。
#[cfg(test)]
fn estimate_tokens(text: &str) -> usize {
    units(text).div_ceil(4)
}

/// 内部计数单位：1 单位 = 1/4 token，避免逐字符累加时的浮点。
fn units(text: &str) -> usize {
    text.chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 4 })
        .sum()
}

/// 按段落边界切块；单段超预算时退到句子边界，仍超则硬切。
fn split_parts(text: &str, max_tokens: usize) -> Vec<String> {
    let budget = max_tokens.max(1) * 4;
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_units = 0usize;

    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        for piece in split_oversized(paragraph, budget) {
            let piece_units = units(&piece);
            if current_units > 0 && current_units + piece_units > budget {
                parts.push(std::mem::take(&mut current));
                current_units = 0;
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&piece);
            current_units += piece_units;
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

fn is_sentence_end(ch: char) -> bool {
    matches!(ch, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';' | '\n')
}

/// 把超过预算的单段切小：优先在句末标点断开，没有标点就按预算硬切。
fn split_oversized(paragraph: &str, budget: usize) -> Vec<String> {
    if units(paragraph) <= budget {
        return vec![paragraph.to_string()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_units = 0usize;
    for ch in paragraph.chars() {
        current.push(ch);
        current_units += if ch.is_ascii() { 1 } else { 4 };
        let at_sentence = is_sentence_end(ch) && current_units >= budget / 2;
        if (at_sentence && current_units >= budget) || current_units >= budget * 5 / 4 {
            pieces.push(std::mem::take(&mut current));
            current_units = 0;
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
}

/// 拼素材：标题 + 手写正文 + 转录。空块不占位，避免提示词里出现空章节。
fn build_material(title: &str, content: &str, transcript: &str) -> String {
    let mut parts = Vec::new();
    if !title.trim().is_empty() {
        parts.push(format!("# {}", title.trim()));
    }
    if !content.trim().is_empty() {
        parts.push(format!("【手写笔记】\n{}", content.trim()));
    }
    if !transcript.trim().is_empty() {
        parts.push(format!("【会议转录】\n{}", transcript.trim()));
    }
    parts.join("\n\n")
}

/// 取用户配置的提示词；未配置或只有空白时回落到内置默认。
///
/// 空串按「未配置」处理，避免用户误删后模型收到空系统提示词、
/// 直接把素材原样吐回来。
fn resolve_prompt(custom: Option<&str>, default: &str) -> String {
    custom
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

/// 把 `{index}` / `{total}` 换成当前段号与总段数。
///
/// 用户模板里不写占位符也不报错——那样每段会得到同样的指令，
/// 只是丢了「第几部分」的上下文，属于可接受的自由度。
fn render_part_prompt(template: &str, index: usize, total: usize) -> String {
    template
        .replace("{index}", &index.to_string())
        .replace("{total}", &total.to_string())
}

/// 去掉模型偶尔加的 ``` 围栏，别把代码块写进笔记。
fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let without_open = trimmed
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let without_close = without_open
        .rsplit_once("```")
        .map(|(body, _)| body)
        .unwrap_or(without_open);
    without_close.trim().to_string()
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.to_string())
    } else {
        Ok(())
    }
}

fn emit_status(
    app: &tauri::AppHandle,
    note_id: i64,
    status: &str,
    part: usize,
    total: usize,
    message: Option<&str>,
) {
    let mut payload = json!({
        "noteId": note_id,
        "status": status,
        "part": part,
        "total": total,
    });
    if let Some(message) = message {
        payload["message"] = json!(message);
    }
    let _ = app.emit(EVENT, payload);
}

/// 解析当前生效的 LLM 端点与凭据；无凭据时给出可操作的错误。
async fn resolve_credentials(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<(LlmEndpoint, String), String> {
    let config = state.llm_provider_config();
    let endpoint = llm_provider::endpoint_for_config(&config);
    let manual_key = llm_provider::load_api_key_for_provider(app, &endpoint.provider);
    let api_key = codex_oauth_service::resolve_api_key_for_provider_with_auth_mode(
        app,
        state,
        &endpoint.provider,
        &manual_key,
        config.openai_auth_mode,
        config.xai_auth_mode,
    )
    .await?;
    if api_key.trim().is_empty() {
        return Err(
            "还没有可用的 AI 凭据：请在设置里登录当前服务或填写 API Key，再生成摘要".into(),
        );
    }
    Ok((endpoint, api_key))
}

async fn call_llm(
    app: &tauri::AppHandle,
    state: &AppState,
    endpoint: &LlmEndpoint,
    api_key: &str,
    system_prompt: &str,
    user_text: &str,
    stream: bool,
) -> Result<String, String> {
    let options = llm_client::LlmRequestOptions {
        auth_context: Some((app, state)),
        stream,
        stream_event: stream.then_some(EVENT),
        stream_progress_timeout_secs: stream.then_some(STREAM_PROGRESS_TIMEOUT_SECS),
        stream_total_timeout_secs: Some(STREAM_TOTAL_TIMEOUT_SECS),
        ..Default::default()
    };
    let user_input = llm_client::LlmUserInput::from(user_text);
    let body = llm_client::build_llm_body(endpoint, system_prompt, &user_input, options);
    llm_client::send_llm_request(
        &state.http_client,
        endpoint,
        api_key,
        &body,
        user_text.chars().count(),
        Some(app),
        options,
    )
    .await
}

/// 生成一篇笔记的摘要。
///
/// 失败与取消都返回 `Err`，调用方据此决定「保留旧摘要」而不是写入半截结果。
pub async fn generate(
    app: &tauri::AppHandle,
    state: &AppState,
    note: &notes_service::NoteDetail,
    cancel: &AtomicBool,
) -> Result<SummaryOutcome, String> {
    let transcript = note.transcript.clone().unwrap_or_default();
    if transcript.trim().is_empty() {
        return Err("这篇笔记还没有转录内容：先录音并转成文字，或从历史记录存为笔记".into());
    }

    let content = note.content.clone();
    emit_status(app, note.id, "preparing", 0, 0, None);

    // 提示词每次生成现读设置：用户改完立刻生效，不用重启应用。
    let (custom_merge, custom_part) = state.with_profile(|profile| {
        (
            profile.note_summary_prompt.clone(),
            profile.note_summary_part_prompt.clone(),
        )
    });
    let merge_prompt = resolve_prompt(custom_merge.as_deref(), DEFAULT_MERGE_PROMPT);
    let part_template = resolve_prompt(custom_part.as_deref(), DEFAULT_PART_PROMPT);

    let material = build_material(&note.title, &content, &transcript);
    let parts = split_parts(&material, MAX_PART_TOKENS);
    let total = parts.len().max(1);

    let (endpoint, api_key) = resolve_credentials(app, state).await?;

    // 短素材：一次调用直接出终稿，省掉「先摘要再摘要」的损耗。
    let (system_prompt, user_text) = if total <= 1 {
        (merge_prompt.clone(), material.clone())
    } else {
        let mut digests = Vec::with_capacity(total);
        for (index, part) in parts.iter().enumerate() {
            check_cancel(cancel)?;
            emit_status(app, note.id, "part", index + 1, total, None);
            let digest = call_llm(
                app,
                state,
                &endpoint,
                &api_key,
                &render_part_prompt(&part_template, index + 1, total),
                part,
                false,
            )
            .await?;
            let digest = digest.trim().to_string();
            if !digest.is_empty() {
                digests.push(format!("## 第 {} 部分要点\n{digest}", index + 1));
            }
        }
        if digests.is_empty() {
            return Err("模型没有返回任何内容，请稍后重试".into());
        }
        let merged = format!("{}\n\n{}", digests.join("\n\n"), {
            if content.trim().is_empty() {
                String::new()
            } else {
                format!("【手写笔记】\n{}", content.trim())
            }
        });
        (merge_prompt, merged)
    };

    check_cancel(cancel)?;
    emit_status(app, note.id, "merging", total, total, None);
    let raw = call_llm(
        app,
        state,
        &endpoint,
        &api_key,
        &system_prompt,
        &user_text,
        true,
    )
    .await?;
    check_cancel(cancel)?;

    let summary = strip_code_fence(&raw);
    if summary.is_empty() {
        return Err("模型返回的摘要为空，请稍后重试".into());
    }

    Ok(SummaryOutcome {
        summary,
        model: endpoint.model,
        source_hash: notes_service::source_hash(&transcript, &content),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_short_material_in_one_part() {
        let parts = split_parts("# 周会\n\n【会议转录】\n大家好", MAX_PART_TOKENS);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("周会"));
    }

    #[test]
    fn split_prefers_paragraph_boundaries_and_respects_budget() {
        let paragraph = "这是一段会议记录。".repeat(400); // 约 3600 字
        let text = vec![paragraph; 4].join("\n\n");
        let parts = split_parts(&text, 1_000);
        assert!(parts.len() >= 4);
        for part in &parts {
            assert!(
                estimate_tokens(part) <= 1_000 * 5 / 4,
                "part too large: {} tokens",
                estimate_tokens(part)
            );
        }
        assert!(parts.iter().all(|part| part.contains("这是一段会议记录")));
    }

    #[test]
    fn oversized_paragraph_is_cut_without_losing_text() {
        let text = "结论已经达成。".repeat(5_000);
        let parts = split_parts(&text, 500);
        assert!(parts.len() > 1);
        let joined: String = parts.concat();
        assert_eq!(joined.matches('。').count(), 5_000);
    }

    #[test]
    fn code_fence_is_removed_but_plain_text_is_kept() {
        assert_eq!(
            strip_code_fence("```markdown\n## 概要\n内容\n```"),
            "## 概要\n内容"
        );
        assert_eq!(strip_code_fence("## 概要\n内容"), "## 概要\n内容");
    }

    #[test]
    fn custom_prompt_falls_back_to_builtin_when_blank() {
        assert_eq!(resolve_prompt(None, "内置"), "内置");
        assert_eq!(resolve_prompt(Some("   \n\t "), "内置"), "内置");
        assert_eq!(resolve_prompt(Some(" 我的要求 "), "内置"), "我的要求");
    }

    #[test]
    fn part_prompt_fills_index_and_total_placeholders() {
        assert_eq!(
            render_part_prompt("第 {index}/{total} 段", 2, 5),
            "第 2/5 段"
        );
        // 用户模板没写占位符时原样返回，不该报错。
        assert_eq!(render_part_prompt("只提炼要点", 2, 5), "只提炼要点");
    }

    #[test]
    fn builtin_prompts_are_not_empty() {
        assert!(!DEFAULT_MERGE_PROMPT.trim().is_empty());
        assert!(DEFAULT_PART_PROMPT.contains("{index}"));
        assert!(DEFAULT_PART_PROMPT.contains("{total}"));
    }
}
