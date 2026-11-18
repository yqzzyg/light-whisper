//! 笔记与笔记目录命令。
//!
//! 命令层只做薄壳：参数透传给 `notes_service`，业务规则（重名校验、事务边界、
//! 分页裁剪）都在 service 里，避免两条调用路径各自维护一套规则。

use crate::services::notes_service;
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

/// 删除笔记。返回被删笔记的音频文件名（P3 起由录音模块据此清文件）。
#[tauri::command]
pub async fn delete_note(id: i64) -> Result<Option<String>, String> {
    notes_service::delete_note(id).await
}

#[tauri::command]
pub async fn move_note(
    id: i64,
    folder_id: Option<i64>,
) -> Result<Option<notes_service::NoteDetail>, String> {
    notes_service::move_note(id, folder_id).await
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
    use super::sanitize_file_name;

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
