//! 会议录音：只往磁盘写，不做实时转写。
//!
//! 与听写链路的区别：
//!
//! - **不跑 ASR**：录音期间 CPU 应该接近空闲，这是「先录后转」省电的前提。
//! - **边录边落盘**：每 `FLUSH_INTERVAL_MS` 把采集缓冲排空、重采样到 16k、追加进
//!   WAV。进程被杀死最多丢一个 tick 的数据，而不是整场会议。
//! - **只存文件名**：`note_recordings.audio_file` 存的是 `notes_audio/` 下的单段文件名，
//!   目录与读取都经过 `paths::safe_file_in_dir`。
//!
//! 互斥：`AppState::note_capture` 保证同一时刻只有一个录音会话；开始前还会检查
//! 听写是否在进行（两者抢同一个麦克风）。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::services::audio_service::resample::ResamplerState;
use crate::services::audio_service::spawn_audio_capture_thread;
use crate::services::notes_service;
use crate::services::wav_writer::WavAppender;
use crate::state::AppState;
use crate::utils::paths;

/// 排空采集缓冲的间隔。原计划 30 秒，这里收到 0.5 秒：
/// 崩溃时最多丢半秒、UI 计时与电平需要更细的粒度，而 16k 单声道每秒只有 32KB。
const FLUSH_INTERVAL_MS: u64 = 500;
pub const NOTE_RECORDING_EVENT: &str = "note-recording-state";

const RECORDING_ALREADY_ACTIVE: &str = "已有会议录音正在进行中";
const RECORDING_BLOCKED_BY_DICTATION: &str = "听写正在进行，请先结束听写再开始会议录音";
const NO_ACTIVE_RECORDING: &str = "当前没有正在进行的会议录音";

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// 前端可见的录音状态。`stop`/`cancel` 的返回值与 `note-recording-state` 事件
/// 用的是同一个结构，前端只维护一份状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteRecordingSnapshot {
    pub active: bool,
    pub session_id: Option<u64>,
    pub note_id: Option<i64>,
    pub elapsed_sec: f64,
    /// 最近一个 tick 的峰值电平（0.0–1.0），用于画电平条。
    pub level: f32,
    /// 仅停止后回填：录音文件名与时长。
    pub audio_file: Option<String>,
    pub duration_sec: Option<f64>,
    pub error: Option<String>,
}

impl NoteRecordingSnapshot {
    fn idle() -> Self {
        Self {
            active: false,
            session_id: None,
            note_id: None,
            elapsed_sec: 0.0,
            level: 0.0,
            audio_file: None,
            duration_sec: None,
            error: None,
        }
    }
}

/// 一次录音会话。持有采集线程与落盘任务的全部句柄，`stop`/`cancel` 靠它收尾。
pub struct NoteCapture {
    pub session_id: u64,
    pub note_id: i64,
    pub file_name: String,
    /// 采集侧的原始采样率。收尾时补写最后一段必须用它构造重采样器，
    /// 否则 48k 缓冲会被当成 16k 直接写进文件，音调全错。
    sample_rate: u32,
    started_at: Instant,
    stop_flag: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    samples: Arc<Mutex<Vec<i16>>>,
    writer: Arc<Mutex<Option<WavAppender>>>,
    error: Arc<Mutex<Option<String>>>,
    audio_thread: Option<std::thread::JoinHandle<()>>,
    flush_task: Option<tokio::task::JoinHandle<()>>,
}

impl NoteCapture {
    fn elapsed_sec(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Acquire))
    }

    fn snapshot(&self, active: bool) -> NoteRecordingSnapshot {
        NoteRecordingSnapshot {
            active,
            session_id: Some(self.session_id),
            note_id: Some(self.note_id),
            elapsed_sec: self.elapsed_sec(),
            level: self.level(),
            audio_file: None,
            duration_sec: None,
            error: self.error.lock().clone(),
        }
    }
}

fn emit_state(app: &AppHandle, snapshot: &NoteRecordingSnapshot) {
    if let Err(error) = app.emit(NOTE_RECORDING_EVENT, snapshot) {
        log::warn!("发送会议录音状态失败: {error}");
    }
}

fn record_error(slot: &Mutex<Option<String>>, message: String) {
    let mut guard = slot.lock();
    if guard.is_none() {
        log::error!("会议录音写入失败: {message}");
        *guard = Some(message);
    }
}

/// 把采集缓冲里现有样本排空并追加落盘。
fn drain_samples(
    samples: &Mutex<Vec<i16>>,
    resampler: &mut ResamplerState,
    writer: &Mutex<Option<WavAppender>>,
    level: &AtomicU32,
    error: &Mutex<Option<String>>,
) {
    let chunk = {
        let mut guard = samples.lock();
        if guard.is_empty() {
            level.store(0f32.to_bits(), Ordering::Release);
            return;
        }
        std::mem::take(&mut *guard)
    };

    let peak = chunk
        .iter()
        .map(|sample| sample.unsigned_abs())
        .max()
        .unwrap_or(0);
    level.store(
        (f32::from(peak) / f32::from(i16::MAX)).to_bits(),
        Ordering::Release,
    );

    let converted = match resampler.push_i16(&chunk) {
        Ok(converted) => converted,
        Err(message) => {
            record_error(error, format!("重采样失败: {message}"));
            return;
        }
    };
    if converted.is_empty() {
        return;
    }
    if let Some(writer) = writer.lock().as_mut() {
        if let Err(message) = writer.append(&converted) {
            record_error(error, message);
        }
    }
}

/// 启动占位：无论 `start` 从哪条路径返回（成功、麦克风失败、写文件失败）都复位，
/// 否则一次失败会把录音功能永久锁死。
struct StartingGuard<'a>(&'a AtomicBool);

impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 开始为指定笔记录音。录音本身不做转写，停止后才由用户手动触发。
pub async fn start(
    app: &AppHandle,
    state: &AppState,
    note_id: i64,
) -> Result<NoteRecordingSnapshot, String> {
    notes_service::initialize().await?;
    notes_service::get_note(note_id)
        .await?
        .ok_or_else(|| "笔记不存在，可能已被删除".to_string())?;

    // 听写与会议录音抢同一个麦克风，先拒绝，避免用户以为只录了一场。
    if state.recording.recording.lock().is_some() {
        return Err(RECORDING_BLOCKED_BY_DICTATION.to_string());
    }
    if state.note_capture.session.lock().is_some()
        || state.note_capture.starting.swap(true, Ordering::AcqRel)
    {
        return Err(RECORDING_ALREADY_ACTIVE.to_string());
    }
    let _starting = StartingGuard(&state.note_capture.starting);

    let session_id = now_millis();
    let file_name = format!("note-{note_id}-{session_id}.wav");
    let directory = paths::notes_audio_dir();
    let path = paths::safe_file_in_dir(&directory, &file_name)
        .ok_or_else(|| "录音文件名非法".to_string())?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::with_capacity(16_000 * 8)));
    let level = Arc::new(AtomicU32::new(0f32.to_bits()));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let capture_samples = samples.clone();
    let capture_stop = stop_flag.clone();
    let device = state.selected_input_device_name();
    let capture_result = tokio::task::spawn_blocking(move || {
        spawn_audio_capture_thread(capture_stop, capture_samples, device)
    })
    .await
    .map_err(|error| format!("录音启动任务异常结束: {error}"))?;

    let (audio_thread, sample_rate) = match capture_result {
        Ok(result) => result,
        Err(error) => return Err(format!("启动麦克风失败: {error}")),
    };

    // 头部必须声明**重采样之后**的采样率：`drain_samples` 一律把采集数据转成
    // `TARGET_SAMPLE_RATE` 再追加，若这里写采集侧原始率（设备不支持 16k 时是
    // 48k），文件会被当成 3 倍慢速音频——时长算错、转写音调全错。
    let writer = match WavAppender::create(&path, super::audio_service::TARGET_SAMPLE_RATE) {
        Ok(writer) => writer,
        Err(error) => {
            stop_flag.store(true, Ordering::Release);
            let _ = tokio::task::spawn_blocking(move || {
                let _ = audio_thread.join();
            })
            .await;
            return Err(error);
        }
    };

    let writer = Arc::new(Mutex::new(Some(writer)));
    let started_at = Instant::now();

    let mut capture = NoteCapture {
        session_id,
        note_id,
        file_name,
        sample_rate,
        started_at,
        stop_flag: stop_flag.clone(),
        level: level.clone(),
        samples: samples.clone(),
        writer: writer.clone(),
        error: error.clone(),
        audio_thread: Some(audio_thread),
        flush_task: None,
    };

    let flush_task = {
        let app_handle = app.clone();
        let samples = samples.clone();
        let writer = writer.clone();
        let level = level.clone();
        let error = error.clone();
        let stop_flag = stop_flag.clone();
        tokio::spawn(async move {
            let mut resampler = match ResamplerState::new(sample_rate) {
                Ok(resampler) => resampler,
                Err(message) => {
                    record_error(&error, format!("初始化重采样失败: {message}"));
                    stop_flag.store(true, Ordering::Release);
                    return;
                }
            };
            let mut ticker = tokio::time::interval(Duration::from_millis(FLUSH_INTERVAL_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                drain_samples(&samples, &mut resampler, &writer, &level, &error);
                let payload = NoteRecordingSnapshot {
                    active: true,
                    session_id: Some(session_id),
                    note_id: Some(note_id),
                    elapsed_sec: started_at.elapsed().as_secs_f64(),
                    level: f32::from_bits(level.load(Ordering::Acquire)),
                    audio_file: None,
                    duration_sec: None,
                    error: error.lock().clone(),
                };
                emit_state(&app_handle, &payload);
            }
        })
    };
    capture.flush_task = Some(flush_task);

    let snapshot = capture.snapshot(true);
    *state.note_capture.session.lock() = Some(capture);
    emit_state(app, &snapshot);
    Ok(snapshot)
}

fn take_session(state: &AppState) -> Result<NoteCapture, String> {
    state
        .note_capture
        .session
        .lock()
        .take()
        .ok_or_else(|| NO_ACTIVE_RECORDING.to_string())
}

/// 停掉采集与落盘任务，返回会话与最终音频时长。
async fn finalize_capture(mut capture: NoteCapture) -> Result<(NoteCapture, f64), String> {
    capture.stop_flag.store(true, Ordering::Release);
    if let Some(task) = capture.flush_task.take() {
        // 中止后仍可能正在一次 drain 内（无 await 点），下面取写锁会等它结束。
        task.abort();
    }
    if let Some(thread) = capture.audio_thread.take() {
        let _ = tokio::task::spawn_blocking(move || {
            let _ = thread.join();
        })
        .await;
    }

    // 采集线程已退出，缓冲里剩下的就是最后一小段。用采集侧的原始采样率
    // 构造重采样器，和录制期间那个是同一套参数。
    let mut resampler = ResamplerState::new(capture.sample_rate)
        .map_err(|error| format!("初始化重采样失败: {error}"))?;
    drain_samples(
        &capture.samples,
        &mut resampler,
        &capture.writer,
        &capture.level,
        &capture.error,
    );

    let writer = capture
        .writer
        .lock()
        .take()
        .ok_or_else(|| "录音写入器已释放".to_string())?;
    let duration = writer.finish()?;
    Ok((capture, duration))
}

/// 停止录音，把文件名与时长写回笔记。
pub async fn stop(app: &AppHandle, state: &AppState) -> Result<NoteRecordingSnapshot, String> {
    let capture = take_session(state)?;
    let (capture, duration) = finalize_capture(capture).await?;
    let write_error = capture.error.lock().clone();

    // 每次 stop 都是新的一段：不再覆盖 note 上可能的旧录音。
    notes_service::add_note_recording(capture.note_id, &capture.file_name, Some(duration)).await?;

    let snapshot = NoteRecordingSnapshot {
        active: false,
        session_id: Some(capture.session_id),
        note_id: Some(capture.note_id),
        elapsed_sec: 0.0,
        level: 0.0,
        audio_file: Some(capture.file_name.clone()),
        duration_sec: Some(duration),
        error: write_error,
    };
    emit_state(app, &snapshot);
    Ok(snapshot)
}

/// 取消录音：删除半成品文件，笔记保持原样。
pub async fn cancel(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let capture = take_session(state)?;
    let (capture, _) = finalize_capture(capture).await?;
    delete_audio_file(&capture.file_name);
    emit_state(app, &NoteRecordingSnapshot::idle());
    Ok(())
}

/// 当前状态。页面切回来时用它同步，避免只依赖事件。
pub fn snapshot(state: &AppState) -> NoteRecordingSnapshot {
    match state.note_capture.session.lock().as_ref() {
        Some(capture) => capture.snapshot(true),
        None => NoteRecordingSnapshot::idle(),
    }
}

/// 正在录制的笔记被删除时，先停掉录音，避免往一行已经不存在的数据上写音频。
pub async fn cancel_if_recording_note(
    app: &AppHandle,
    state: &AppState,
    note_id: i64,
) -> Result<bool, String> {
    let matches = state
        .note_capture
        .session
        .lock()
        .as_ref()
        .is_some_and(|capture| capture.note_id == note_id);
    if !matches {
        return Ok(false);
    }
    cancel(app, state).await?;
    Ok(true)
}

/// 删除笔记音频文件。文件名非法或文件已不存在时静默通过。
pub fn delete_audio_file(file_name: &str) {
    let Some(path) = paths::safe_file_in_dir(&paths::notes_audio_dir(), file_name) else {
        log::warn!("忽略非法的笔记录音文件名: {file_name}");
        return;
    };
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("删除笔记录音失败 {}: {error}", path.display());
        }
    }
}

/// 启动清理：先修复被异常中断的录音头，再删掉没有任何笔记引用的文件。
///
/// 修复必须在删除之前：崩溃留下的文件虽然没人引用，但先让它成为合法 WAV
/// 才能判断到底是「孤儿」还是「上一轮没来得及挂到笔记上的录音」。
pub async fn cleanup_startup() -> Result<(), String> {
    notes_service::initialize().await?;
    let referenced = notes_service::audio_file_names().await?;
    tokio::task::spawn_blocking(move || {
        let directory = paths::notes_audio_dir();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                log::warn!("读取笔记录音目录失败 {}: {error}", directory.display());
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("wav") {
                continue;
            }
            if let Err(error) = crate::services::wav_writer::repair_header(&path) {
                log::warn!("修复录音头失败 {}: {error}", path.display());
            }
            let file_name = entry.file_name().to_string_lossy().to_string();
            if referenced.contains(&file_name) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("清理未被引用的录音文件: {file_name}"),
                Err(error) => log::warn!("清理录音文件失败 {}: {error}", path.display()),
            }
        }
    })
    .await
    .map_err(|error| format!("清理笔记录音任务失败: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempWav(PathBuf);

    impl TempWav {
        fn new(tag: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "minutes-note-rec-{}-{nonce}-{tag}.wav",
                std::process::id()
            ));
            Self(path)
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn drain_appends_pending_samples_and_publishes_peak_level() {
        let temp = TempWav::new("drain");
        let writer = Mutex::new(Some(WavAppender::create(&temp.0, 16_000).unwrap()));
        let samples = Mutex::new(vec![1_000i16, -2_000, 3_000]);
        let level = AtomicU32::new(0);
        let error = Mutex::new(None);
        let mut resampler = ResamplerState::new(16_000).unwrap();

        drain_samples(&samples, &mut resampler, &writer, &level, &error);

        assert!(samples.lock().is_empty(), "缓冲必须被排空");
        let peak = f32::from_bits(level.load(Ordering::Acquire));
        assert!(
            (peak - 3_000.0 / 32_767.0).abs() < 1e-6,
            "峰值电平不对: {peak}"
        );
        assert!(error.lock().is_none());

        let duration = writer.lock().take().unwrap().finish().unwrap();
        assert!((duration - 3.0 / 16_000.0).abs() < 1e-9);
        assert_eq!(std::fs::read(&temp.0).unwrap().len(), 44 + 6);
    }

    #[test]
    fn drain_resets_level_to_zero_when_the_buffer_is_empty() {
        let temp = TempWav::new("idle");
        let writer = Mutex::new(Some(WavAppender::create(&temp.0, 16_000).unwrap()));
        let samples: Mutex<Vec<i16>> = Mutex::new(Vec::new());
        let level = AtomicU32::new(1.0f32.to_bits());
        let error = Mutex::new(None);
        let mut resampler = ResamplerState::new(16_000).unwrap();

        drain_samples(&samples, &mut resampler, &writer, &level, &error);

        assert_eq!(level.load(Ordering::Acquire), 0f32.to_bits());
    }

    #[test]
    fn drain_survives_a_released_writer_without_panicking() {
        let samples = Mutex::new(vec![10i16; 8]);
        let writer: Mutex<Option<WavAppender>> = Mutex::new(None);
        let level = AtomicU32::new(0);
        let error = Mutex::new(None);
        let mut resampler = ResamplerState::new(16_000).unwrap();

        drain_samples(&samples, &mut resampler, &writer, &level, &error);

        assert!(samples.lock().is_empty());
        assert!(error.lock().is_none());
    }

    #[test]
    fn snapshot_is_idle_without_a_session() {
        let state = AppState::default();
        let snapshot = snapshot(&state);
        assert!(!snapshot.active);
        assert_eq!(snapshot.note_id, None);
        assert_eq!(snapshot.elapsed_sec, 0.0);
    }
}
