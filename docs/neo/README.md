# neo — 新方案文档

这里放**新写的**功能方案、PRD 与实施清单。

`docs/` 根目录是源码自带的既有技术文档（`r2t2-native.md`、`privacy.md`、
`development.md`、`releasing.md`、`api-defaults.md` 等），那些是上游维护的，
不归这里管，也不要顺手改动。

## 收录规则

- 新增方案文档一律放这个目录，不要直接扔在 `docs/` 根。
- 文件名用小写连字符，如 `notes-feature-plan.md`、`import-audio-transcription-plan.md`。
- 文档里引用代码文件时写**仓库根相对路径**（如 `src-tauri/src/services/notes_service.rs`），
  不要写相对路径——移动文档时相对路径会失效。
- 文档里引用 Rust 文件与符号后，跑一遍 `python scripts/check_doc_refs.py` 校验引用有效。
  该脚本目前只覆盖 `notes-feature-plan.md`，新增文档后可一并加进去。

## 现有文档

| 文档 | 状态 |
|---|---|
| `notes-feature-plan.md` | 笔记功能：方案调研 + 工作清单，P0–P4 已完成，P5 部分落地 |
| `import-audio-transcription-plan.md` | 上传音频转写：详细实施方案 |
| `ui-redesign-prd.md` | UI 改版 PRD，已按实际交付更新 |
