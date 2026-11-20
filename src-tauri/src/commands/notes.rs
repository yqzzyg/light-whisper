//! 笔记与笔记目录命令。
//!
//! 命令层只做薄壳：参数透传给 `notes_service`，业务规则（重名校验、事务边界、
//! 分页裁剪）都在 service 里，避免两条调用路径各自维护一套规则。

use tauri::Manager;

use crate::services::notes_service;
use crate::state::AppState;
use crate::utils::paths;

#[tauri::command]
pub async fn list_note_folders() -> Result<Vec<notes_service::NoteFolder>, String> {
    notes_service::list_folders().await
}

#[tauri::command]
pub async fn create_note_folder(name: String) -> Result<notes_service::NoteFolder, String> {
    notes_service::create_folder(&name).await
}

#[tauri::command]
pub async fn rename_note_folder(
    id: i64,
    name: String,
) -> Result<notes_service::NoteFolder, String> {
    notes_service::rename_folder(id, &name).await
}

/// 删目录不删笔记：目录内笔记退化为「未分类」。
#[tauri::command]
pub async fn delete_note_folder(id: i64) -> Result<bool, String> {
    notes_service::delete_folder(id).await
}

#[tauri::command]
pub async fn reorder_note_folders(ids: Vec<i64>) -> Result<Vec<notes_service::NoteFolder>, String> {
    notes_service::reorder_folders(ids).await
}

#[tauri::command]
pub async fn list_notes(
    query: notes_service::NoteQuery,
) -> Result<notes_service::NotePage, String> {
    notes_service::list_notes(query).await
}

#[tauri::command]
pub async fn get_note(id: i64) -> Result<Option<notes_service::NoteDetail>, String> {
    notes_service::get_note(id).await
}

#[tauri::command]
pub async fn create_note(
    folder_id: Option<i64>,
    title: String,
    kind: String,
) -> Result<notes_service::NoteDetail, String> {
    notes_service::create_note(folder_id, &title, &kind).await
}

#[tauri::command]
pub async fn update_note(
    id: i64,
    patch: notes_service::NotePatch,
) -> Result<Option<notes_service::NoteDetail>, String> {
    notes_service::update_note(id, patch).await
}

/// 删除笔记。返回被删笔记所有段的音频文件名。
///
/// 正在录这条笔记时先取消录音：否则停止后音频会挂到一行已经不存在的数据上，
/// 文件也就没人认领了。取消失败不阻止删除，残留文件由启动清理兜底。
#[tauri::command]
pub async fn delete_note(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<Vec<String>>, String> {
    if let Err(error) =
        crate::services::note_recording_service::cancel_if_recording_note(&app, state.inner(), id)
            .await
    {
        log::warn!("删除笔记前取消录音失败（继续删除）: {error}");
    }
    let audio_files = notes_service::delete_note(id).await?;
    if let Some(files) = audio_files.as_ref() {
        for file_name in files {
            crate::services::note_recording_service::delete_audio_file(file_name);
        }
    }
    Ok(audio_files)
}

#[tauri::command]
pub async fn list_note_recordings(
    note_id: i64,
) -> Result<Vec<notes_service::NoteRecording>, String> {
    notes_service::list_note_recordings(note_id).await
}

/// 删除一段录音：先删库里的引用，再删磁盘文件。重算聚合转录由 service 完成。
#[tauri::command]
pub async fn delete_note_recording(id: i64) -> Result<(), String> {
    if let Some((_, file_name)) = notes_service::delete_note_recording(id).await? {
        crate::services::note_recording_service::delete_audio_file(&file_name);
    }
    Ok(())
}

/// 为某段录音生成可在应用内 `<audio>` 播放的 asset URL。
///
/// asset 协议的静态 scope 是空的（不给整个数据目录开门），这里按段授权单个
/// 文件——只有用户明确点了播放的那一段才会被放行，且文件必须真实存在。
#[tauri::command]
pub async fn note_recording_audio_url(app: tauri::AppHandle, id: i64) -> Result<String, String> {
    let recording = notes_service::get_note_recording(id)
        .await?
        .ok_or_else(|| "录音片段不存在或已被清理".to_string())?;
    let path = paths::safe_file_in_dir(&paths::notes_audio_dir(), &recording.audio_file)
        .ok_or_else(|| "录音文件名非法".to_string())?;
    if !path.is_file() {
        return Err("录音文件不存在或已被清理".into());
    }
    app.asset_protocol_scope()
        .allow_file(&path)
        .map_err(|error| format!("授权播放录音失败: {error}"))?;
    // convertFileSrc 只需要 plain 路径（去掉 Windows 的 \\?\ 前缀），URL 由前端拼。
    Ok(paths::strip_win_prefix(&path))
}

/// 在系统文件管理器中定位某一笔记录音。
/// Windows 用 explorer /select 选中文件，其它平台打开所在目录。
#[tauri::command]
pub async fn reveal_note_audio(id: i64) -> Result<(), String> {
    let Some(recording) = notes_service::get_note_recording(id).await? else {
        return Err("录音不存在或已被清理".into());
    };
    let Some(path) = paths::safe_file_in_dir(&paths::notes_audio_dir(), &recording.audio_file)
    else {
        return Err("录音文件名非法".into());
    };
    if !path.is_file() {
        return Err("录音文件不存在或已被清理".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map_err(|error| format!("打开文件目录失败: {error}"))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let directory = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        std::process::Command::new("xdg-open")
            .arg(directory)
            .spawn()
            .map_err(|error| format!("打开文件目录失败: {error}"))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn move_note(
    id: i64,
    folder_id: Option<i64>,
) -> Result<Option<notes_service::NoteDetail>, String> {
    notes_service::move_note(id, folder_id).await
}

/// 用正文首行做标题：去掉行首的 Markdown 前缀符号，超过 40 字截断。
///
/// 历史记录本身没有标题，让用户面对一列「未命名笔记」无法区分。
fn derive_note_title(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let stripped = first.trim_start_matches(['#', '>', '-', '*', '+', ' ']);
    stripped.chars().take(40).collect()
}

/// 历史记录 → 笔记：转录、时长、`history_id` 一起带过去，音频不复制。
///
/// 同一条历史记录重复调用返回已有笔记，用户连点不会堆出重复笔记。
#[tauri::command]
pub async fn create_note_from_history(
    history_id: i64,
    folder_id: Option<i64>,
) -> Result<notes_service::NoteDetail, String> {
    let stored = crate::services::history_service::get(history_id)
        .await?
        .ok_or_else(|| "历史记录不存在，可能已被删除".to_string())?;
    let record = stored.record;
    let transcript = record.text.trim().to_string();
    if transcript.is_empty() {
        return Err("这条记录没有转写文本，无法存为笔记".into());
    }
    let title = derive_note_title(&transcript);
    let (note, _) = notes_service::create_note_from_transcript(
        folder_id,
        &title,
        "import",
        &transcript,
        record.duration_sec,
        history_id,
    )
    .await?;
    Ok(note)
}

/// 历史记录反查笔记 id：历史卡片上的「打开笔记」用它跳转。
///
/// 返回 `None` 表示笔记已被删除（历史行还在），前端据此提示而不是静默跳不过去。
#[tauri::command]
pub async fn find_note_by_history(history_id: i64) -> Result<Option<i64>, String> {
    notes_service::find_note_id_by_history(history_id).await
}

/// 只保留最后一段文件名，挡住前端拼出来的路径分隔符与空名。
fn sanitize_file_name(raw: &str) -> Result<String, String> {
    let name = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let name: String = name.chars().take(120).collect();
    if name.is_empty() || name == "." || name == ".." {
        return Err("导出文件名不能为空".into());
    }
    Ok(name)
}

/// 把调用方拼好的文本写到用户选择的位置。
///
/// 内容由前端按当前界面语言拼装（导出文档里的标题、小节名都跟着界面语言走），
/// 后端只负责「弹保存框 + 落盘」，不解释文本语义，也不改动库里的笔记。
#[tauri::command]
pub async fn save_text_file(
    window: tauri::WebviewWindow,
    file_name: String,
    content: String,
) -> Result<Option<String>, String> {
    let file_name = sanitize_file_name(&file_name)?;
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty() && ext.len() <= 8)
        .unwrap_or_else(|| "txt".to_string());
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .add_filter(extension.to_ascii_uppercase(), &[extension.as_str()])
        .set_file_name(&file_name);
    if let Some(directory) = dirs::download_dir() {
        dialog = dialog.set_directory(directory);
    }
    let Some(path) = dialog.save_file().await else {
        return Ok(None);
    };
    tokio::fs::write(path.path(), content.as_bytes())
        .await
        .map_err(|error| format!("写入导出文件失败: {error}"))?;
    Ok(Some(paths::strip_win_prefix(path.path())))
}

#[cfg(test)]
mod tests {
    use super::{derive_note_title, sanitize_file_name};

    #[test]
    fn note_title_comes_from_the_first_content_line() {
        assert_eq!(derive_note_title("周会纪要\n\n第二段"), "周会纪要");
        assert_eq!(derive_note_title("# 标题行\n正文"), "标题行");
        assert_eq!(derive_note_title("\n\n  第二行才有内容"), "第二行才有内容");
        assert_eq!(derive_note_title(""), "");
    }

    #[test]
    fn note_title_is_capped_without_splitting_characters() {
        let title = derive_note_title(&"会".repeat(60));
        assert_eq!(title.chars().count(), 40);
    }

    #[test]
    fn export_file_name_keeps_only_the_last_segment() {
        assert_eq!(sanitize_file_name("周会.md").unwrap(), "周会.md");
        assert_eq!(sanitize_file_name("../../周会.md").unwrap(), "周会.md");
        assert_eq!(sanitize_file_name(r"C:\temp\周会.md").unwrap(), "周会.md");
        assert_eq!(sanitize_file_name("  周会.md  ").unwrap(), "周会.md");
    }

    #[test]
    fn export_file_name_rejects_blank_and_dot_paths() {
        assert!(sanitize_file_name("").is_err());
        assert!(sanitize_file_name("   ").is_err());
        assert!(sanitize_file_name("..").is_err());
        assert!(sanitize_file_name("dir/").is_err());
    }

    #[test]
    fn export_file_name_is_length_capped() {
        let name = format!("{}.md", "长".repeat(200));
        assert_eq!(sanitize_file_name(&name).unwrap().chars().count(), 120);
    }
}
