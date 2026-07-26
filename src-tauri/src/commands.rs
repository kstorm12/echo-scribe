//! Tauri commands exposed to the frontend.
//!
//! These wire up:
//!  - permission status checks + deep links into System Settings
//!  - get/update of the user-configurable voice-at-cursor binding
//!  - explicit pipeline start (called by onboarding once permissions are green)
//!
//! The [`AppState`] container is created in [`crate::lib::run`] and managed via
//! Tauri's state container (`app.manage(...)`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, Wry};
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

use crate::asr::downloader::{self, DownloadProgress};
use crate::asr::pipeline::AsrPipeline;
use crate::asr::registry::{self, ModelEntry};
use crate::coordinator::{self, Action, CoordinatorMsg, StateHandle, TrayPipelineState};
use crate::db::chat;
use crate::db::items::{chrono_now_iso, ItemKind};
use crate::db::projects::{Project, ProjectPatch};
use crate::db::tasks::TaskWithItem;
use crate::db::{self, ChatMessage, ChatSession, Db, Item};
use crate::input::binding::{
    code_from_key, key_from_code, Binding, ModifierKind, ModifierSide, SerKey,
};
use crate::input::hotkeys::{spawn_listener, HotkeyEvent};
use crate::input::trigger::HotkeyProblem;
use crate::llm::{self, rag, GenerateRequest, Llm, LlmDownloadProgress, LlmModelEntry};
use crate::permissions::{self, CameraAccessOutcome, MicAccessOutcome, PermissionsStatus, SettingsPane};
use crate::settings::SettingsStore;
use crate::temporal::extract_date_window;
use crate::ui::tray::TrayHandle;

/// Metadata captured at recording-start time so `stop_screen_recording` can
/// persist the correct source/audio info without re-deriving it.
pub struct RecordingMeta {
    pub source_label: String,
    pub has_mic: bool,
    pub has_sysaudio: bool,
    /// Whether `start()` was called with `--hide-cursor`. Persisted on stop
    /// from this call-time value, not from anything the sidecar reports.
    pub cursor_hidden: bool,
}

/// Application-wide state shared by all Tauri commands.
///
/// `tray` is held behind an `Arc<Mutex<_>>` because tray updates fire from
/// the coordinator thread. `binding` is an `Arc<RwLock<_>>` so the rdev
/// listener thread can re-read it on every event without us having to tear
/// the listener down to change the hotkey.
pub struct AppState {
    pub tray: Arc<Mutex<TrayHandle<Wry>>>,
    pub settings: SettingsStore,
    pub binding: Arc<RwLock<Binding>>,
    pub log_capture_binding: Arc<RwLock<Binding>>,
    pub action_binding: Arc<RwLock<Binding>>,
    pub edit_selection_binding: Arc<RwLock<Binding>>,
    pub hotkey_started: AtomicBool,
    /// Friendly, user-facing reasons the configured hotkeys couldn't be
    /// registered (Windows/Linux only — macOS uses the event tap and never
    /// registers anything). Refreshed on every `sync_hotkeys` call and read by
    /// the settings UI via [`get_hotkey_problems`], so a shortcut that another
    /// app already owns shows up in the UI instead of only in the log.
    pub hotkey_problems: Arc<RwLock<Vec<HotkeyProblem>>>,
    /// When `true`, the coordinator drops Pressed/Released events. Toggled
    /// from the tray menu (Pause/Resume hotkeys). The hotkey listeners stay
    /// running so the toggle is instant — only the coordinator filters.
    pub paused_hotkeys: Arc<AtomicBool>,
    /// When `true`, the CGEventTap passes all events through without swallowing
    /// or emitting. Set while the settings UI is in hotkey-capture mode so the
    /// web view's DOM receives raw key events for the rebinder.
    pub rebinding: Arc<AtomicBool>,
    /// Multiplexed channel into the coordinator. Wraps both
    /// hotkey-derived events (with their `Action` tag) and frontend
    /// confirmation messages for LogCapture. Populated on first
    /// `start_pipeline`.
    pub coord_tx: Mutex<Option<UnboundedSender<CoordinatorMsg>>>,
    pub pipeline_state: StateHandle,
    pub asr: Arc<AsrPipeline>,
    pub llm: Arc<Llm>,
    pub embedder: Arc<crate::embed::Embedder>,
    /// On-disk SQLite handle. `None` only if DB initialization failed at
    /// startup (in which case persistence is disabled but Phase 1 behavior
    /// still works — paste-at-cursor must never be blocked by DB issues).
    pub db: Option<Db>,
    /// Root for the user-facing event archive (defaults to `~/EchoScribe/`).
    pub event_log_root: Option<std::path::PathBuf>,
    /// The non-blocking log appender's worker guard. Held for the lifetime
    /// of `AppState` so logs flush on graceful exit — see `lib.rs::run`.
    pub _log_guard: Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>,
    pub meeting_manager: Arc<crate::meeting::MeetingManager>,
    pub active_recording: std::sync::Arc<
        std::sync::Mutex<Option<(crate::screenrec::ScreenrecHandle, RecordingMeta)>>,
    >,
}

#[tauri::command]
pub fn platform_capabilities() -> crate::platform::Capabilities {
    crate::platform::Capabilities::current()
}

/// JSON-friendly mirror of [`Binding`].
///
/// `primary` is the DOM `KeyboardEvent.code` string (e.g. `"ControlRight"`).
/// We translate it to/from `rdev::Key` via [`key_from_code`] and [`code_from_key`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsBinding {
    pub primary: String,
    pub modifiers: Vec<JsModifier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsModifier {
    /// "Control" | "Shift" | "Alt" | "Meta"
    pub kind: String,
    /// "Left" | "Right" | "Either"
    pub side: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BindingConversionError {
    #[error("unknown key code: {0}")]
    UnknownKey(String),
    #[error("unknown modifier kind: {0}")]
    UnknownModifierKind(String),
    #[error("unknown modifier side: {0}")]
    UnknownModifierSide(String),
}

impl TryFrom<JsBinding> for Binding {
    type Error = BindingConversionError;

    fn try_from(js: JsBinding) -> Result<Self, Self::Error> {
        let primary = key_from_code(&js.primary)
            .ok_or_else(|| BindingConversionError::UnknownKey(js.primary.clone()))?;
        let mut modifiers = Vec::with_capacity(js.modifiers.len());
        for m in js.modifiers {
            let kind = match m.kind.as_str() {
                "Control" => ModifierKind::Control,
                "Shift" => ModifierKind::Shift,
                "Alt" => ModifierKind::Alt,
                "Meta" => ModifierKind::Meta,
                other => {
                    return Err(BindingConversionError::UnknownModifierKind(
                        other.to_string(),
                    ))
                }
            };
            let side = match m.side.as_str() {
                "Left" => ModifierSide::Left,
                "Right" => ModifierSide::Right,
                "Either" => ModifierSide::Either,
                other => {
                    return Err(BindingConversionError::UnknownModifierSide(
                        other.to_string(),
                    ))
                }
            };
            modifiers.push((kind, side));
        }
        Ok(Binding {
            primary: SerKey(primary),
            modifiers,
        })
    }
}

impl From<Binding> for JsBinding {
    fn from(b: Binding) -> Self {
        let primary = code_from_key(b.primary.0).unwrap_or("Unknown").to_string();
        let modifiers = b
            .modifiers
            .into_iter()
            .map(|(kind, side)| JsModifier {
                kind: match kind {
                    ModifierKind::Control => "Control",
                    ModifierKind::Shift => "Shift",
                    ModifierKind::Alt => "Alt",
                    ModifierKind::Meta => "Meta",
                }
                .to_string(),
                side: match side {
                    ModifierSide::Left => "Left",
                    ModifierSide::Right => "Right",
                    ModifierSide::Either => "Either",
                }
                .to_string(),
            })
            .collect();
        JsBinding { primary, modifiers }
    }
}

// ----- Tauri commands -----

#[tauri::command]
pub fn permissions_status() -> PermissionsStatus {
    permissions::status()
}

#[tauri::command]
pub fn open_microphone_settings() -> Result<(), String> {
    permissions::open_settings(SettingsPane::Microphone).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_accessibility_settings() -> Result<(), String> {
    permissions::open_settings(SettingsPane::Accessibility).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_screen_recording_settings() -> Result<(), String> {
    permissions::open_settings(SettingsPane::ScreenCapture).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_camera_settings() -> Result<(), String> {
    permissions::open_settings(SettingsPane::Camera).map_err(|e| e.to_string())
}

/// Trigger the macOS in-process microphone prompt (or return the cached
/// decision). Returns `true` if access is granted (now or already), `false`
/// if denied or undetermined.
#[tauri::command]
pub async fn request_microphone_access() -> Result<bool, String> {
    let outcome = permissions::request_microphone().await;
    // Every prompt-capable permission entry point logs when it runs: if a
    // macOS permission dialog ever appears without a user action (e.g. at
    // launch), the daily log names which path triggered it.
    info!(target: "perm", ?outcome, "microphone access requested (may show macOS prompt)");
    Ok(matches!(outcome, MicAccessOutcome::Granted))
}

/// Maps a [`CameraAccessOutcome`] to the string the frontend switches on.
/// Kept as a standalone function (rather than inlined in the command) so the
/// mapping itself is unit-testable.
fn camera_access_outcome_str(outcome: CameraAccessOutcome) -> &'static str {
    match outcome {
        CameraAccessOutcome::Granted => "granted",
        CameraAccessOutcome::Denied => "denied",
        CameraAccessOutcome::Undetermined => "undetermined",
    }
}

/// Trigger the macOS in-process camera prompt (or return the cached
/// decision). Returns "granted" / "denied" / "undetermined" — SetupWindow
/// only acts on "denied" (shows the inline warning); the other two proceed
/// silently, mirroring `request_microphone_access` but as a tri-state string
/// since the UI needs to distinguish "denied" from "undetermined".
#[tauri::command]
pub async fn request_camera_access() -> Result<String, String> {
    let outcome = permissions::request_camera().await;
    info!(target: "screenrec", ?outcome, "camera access requested");
    Ok(camera_access_outcome_str(outcome).to_string())
}

/// Log bridge for the camera self-view window. WKWebView console output is
/// invisible in a production bundle, so the preview page reports its
/// getUserMedia failures here to land the raw error (name + message) in the
/// daily log next to the sidecar's camera events.
#[tauri::command]
pub fn log_camera_preview_error(message: String) {
    warn!(target: "screenrec", %message, "camera self-view getUserMedia failed");
}

/// Log bridge for the WebCodecs export/render pipeline. The whole render runs
/// in the webview, so a failure only reaches `console.error` — invisible in a
/// production bundle, which is why the "See logs" toast previously pointed at
/// a log that had nothing. The editor reports the raw error (name + message +
/// stack) here so an export failure is actually diagnosable from the daily
/// log.
#[tauri::command]
pub fn log_export_error(message: String) {
    error!(target: "screenrec", %message, "video export failed");
}

/// Open (or focus) a dedicated editor window for a recording. One window per
/// recording (label `editor-<id>`), so re-clicking Edit brings the existing
/// window forward instead of spawning a duplicate. The recording id reaches
/// the page via an initialization script — no query string on the asset URL.
#[tauri::command]
pub fn open_recording_editor(
    app: AppHandle,
    id: String,
    title: Option<String>,
) -> Result<(), String> {
    // Window labels only allow [a-zA-Z0-9-/:_]; recording ids should already
    // comply, but filter defensively so a weird id can't fail the window build.
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let label = format!("editor-{safe}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        info!(target: "screenrec", %label, "editor window already open; focusing");
        return Ok(());
    }
    let window_title = title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| format!("Edit — {t}"))
        .unwrap_or_else(|| "Edit recording".to_string());
    let init = format!(
        "window.__EDITOR_RECORDING_ID__ = {};",
        serde_json::to_string(&id).map_err(|e| e.to_string())?
    );
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App("src/editor/index.html".into()),
    )
    .title(&window_title)
    .inner_size(1280.0, 860.0)
    .min_inner_size(980.0, 640.0)
    .initialization_script(&init)
    .build()
    .map_err(|e| {
        error!(target: "screenrec", error = %e, "failed to open editor window");
        format!("Could not open the editor window: {e}")
    })?;
    info!(target: "screenrec", %label, "opened editor window");
    Ok(())
}

/// Trigger the macOS Accessibility prompt. The dialog is a side effect; the
/// returned bool is the current trust state (typically `false` on first
/// call — the user still has to flip the toggle in System Settings).
#[tauri::command]
pub fn prompt_accessibility_access() -> Result<bool, String> {
    let trusted = permissions::prompt_accessibility();
    info!(target: "perm", trusted, "accessibility access prompted (may show macOS prompt)");
    Ok(trusted)
}

/// Trigger the macOS Screen Recording prompt (or return the cached
/// decision). Backs the ScreenCaptureKit-driven "other person" audio
/// track in meetings.
#[tauri::command]
pub fn request_screen_recording_access() -> Result<bool, String> {
    let granted = permissions::request_screen_recording();
    info!(target: "perm", granted, "screen recording access requested (may show macOS prompt)");
    Ok(granted)
}

/// Re-register the global shortcuts from the current bindings and store any
/// user-facing problems for [`get_hotkey_problems`].
///
/// No-op on macOS, where the CGEventTap listeners read the shared `Binding`
/// directly on every key event and so pick up rebinds with nothing to re-register.
pub fn sync_hotkeys(state: &AppState, app: &AppHandle) {
    #[cfg(not(target_os = "macos"))]
    {
        let problems = crate::input::trigger::sync_shortcuts(app, state);
        if let Ok(mut guard) = state.hotkey_problems.write() {
            *guard = problems;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (state, app);
    }
}

/// Hotkeys that could not be registered (empty on macOS, and empty on
/// Windows/Linux when everything registered cleanly).
#[tauri::command]
pub fn get_hotkey_problems(state: State<'_, AppState>) -> Vec<HotkeyProblem> {
    state
        .hotkey_problems
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn get_voice_at_cursor_binding(state: State<'_, AppState>) -> JsBinding {
    let b = state
        .binding
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| crate::settings::default_binding());
    b.into()
}

#[tauri::command]
pub fn update_voice_at_cursor_binding(
    state: State<'_, AppState>,
    app: AppHandle,
    binding: JsBinding,
) -> Result<(), String> {
    let parsed: Binding = binding
        .try_into()
        .map_err(|e: BindingConversionError| e.to_string())?;
    state
        .settings
        .set_voice_at_cursor_binding(parsed.clone())
        .map_err(|e| e.to_string())?;
    {
        let mut guard = state
            .binding
            .write()
            .map_err(|_| "binding lock poisoned".to_string())?;
        *guard = parsed;
    }
    sync_hotkeys(&state, &app);
    Ok(())
}

#[tauri::command]
pub fn get_log_capture_binding(state: State<'_, AppState>) -> JsBinding {
    let b = state
        .log_capture_binding
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| crate::settings::default_log_capture_binding());
    b.into()
}

#[tauri::command]
pub fn update_log_capture_binding(
    state: State<'_, AppState>,
    app: AppHandle,
    binding: JsBinding,
) -> Result<(), String> {
    let parsed: Binding = binding
        .try_into()
        .map_err(|e: BindingConversionError| e.to_string())?;
    state
        .settings
        .set_log_capture_binding(parsed.clone())
        .map_err(|e| e.to_string())?;
    {
        let mut guard = state
            .log_capture_binding
            .write()
            .map_err(|_| "binding lock poisoned".to_string())?;
        *guard = parsed;
    }
    sync_hotkeys(&state, &app);
    Ok(())
}

#[tauri::command]
pub fn get_action_binding(state: State<'_, AppState>) -> JsBinding {
    let b = state
        .action_binding
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| crate::settings::default_action_binding());
    b.into()
}

#[tauri::command]
pub fn update_action_binding(
    state: State<'_, AppState>,
    app: AppHandle,
    binding: JsBinding,
) -> Result<(), String> {
    let parsed: Binding = binding
        .try_into()
        .map_err(|e: BindingConversionError| e.to_string())?;
    state
        .settings
        .set_action_binding(parsed.clone())
        .map_err(|e| e.to_string())?;
    {
        let mut guard = state
            .action_binding
            .write()
            .map_err(|_| "action_binding lock poisoned".to_string())?;
        *guard = parsed;
    }
    sync_hotkeys(&state, &app);
    Ok(())
}

#[tauri::command]
pub fn get_edit_selection_binding(state: State<'_, AppState>) -> JsBinding {
    let b = state
        .edit_selection_binding
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| crate::settings::default_edit_selection_binding());
    b.into()
}

#[tauri::command]
pub fn update_edit_selection_binding(
    state: State<'_, AppState>,
    app: AppHandle,
    binding: JsBinding,
) -> Result<(), String> {
    let parsed: Binding = binding
        .try_into()
        .map_err(|e: BindingConversionError| e.to_string())?;
    state
        .settings
        .set_edit_selection_binding(parsed.clone())
        .map_err(|e| e.to_string())?;
    {
        let mut guard = state
            .edit_selection_binding
            .write()
            .map_err(|_| "edit_selection_binding lock poisoned".to_string())?;
        *guard = parsed;
    }
    sync_hotkeys(&state, &app);
    Ok(())
}

#[tauri::command]
pub fn get_trigger_word_routing_enabled(state: State<'_, AppState>) -> bool {
    state.settings.trigger_word_routing_enabled()
}

#[tauri::command]
pub fn set_trigger_word_routing_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    state
        .settings
        .set_trigger_word_routing_enabled(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_action_trigger_word(state: State<'_, AppState>) -> String {
    state.settings.action_trigger_word()
}

#[tauri::command]
pub fn set_action_trigger_word(state: State<'_, AppState>, word: String) -> Result<(), String> {
    state
        .settings
        .set_action_trigger_word(&word)
        .map_err(|e| e.to_string())
}

/// Suspend or resume the CGEventTap listeners. Set to `true` while the
/// settings UI is in hotkey-capture mode so the web view can receive raw key
/// events without them being swallowed by the tap.
#[tauri::command]
pub fn set_rebinding(state: State<'_, AppState>, active: bool) {
    state.rebinding.store(active, Ordering::SeqCst);
}

#[tauri::command]
pub async fn confirm_log_capture(
    state: State<'_, AppState>,
    content: String,
    kind: String,
    project_id: Option<String>,
    new_project_name: Option<String>,
    tags: Vec<String>,
    deadline_iso: Option<String>,
) -> Result<String, String> {
    let kind_parsed = ItemKind::parse(&kind).ok_or_else(|| format!("invalid kind: {kind}"))?;
    let tx_clone = {
        let slot = state
            .coord_tx
            .lock()
            .map_err(|_| "coord_tx lock poisoned".to_string())?;
        slot.clone()
            .ok_or_else(|| "pipeline not started".to_string())?
    };
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    tx_clone
        .send(CoordinatorMsg::ConfirmLogCapture {
            content,
            kind: kind_parsed,
            project_id,
            new_project_name,
            tags,
            deadline_iso,
            reply: reply_tx,
        })
        .map_err(|e| e.to_string())?;
    match reply_rx.recv().await {
        Some(res) => res,
        None => Err("coordinator dropped reply channel".into()),
    }
}

#[tauri::command]
pub fn cancel_log_capture(state: State<'_, AppState>) -> Result<(), String> {
    let slot = state
        .coord_tx
        .lock()
        .map_err(|_| "coord_tx lock poisoned".to_string())?;
    let tx = slot
        .as_ref()
        .ok_or_else(|| "pipeline not started".to_string())?;
    tx.send(CoordinatorMsg::CancelLogCapture)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn start_pipeline(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    if !state.asr.ready() {
        return Err("speech model not ready".to_string());
    }
    ensure_pipeline_started(&state, &app);
    Ok(())
}

// ----- Speech model commands -----

#[derive(Debug, Clone, Serialize)]
pub struct SpeechModelStatus {
    pub id: String,
    pub display_name: String,
    pub version_label: String,
    pub description: String,
    pub language_label: String,
    pub english_only: bool,
    pub accuracy_bars: u8,
    pub speed_bars: u8,
    pub size_label: String,
    pub size_bytes: u64,
    pub downloaded: bool,
    pub active: bool,
    pub supported: bool,
}

fn model_status_for(entry: &ModelEntry, active_id: Option<&str>) -> SpeechModelStatus {
    SpeechModelStatus {
        id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        version_label: entry.version_label.clone(),
        description: entry.description.clone(),
        language_label: entry.language_label.clone(),
        english_only: entry.english_only,
        accuracy_bars: entry.accuracy_bars,
        speed_bars: entry.speed_bars,
        size_label: entry.size_label.clone(),
        size_bytes: entry.size_bytes,
        downloaded: downloader::is_downloaded(entry),
        active: active_id.map(|a| a == entry.id).unwrap_or(false),
        supported: registry::is_supported(entry),
    }
}

#[tauri::command]
pub fn list_speech_models(state: State<'_, AppState>) -> Vec<SpeechModelStatus> {
    let active = state.asr.active_model_id();
    registry::registry()
        .iter()
        .map(|m| model_status_for(m, active.as_deref()))
        .collect()
}

#[tauri::command]
pub fn get_active_speech_model_id(state: State<'_, AppState>) -> String {
    state
        .asr
        .active_model_id()
        .unwrap_or_else(|| state.settings.speech_model_id().unwrap_or_default())
}

#[tauri::command]
pub fn set_active_speech_model(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let entry = registry::lookup(&id)
        .ok_or_else(|| format!("unknown speech model id: {id}"))?
        .clone();
    state
        .settings
        .set_speech_model_id(&entry.id)
        .map_err(|e| e.to_string())?;
    state.asr.set_active_model(entry);
    Ok(())
}

#[tauri::command]
pub async fn download_speech_model(
    state: State<'_, AppState>,
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    let entry = registry::lookup(&id)
        .ok_or_else(|| format!("unknown speech model id: {id}"))?
        .clone();
    let target = downloader::model_dir(&entry);
    let app_for_progress = app.clone();
    let res = downloader::download_model(&entry, &target, move |p: DownloadProgress| {
        let _ = app_for_progress.emit("speech_model:progress", &p);
    })
    .await;
    match res {
        Ok(_) => {
            // If this is the currently-active model id (or no model is active),
            // refresh the engine's active-model entry so it picks up the new
            // on-disk state without a restart.
            if state.asr.active_model_id().as_deref() == Some(entry.id.as_str()) {
                state.asr.set_active_model(entry);
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn delete_speech_model(id: String) -> Result<(), String> {
    let entry = registry::lookup(&id).ok_or_else(|| format!("unknown speech model id: {id}"))?;
    let dir = downloader::model_dir(entry);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn is_pipeline_running(state: State<'_, AppState>) -> bool {
    state.hotkey_started.load(Ordering::SeqCst)
}

/// Idempotently start the rdev listener + coordinator pipeline.
///
/// On first call, creates the hotkey channel, spawns the rdev listener with
/// the shared `Arc<RwLock<Binding>>`, and spawns the coordinator on its own
/// dedicated thread + LocalSet (the coordinator owns a `!Send` `cpal::Stream`).
///
/// On subsequent calls, returns early. Callers can also poll
/// [`is_pipeline_running`] / `state.hotkey_started`.
pub fn ensure_pipeline_started(state: &AppState, app: &AppHandle) {
    if state.hotkey_started.swap(true, Ordering::SeqCst) {
        // Already started.
        return;
    }

    info!("starting voice-at-cursor + log-capture pipelines");

    // The coordinator listens on a single multiplexed channel. Each hotkey
    // listener gets its own raw HotkeyEvent channel; we spawn small adapter
    // tasks that tag the events with the right Action and forward them.
    let (coord_tx, coord_rx) = mpsc::unbounded_channel::<CoordinatorMsg>();

    if let Ok(mut slot) = state.coord_tx.lock() {
        *slot = Some(coord_tx.clone());
    }

    let (vac_tx, mut vac_rx) = mpsc::unbounded_channel::<HotkeyEvent>();
    let (lc_tx, mut lc_rx) = mpsc::unbounded_channel::<HotkeyEvent>();
    let (ac_tx, mut ac_rx) = mpsc::unbounded_channel::<HotkeyEvent>();
    let (es_tx, mut es_rx) = mpsc::unbounded_channel::<HotkeyEvent>();

    spawn_listener(Arc::clone(&state.binding), vac_tx, Arc::clone(&state.rebinding));
    spawn_listener(Arc::clone(&state.log_capture_binding), lc_tx, Arc::clone(&state.rebinding));
    spawn_listener(Arc::clone(&state.action_binding), ac_tx, Arc::clone(&state.rebinding));
    spawn_listener(Arc::clone(&state.edit_selection_binding), es_tx, Arc::clone(&state.rebinding));

    // macOS drives the coordinator from the CGEventTap listeners above.
    // Non-macOS has no tap, so register the configured bindings as global
    // shortcuts. Must run after `coord_tx` is stored in state — the shortcut
    // handlers forward into that channel.
    sync_hotkeys(state, app);

    // Adapter tasks: tag + forward into the coordinator channel. We use
    // `tokio::spawn` rather than a dedicated thread because these are pure
    // async pumps with no `!Send` state.
    {
        let coord_tx = coord_tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = vac_rx.recv().await {
                if coord_tx
                    .send(CoordinatorMsg::Hotkey(Action::VoiceAtCursor, ev))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    {
        let coord_tx = coord_tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = lc_rx.recv().await {
                if coord_tx
                    .send(CoordinatorMsg::Hotkey(Action::LogCapture, ev))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    {
        let coord_tx = coord_tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = ac_rx.recv().await {
                if coord_tx
                    .send(CoordinatorMsg::Hotkey(Action::ActionCommand, ev))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    {
        let coord_tx = coord_tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = es_rx.recv().await {
                if coord_tx
                    .send(CoordinatorMsg::Hotkey(Action::EditSelection, ev))
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    let pipeline_state = Arc::clone(&state.pipeline_state);
    let tray_for_state = Arc::clone(&state.tray);
    let asr = Arc::clone(&state.asr);
    let llm = Arc::clone(&state.llm);
    let db = state.db.clone();
    let event_log_root = state.event_log_root.clone();
    let paused = Arc::clone(&state.paused_hotkeys);
    let app = app.clone();

    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                error!(?e, "failed to build coordinator runtime");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.spawn_local(async move {
            coordinator::spawn(
                coord_rx,
                pipeline_state,
                asr,
                llm,
                app,
                db,
                event_log_root,
                paused,
                move |new_state: TrayPipelineState| {
                    if let Ok(t) = tray_for_state.lock() {
                        t.set_state(new_state);
                    }
                },
            );
        });
        rt.block_on(local);
    });
}

/// Convenience wrapper used by `lib.rs::run`'s setup hook to look up the
/// managed `AppState` and start the pipeline if permissions allow.
pub fn ensure_pipeline_started_from_handle(app: &AppHandle) {
    let state: State<'_, AppState> = app.state();
    ensure_pipeline_started(&state, app);
}

// ----- Item store commands -----

const DEFAULT_ITEM_LIMIT: u32 = 50;
const MAX_ITEM_LIMIT: u32 = 500;

fn require_db(state: &AppState) -> Result<&Db, String> {
    state
        .db
        .as_ref()
        .ok_or_else(|| "database not available".to_string())
}

fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_ITEM_LIMIT).clamp(1, MAX_ITEM_LIMIT)
}

#[tauri::command]
pub fn list_items(
    state: State<'_, AppState>,
    project_id: Option<String>,
    kind: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<Item>, String> {
    let db = require_db(&state)?;
    let limit = clamp_limit(limit);
    let offset = offset.unwrap_or(0);
    db.with_conn(|c| {
        db::items::list_items(c, project_id.as_deref(), kind.as_deref(), limit, offset)
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_item(state: State<'_, AppState>, id: String) -> Result<Option<Item>, String> {
    let db = require_db(&state)?;
    db.with_conn(move |c| db::items::get_item(c, &id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn search_items(
    state: State<'_, AppState>,
    query: String,
    kind: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Item>, String> {
    let db = require_db(&state)?;
    let limit = clamp_limit(limit);
    db.with_conn(|c| db::search::search_items(c, &query, kind.as_deref(), limit))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_item(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::items::soft_delete_item(c, &id))
        .map_err(|e| e.to_string())?;
    let id_for_event = id.clone();
    let _ = db.with_conn(move |c| db::events::insert_event(c, &id_for_event, "deleted", None));
    Ok(())
}

#[tauri::command]
pub fn count_items(state: State<'_, AppState>) -> Result<u32, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::items::count_items(c))
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct ActivityExportOutcome {
    pub path: String,
    pub count: u32,
}

const EXPORT_FRIENDLY_ERR: &str = "Export failed. See Settings → Diagnostics → logs for details.";

/// Export all activity (transcriptions, notes, tasks, meetings) captured at or
/// after `since` (ISO-8601 UTC; `None` = all time) as one Markdown or CSV file
/// in the user's Downloads folder, then reveal it in Finder.
#[tauri::command]
pub fn export_activity(
    state: State<'_, AppState>,
    app: AppHandle,
    since: Option<String>,
    format: String,
    range_label: Option<String>,
) -> Result<ActivityExportOutcome, String> {
    use crate::export::activity::{render_csv, render_markdown, ActivityEntry};
    use std::collections::HashMap;

    let ext = match format.as_str() {
        "markdown" => "md",
        "csv" => "csv",
        other => {
            error!(target: "export", format = %other, "export_activity: unknown format");
            return Err(EXPORT_FRIENDLY_ERR.into());
        }
    };

    let db = require_db(&state)?;
    let items = db
        .with_conn(|c| db::items::list_items_since(c, since.as_deref()))
        .map_err(|e| {
            error!(target: "export", error = %e, "export_activity: item query failed");
            EXPORT_FRIENDLY_ERR.to_string()
        })?;

    // Resolve project names once per unique id.
    let mut project_names: HashMap<String, String> = HashMap::new();
    for item in &items {
        let Some(pid) = item.project_id.as_deref() else {
            continue;
        };
        if project_names.contains_key(pid) {
            continue;
        }
        let name = db
            .with_conn(|c| db::projects::get_project(c, pid))
            .ok()
            .flatten()
            .map(|p| p.name);
        if let Some(name) = name {
            project_names.insert(pid.to_string(), name);
        }
    }

    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        // Meeting record = source 'meeting' with no parsed kind (the kind
        // column stores 'meeting'); meeting-derived tasks keep kind = task.
        let meeting =
            if matches!(item.source, db::items::ItemSource::Meeting) && item.kind.is_none() {
                let id = item.id.clone();
                db.with_conn(move |c| db::meetings::get_meeting(c, &id))
                    .ok()
                    .flatten()
            } else {
                None
            };
        let project_name = item
            .project_id
            .as_deref()
            .and_then(|pid| project_names.get(pid).cloned());
        entries.push(ActivityEntry {
            item,
            project_name,
            meeting,
        });
    }

    let now = chrono_now_iso();
    let label = range_label.unwrap_or_else(|| "All time".to_string());
    let body = match ext {
        "csv" => render_csv(&entries),
        _ => render_markdown(&entries, &label, &now),
    };

    let dir = app.path().download_dir().map_err(|e| {
        error!(target: "export", error = %e, "export_activity: no Downloads dir");
        EXPORT_FRIENDLY_ERR.to_string()
    })?;
    // "2026-06-11T18:04:21Z" → "2026-06-11-1804"
    let stamp = format!(
        "{}-{}{}",
        now.get(0..10).unwrap_or("0000-00-00"),
        now.get(11..13).unwrap_or("00"),
        now.get(14..16).unwrap_or("00"),
    );
    let path = dir.join(format!("echo-scribe-activity-{stamp}.{ext}"));
    std::fs::write(&path, &body).map_err(|e| {
        error!(
            target: "export",
            path = %path.display(),
            error = %e,
            "export_activity: write failed"
        );
        EXPORT_FRIENDLY_ERR.to_string()
    })?;

    info!(
        target: "export",
        path = %path.display(),
        count = entries.len(),
        format = %format,
        since = since.as_deref().unwrap_or("(all time)"),
        "exported activity"
    );

    // Best-effort reveal in Finder; the export itself already succeeded.
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(&path)
        .spawn();

    Ok(ActivityExportOutcome {
        path: path.to_string_lossy().into_owned(),
        count: entries.len() as u32,
    })
}

// ----- Project commands -----

#[tauri::command]
pub fn list_projects(
    state: State<'_, AppState>,
    include_archived: bool,
) -> Result<Vec<Project>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::projects::list_projects(c, include_archived))
        .map_err(|e| e.to_string())
}

#[derive(Debug, Default, Deserialize)]
pub struct CreateProjectInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub routing_aliases: Option<Vec<String>>,
    #[serde(default)]
    pub routing_app_hints: Option<Vec<String>>,
    #[serde(default)]
    pub routing_url_hints: Option<Vec<String>>,
    #[serde(default)]
    pub routing_window_hints: Option<Vec<String>>,
    #[serde(default)]
    pub routing_positive_examples: Option<Vec<String>>,
    #[serde(default)]
    pub routing_negative_examples: Option<Vec<String>>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
}

fn normalize_keywords(kws: Vec<String>) -> Vec<String> {
    normalize_string_list(kws, true)
}

fn normalize_string_list(values: Vec<String>, lowercase: bool) -> Vec<String> {
    use std::collections::BTreeSet;
    values
        .into_iter()
        .map(|v| {
            let t = v.trim();
            if lowercase {
                t.to_lowercase()
            } else {
                t.to_string()
            }
        })
        .filter(|k| !k.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[tauri::command]
pub fn create_project(
    state: State<'_, AppState>,
    input: CreateProjectInput,
) -> Result<Project, String> {
    let trimmed = input.name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Project name cannot be empty.".into());
    }
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    let project = Project {
        id: ulid::Ulid::new().to_string(),
        name: trimmed,
        created_at: now.clone(),
        archived_at: None,
        description: input.description.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }),
        keywords: normalize_keywords(input.keywords.unwrap_or_default()),
        color: input.color.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }),
        emoji: input.emoji.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }),
        updated_at: Some(now),
        export_folder: None,
        routing_aliases: normalize_string_list(input.routing_aliases.unwrap_or_default(), true),
        routing_app_hints: normalize_string_list(
            input.routing_app_hints.unwrap_or_default(),
            false,
        ),
        routing_url_hints: normalize_string_list(input.routing_url_hints.unwrap_or_default(), true),
        routing_window_hints: normalize_string_list(
            input.routing_window_hints.unwrap_or_default(),
            true,
        ),
        routing_positive_examples: normalize_string_list(
            input.routing_positive_examples.unwrap_or_default(),
            false,
        ),
        routing_negative_examples: normalize_string_list(
            input.routing_negative_examples.unwrap_or_default(),
            false,
        ),
    };
    let p = project.clone();
    db.with_conn(move |c| db::projects::insert_project(c, &p))
        .map_err(|e| {
            error!(target: "projects", error = %e, name = %project.name, "create_project failed");
            "Couldn't create project. See Settings → Diagnostics → logs for details.".to_string()
        })?;
    info!(target: "projects", id = %project.id, name = %project.name, "created project");
    Ok(project)
}

#[derive(Debug, Deserialize)]
pub struct UpdateProjectInput {
    pub id: String,
    #[serde(flatten)]
    pub patch: ProjectPatch,
}

#[tauri::command]
pub fn update_project(
    state: State<'_, AppState>,
    input: UpdateProjectInput,
) -> Result<Project, String> {
    let db = require_db(&state)?;
    let now = chrono_now_iso();

    // Normalize incoming patch before persisting: trim strings, lowercase
    // + dedupe keywords. Validation: reject empty/whitespace name (UI also
    // guards, but defend the boundary).
    let mut patch = input.patch.clone();
    if let Some(n) = &patch.name {
        let trimmed = n.trim().to_string();
        if trimmed.is_empty() {
            return Err("Project name cannot be empty.".into());
        }
        patch.name = Some(trimmed);
    }
    if let Some(Some(desc)) = &patch.description {
        let trimmed = desc.trim().to_string();
        patch.description = if trimmed.is_empty() {
            Some(None)
        } else {
            Some(Some(trimmed))
        };
    }
    if let Some(Some(color)) = &patch.color {
        let trimmed = color.trim().to_string();
        patch.color = if trimmed.is_empty() {
            Some(None)
        } else {
            Some(Some(trimmed))
        };
    }
    if let Some(Some(emoji)) = &patch.emoji {
        let trimmed = emoji.trim().to_string();
        patch.emoji = if trimmed.is_empty() {
            Some(None)
        } else {
            Some(Some(trimmed))
        };
    }
    if let Some(kws) = patch.keywords {
        patch.keywords = Some(normalize_keywords(kws));
    }
    if let Some(v) = patch.routing_aliases {
        patch.routing_aliases = Some(normalize_string_list(v, true));
    }
    if let Some(v) = patch.routing_app_hints {
        patch.routing_app_hints = Some(normalize_string_list(v, false));
    }
    if let Some(v) = patch.routing_url_hints {
        patch.routing_url_hints = Some(normalize_string_list(v, true));
    }
    if let Some(v) = patch.routing_window_hints {
        patch.routing_window_hints = Some(normalize_string_list(v, true));
    }
    if let Some(v) = patch.routing_positive_examples {
        patch.routing_positive_examples = Some(normalize_string_list(v, false));
    }
    if let Some(v) = patch.routing_negative_examples {
        patch.routing_negative_examples = Some(normalize_string_list(v, false));
    }

    let id = input.id.clone();
    let id_for_log = id.clone();
    db.with_conn(move |c| db::projects::update_project(c, &id, &patch, &now))
        .map_err(|e| {
            error!(target: "projects", error = %e, id = %id_for_log, "update_project failed");
            "Couldn't update project. See Settings → Diagnostics → logs for details.".to_string()
        })?;

    let id2 = input.id.clone();
    let project = db
        .with_conn(move |c| db::projects::get_project(c, &id2))
        .map_err(|e| {
            error!(target: "projects", error = %e, "update_project: re-read failed");
            "Couldn't read updated project. See logs.".to_string()
        })?
        .ok_or_else(|| "Project not found after update.".to_string())?;

    info!(target: "projects", id = %project.id, name = %project.name, "updated project");
    Ok(project)
}

#[tauri::command]
pub fn rename_project(state: State<'_, AppState>, id: String, name: String) -> Result<(), String> {
    // Kept as a thin shim over update_project so existing callers keep
    // working. New UI should call update_project directly.
    let patch = ProjectPatch {
        name: Some(name),
        ..Default::default()
    };
    let _ = update_project(state, UpdateProjectInput { id, patch })?;
    Ok(())
}

#[tauri::command]
pub fn archive_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    let id_for_log = id.clone();
    db.with_conn(move |c| db::projects::archive_project(c, &id, &now))
        .map_err(|e| {
            error!(target: "projects", error = %e, id = %id_for_log, "archive_project failed");
            "Couldn't archive project. See Settings → Diagnostics → logs for details.".to_string()
        })?;
    info!(target: "projects", id = %id_for_log, "archived project");
    Ok(())
}

#[tauri::command]
pub fn unarchive_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    let id_for_log = id.clone();
    db.with_conn(move |c| db::projects::unarchive_project(c, &id))
        .map_err(|e| {
            error!(target: "projects", error = %e, id = %id_for_log, "unarchive_project failed");
            "Couldn't unarchive project. See Settings → Diagnostics → logs for details.".to_string()
        })?;
    info!(target: "projects", id = %id_for_log, "unarchived project");
    Ok(())
}

#[tauri::command]
pub fn delete_project(
    state: State<'_, AppState>,
    id: String,
    reassign_to: Option<String>,
) -> Result<(), String> {
    let db = require_db(&state)?;
    let id_for_log = id.clone();
    let reassign_for_log = reassign_to.clone();
    db.with_conn_mut(move |c| db::projects::delete_project(c, &id, reassign_to.as_deref()))
        .map_err(|e| {
            error!(
                target: "projects",
                error = %e,
                id = %id_for_log,
                reassign_to = ?reassign_for_log,
                "delete_project failed"
            );
            "Couldn't delete project. See Settings → Diagnostics → logs for details.".to_string()
        })?;
    info!(
        target: "projects",
        id = %id_for_log,
        reassign_to = ?reassign_for_log,
        "deleted project"
    );
    Ok(())
}

#[tauri::command]
pub fn count_items_for_project(state: State<'_, AppState>, id: String) -> Result<u32, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::projects::count_items_for_project(c, &id))
        .map_err(|e| e.to_string())
}

// ----- Task commands -----

#[tauri::command]
pub fn list_tasks(
    state: State<'_, AppState>,
    include_completed: bool,
    project_id: Option<String>,
) -> Result<Vec<TaskWithItem>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::tasks::list_tasks(c, include_completed, project_id.as_deref()))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn complete_task(state: State<'_, AppState>, item_id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    db.with_conn(|c| db::tasks::complete_task(c, &item_id, &now))
        .map_err(|e| e.to_string())?;
    let id_ev = item_id.clone();
    let _ = db.with_conn(move |c| db::events::insert_event(c, &id_ev, "completed", None));
    Ok(())
}

#[tauri::command]
pub fn uncomplete_task(state: State<'_, AppState>, item_id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::tasks::uncomplete_task(c, &item_id))
        .map_err(|e| e.to_string())?;
    let id_ev = item_id.clone();
    let _ = db.with_conn(move |c| db::events::insert_event(c, &id_ev, "uncompleted", None));
    Ok(())
}

#[tauri::command]
pub fn set_task_deadline(
    state: State<'_, AppState>,
    item_id: String,
    deadline_iso: Option<String>,
) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::tasks::set_deadline(c, &item_id, deadline_iso.as_deref()))
        .map_err(|e| e.to_string())
}

// ----- Item update / restore -----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateItemArgs {
    pub id: String,
    /// `Some(text)` → update content. `None` → leave alone.
    pub content: Option<String>,
    /// Outer `None` → leave alone. `Some(None)` → clear. `Some(Some(id))` → set.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub project_id: Option<Option<String>>,
    /// `Some("note")` | `Some("task")` | `Some("")` (clear). `None` → leave alone.
    pub kind: Option<String>,
    /// `Some(vec![...])` → replace tag set. `None` → leave alone.
    pub tags: Option<Vec<String>>,
}

fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    // serde_json: a missing key → field default (Option::None). A present-but-null
    // key → Some(None). A present value → Some(Some(value)).
    Option::<T>::deserialize(deserializer).map(Some)
}

#[tauri::command]
pub fn update_item(state: State<'_, AppState>, args: UpdateItemArgs) -> Result<Item, String> {
    let db = require_db(&state)?;
    let kind_arg: Option<Option<ItemKind>> = match args.kind.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(k) => Some(Some(
            ItemKind::parse(k).ok_or_else(|| format!("invalid kind: {k}"))?,
        )),
    };
    let id_for_db = args.id.clone();
    let content_owned = args.content.clone();
    let project_arg = args.project_id.clone();
    let tags_arg = args.tags.clone();

    db.with_conn(move |c| {
        let project_ref: Option<Option<&str>> = project_arg.as_ref().map(|inner| inner.as_deref());
        db::items::update_item(
            c,
            &id_for_db,
            content_owned.as_deref(),
            project_ref,
            kind_arg,
        )?;
        if let Some(tags) = &tags_arg {
            db::items::replace_tags(c, &id_for_db, tags)?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;

    // Record lifecycle events for the changes.
    if args.content.is_some() {
        let id_ev = args.id.clone();
        let _ = db.with_conn(move |c| db::events::insert_event(c, &id_ev, "content_edited", None));
    }
    if let Some(ref kind_val) = args.kind {
        let id_ev = args.id.clone();
        let detail = if kind_val.is_empty() {
            "kind cleared".to_string()
        } else {
            format!("kind set to {kind_val}")
        };
        let _ = db
            .with_conn(move |c| db::events::insert_event(c, &id_ev, "kind_changed", Some(&detail)));
    }
    if let Some(ref proj) = args.project_id {
        let id_ev = args.id.clone();
        let detail = match proj {
            Some(pid) => format!("assigned to project {pid}"),
            None => "removed from project".to_string(),
        };
        let _ = db.with_conn(move |c| {
            db::events::insert_event(c, &id_ev, "project_changed", Some(&detail))
        });
    }
    if args.tags.is_some() {
        let id_ev = args.id.clone();
        let _ = db.with_conn(move |c| db::events::insert_event(c, &id_ev, "tags_changed", None));
    }

    let id_for_get = args.id.clone();
    let item = db
        .with_conn(move |c| db::items::get_item(c, &id_for_get))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "item not found after update".to_string())?;

    // Re-export markdown if the item is routed to a project with an
    // export_folder. Stable filename → overwrite. `try_export_item` is
    // a noop for items that don't qualify (no folder, below threshold,
    // unsupported kind such as the meeting record itself).
    let threshold = state.settings.export_confidence_threshold();
    crate::export::try_export_item(&db, &item, threshold);

    Ok(item)
}

#[tauri::command]
pub fn restore_item(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::items::restore_item(c, &id))
        .map_err(|e| e.to_string())?;
    let id_for_event = id.clone();
    let _ = db.with_conn(move |c| db::events::insert_event(c, &id_for_event, "restored", None));
    Ok(())
}

/// Soft-delete an item that the auto-file flow created and emit `item:deleted`
/// so the frontend can drop its toast / activity feed row. Used by the "Undo"
/// button shown after a confident capture is auto-filed.
#[tauri::command]
pub fn undo_log_capture(
    item_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let db = require_db(&state)?;
    let id_for_db = item_id.clone();
    db.with_conn(move |c| db::items::soft_delete_item(c, &id_for_db))
        .map_err(|e| e.to_string())?;
    let _ = app.emit("item:deleted", item_id);
    Ok(())
}

// ----- Auto-file (confident captures) settings -----

#[tauri::command]
pub fn get_auto_file_enabled(state: State<'_, AppState>) -> bool {
    state.settings.auto_file_enabled()
}

#[tauri::command]
pub fn set_auto_file_enabled(enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    state
        .settings
        .set_auto_file_enabled(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_auto_file_threshold(state: State<'_, AppState>) -> f32 {
    state.settings.auto_file_threshold()
}

#[tauri::command]
pub fn set_auto_file_threshold(threshold: f32, state: State<'_, AppState>) -> Result<(), String> {
    state
        .settings
        .set_auto_file_threshold(threshold)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_export_confidence_threshold(state: State<'_, AppState>) -> f32 {
    state.settings.export_confidence_threshold()
}

#[tauri::command]
pub fn set_export_confidence_threshold(
    threshold: f32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state
        .settings
        .set_export_confidence_threshold(threshold)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectTaggerStatus {
    pub enabled: bool,
    pub pending: u32,
    pub deferred: u32,
    pub done: u32,
    pub failed: u32,
    pub llm_ready: bool,
    pub deterministic_batch_size: u32,
    pub interval_minutes: u64,
}

#[tauri::command]
pub fn get_project_auto_tagging_enabled(state: State<'_, AppState>) -> bool {
    state.settings.project_auto_tagging_enabled()
}

#[tauri::command]
pub fn set_project_auto_tagging_enabled(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state
        .settings
        .set_project_auto_tagging_enabled(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn project_tagger_status(state: State<'_, AppState>) -> Result<ProjectTaggerStatus, String> {
    let db = require_db(&state)?;
    let counts = db
        .with_conn(crate::db::project_tag_jobs::counts)
        .map_err(|e| e.to_string())?;
    Ok(ProjectTaggerStatus {
        enabled: state.settings.project_auto_tagging_enabled(),
        pending: counts.pending,
        deferred: counts.deferred,
        done: counts.done,
        failed: counts.failed,
        llm_ready: state.llm.ready(),
        deterministic_batch_size: state.settings.project_auto_tagging_batch_size(),
        interval_minutes: state.settings.project_auto_tagging_interval_minutes(),
    })
}

#[tauri::command]
pub fn project_tagger_backfill(
    state: State<'_, AppState>,
    source: Option<String>,
    limit: Option<u32>,
) -> Result<u32, String> {
    let db = require_db(&state)?;
    let source = match source.as_deref() {
        None | Some("voice_at_cursor") => Some(crate::db::items::ItemSource::VoiceAtCursor),
        Some("log_capture") => Some(crate::db::items::ItemSource::LogCapture),
        Some("meeting") => Some(crate::db::items::ItemSource::Meeting),
        Some(other) => return Err(format!("invalid source: {other}")),
    };
    let now = chrono_now_iso();
    db.with_conn(|c| {
        crate::db::project_tag_jobs::enqueue_backfill(c, source, limit.unwrap_or(500), &now)
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn run_project_tagger_deterministic_once(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<crate::project_tagger::ProjectTaggerRunSummary, String> {
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    db.with_conn(|c| {
        crate::project_tagger::run_deterministic_batch(
            c,
            limit.unwrap_or_else(|| state.settings.project_auto_tagging_batch_size()),
            &now,
        )
    })
    .map_err(|e| e.to_string())
}

/// Payload for `tagger:progress` events emitted during `run_project_tagger_all`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectTaggerProgress {
    pub processed: u32,
    pub total: u32,
    pub assigned: u32,
}

/// Manual "tag everything now": backfill every untagged item + recording into
/// the queue, then walk the whole queue once (router first, then local AI when
/// loaded). Emits `tagger:progress` after each job so the UI can show a live
/// counter.
#[tauri::command]
pub async fn run_project_tagger_all(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<crate::project_tagger::ProjectTaggerRunSummary, String> {
    let db = require_db(&state)?.clone();
    let now = chrono_now_iso();
    let dow = crate::classifier::dow_from_iso(&now).to_string();
    let llm_arc = Arc::clone(&state.llm);
    let llm: Option<&crate::llm::Llm> = if llm_arc.ready() {
        Some(llm_arc.as_ref())
    } else {
        None
    };
    let on_progress = |s: &crate::project_tagger::ProjectTaggerRunSummary, total: u32| {
        let _ = app.emit(
            "tagger:progress",
            ProjectTaggerProgress {
                processed: s.scanned,
                total,
                assigned: s.assigned,
            },
        );
    };
    crate::project_tagger::run_full_pass_db(&db, llm, &now, &dow, on_progress)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn run_project_tagger_llm_once(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<crate::project_tagger::ProjectTaggerRunSummary, String> {
    if !state.llm.ready() {
        return Err("No local AI model is ready for project tagging.".into());
    }
    let db = require_db(&state)?.clone();
    let llm = Arc::clone(&state.llm);
    let now = chrono_now_iso();
    let dow = crate::classifier::dow_from_iso(&now).to_string();
    let limit = limit.unwrap_or_else(|| state.settings.project_auto_tagging_batch_size());
    crate::project_tagger::run_llm_batch_db(&db, llm.as_ref(), limit, &now, &dow)
        .await
        .map_err(|e| e.to_string())
}

/// Open a native folder picker and return the chosen absolute path. Returns
/// `None` if the user cancels.
#[tauri::command]
pub async fn pick_export_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose export folder")
        .pick_folder(move |path| {
            let s = path.and_then(|p| p.as_path().map(|pp| pp.to_string_lossy().into_owned()));
            let _ = tx.send(s);
        });
    rx.await.map_err(|e| e.to_string())
}

/// Backfill: re-export every non-deleted item + meeting for a project to its
/// configured folder. No-op when the project has no folder set. Returns the
/// number of files written. Skips items below the confidence threshold the same
/// way the live hooks do; meetings always export when a folder is set.
#[tauri::command]
pub fn export_project_backfill(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<u32, String> {
    let db = require_db(&state)?;
    let settings = state.settings.clone();
    let threshold = settings.export_confidence_threshold();
    crate::export::backfill_project(&db, &project_id, threshold).map_err(|e| {
        tracing::error!(target: "export", error = %e, project = %project_id, "backfill failed");
        "Backfill failed. See Settings → Diagnostics → logs for details.".to_string()
    })
}

#[tauri::command]
pub fn list_tags_for_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<Vec<String>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::items::list_tags_for_item(c, &item_id))
        .map_err(|e| e.to_string())
}

// ----- Claude Code session transcript -----

/// List available Claude Code session summaries for this project.
/// Reads from `~/.claude/projects/<project-path>/.sessions/` if it exists.
#[tauri::command]
pub fn list_claude_sessions() -> Result<Vec<ClaudeSessionSummary>, String> {
    let sessions_dir = claude_sessions_dir().ok_or("Claude sessions directory not found")?;
    if !sessions_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut summaries = Vec::new();
    let entries = std::fs::read_dir(&sessions_dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            continue;
        }
        // Read first and last line for timestamp/preview.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            continue;
        }
        let message_count = lines.len();
        // Try to extract a preview from the first user message.
        let mut preview = String::new();
        let mut timestamp = String::new();
        for line in &lines {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if timestamp.is_empty() {
                    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                        timestamp = ts.to_string();
                    }
                }
                if preview.is_empty() {
                    if let Some(role) = v.get("role").and_then(|r| r.as_str()) {
                        if role == "human" || role == "user" {
                            if let Some(msg) = v.get("content") {
                                let text = if let Some(s) = msg.as_str() {
                                    s.to_string()
                                } else if let Some(arr) = msg.as_array() {
                                    arr.iter()
                                        .filter_map(|item| {
                                            if item.get("type").and_then(|t| t.as_str())
                                                == Some("text")
                                            {
                                                item.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(|s| s.to_string())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                } else {
                                    String::new()
                                };
                                preview = text.chars().take(100).collect();
                            }
                        }
                    }
                }
                if !preview.is_empty() && !timestamp.is_empty() {
                    break;
                }
            }
        }
        summaries.push(ClaudeSessionSummary {
            session_id,
            preview,
            message_count,
            timestamp,
        });
    }
    // Sort newest first by timestamp.
    summaries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(summaries)
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSessionSummary {
    pub session_id: String,
    pub preview: String,
    pub message_count: usize,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSessionMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

/// Load the transcript for a specific Claude Code session.
#[tauri::command]
pub fn load_claude_session(session_id: String) -> Result<Vec<ClaudeSessionMessage>, String> {
    let sessions_dir = claude_sessions_dir().ok_or("Claude sessions directory not found")?;
    let path = sessions_dir.join(format!("{session_id}.jsonl"));
    if !path.is_file() {
        return Err(format!("session file not found: {session_id}"));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut messages = Vec::new();
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let role = v
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown")
                .to_string();
            let timestamp = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            // Extract text content.
            let text = if let Some(content) = v.get("content") {
                if let Some(s) = content.as_str() {
                    s.to_string()
                } else if let Some(arr) = content.as_array() {
                    arr.iter()
                        .filter_map(|item| {
                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                item.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            if !text.is_empty() {
                messages.push(ClaudeSessionMessage {
                    role,
                    content: text,
                    timestamp,
                });
            }
        }
    }
    Ok(messages)
}

/// Locate the Claude Code sessions directory for this project.
fn claude_sessions_dir() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    // Claude Code stores sessions at ~/.claude/projects/<encoded-path>/.sessions/
    // The project path is the current working directory encoded with dashes.
    let cwd = std::env::current_dir().ok()?;
    let cwd_str = cwd.to_string_lossy();
    // Claude encodes the path by replacing '/' with '-' and prepending '-'.
    let encoded = cwd_str.replace('/', "-");
    let sessions = home
        .join(".claude")
        .join("projects")
        .join(&encoded)
        .join(".sessions");
    if sessions.is_dir() {
        return Some(sessions);
    }
    // Fallback: scan all project dirs for one that matches our working directory.
    let projects_dir = home.join(".claude").join("projects");
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let sessions_path = entry.path().join(".sessions");
            if sessions_path.is_dir() {
                return Some(sessions_path);
            }
        }
    }
    None
}

// ----- Reset onboarding -----

#[tauri::command]
pub async fn reset_onboarding_and_quit(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app.store("settings.json").map_err(|e| e.to_string())?;
    store.clear();
    store.save().map_err(|e| e.to_string())?;
    // Spawn a delayed exit so the command's reply has time to flush.
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        handle.exit(0);
    });
    Ok(())
}

const APP_BUNDLE_NAME: &str = "Echo Scribe.app";

fn containing_app_bundle(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    executable
        .ancestors()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(APP_BUNDLE_NAME))
        .map(std::path::Path::to_path_buf)
}

fn uninstall_data_paths(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    [
        "Library/Application Support/EchoScribe",
        "Library/Application Support/Echo Scribe",
        "Library/Application Support/com.echoscribe.app",
        "Library/Caches/com.echoscribe.app",
        "Library/Caches/echo-scribe",
        "Library/Preferences/com.echoscribe.app.plist",
        "Library/Preferences/EchoScribe.plist",
        "Library/Logs/EchoScribe",
        "Library/WebKit/com.echoscribe.app",
        "Library/WebKit/echo-scribe",
        "Library/HTTPStorages/com.echoscribe.app",
        "Library/HTTPStorages/EchoScribe",
        "Library/Saved Application State/com.echoscribe.app.savedState",
        "Library/LaunchAgents/Echo Scribe.plist",
        "EchoScribe",
    ]
    .into_iter()
    .map(|relative| home.join(relative))
    .collect()
}

#[cfg(target_os = "macos")]
fn applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn move_paths_to_trash(paths: &[std::path::PathBuf]) -> Result<(), String> {
    let existing = paths.iter().filter(|path| path.exists()).collect::<Vec<_>>();
    if existing.is_empty() {
        return Ok(());
    }

    let mut script = String::from(
        "with timeout of 3600 seconds\n\
         tell application \"Finder\"\n",
    );
    for path in existing {
        let escaped = applescript_string(&path.to_string_lossy());
        script.push_str(&format!("delete POSIX file \"{escaped}\"\n"));
    }
    script.push_str("end tell\nend timeout");

    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("could not start Finder uninstall: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "Finder could not move Echo Scribe to the Trash: {}",
        stderr.trim()
    ))
}

/// Move the running application bundle to the macOS Trash and quit. When
/// `delete_data` is false, every data location is intentionally left in
/// place so a later install resumes with the same database, models, media,
/// settings, and startup preference.
#[tauri::command]
pub async fn uninstall_application(app: AppHandle, delete_data: bool) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, delete_data);
        return Err("Uninstall from Settings is currently available on macOS only.".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        let executable = std::env::current_exe()
            .map_err(|e| format!("could not locate the running application: {e}"))?;
        let bundle = containing_app_bundle(&executable).ok_or_else(|| {
            "Uninstall is only available when Echo Scribe is running from its app bundle."
                .to_string()
        })?;

        let mut paths = Vec::new();
        if delete_data {
            let home =
                dirs::home_dir().ok_or_else(|| "could not locate your home folder".to_string())?;
            paths.extend(uninstall_data_paths(&home));
        }
        // Move the bundle last: if Finder rejects an earlier data location,
        // the app remains installed and can show the error.
        paths.push(bundle.clone());

        info!(
            bundle = %bundle.display(),
            delete_data,
            "uninstall requested"
        );
        move_paths_to_trash(&paths)?;

        if delete_data {
            if let Err(e) = crate::screenrec::drive::delete_refresh_token() {
                warn!(error = %e, "could not remove Drive credential during uninstall");
            }
            for service in TCC_RESET_SERVICES {
                match std::process::Command::new("/usr/bin/tccutil")
                    .args(["reset", service, "com.echoscribe.app"])
                    .output()
                {
                    Ok(output) if output.status.success() => {}
                    Ok(output) => warn!(
                        service,
                        status = %output.status,
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "could not reset permission during uninstall"
                    ),
                    Err(e) => warn!(
                        service,
                        error = %e,
                        "could not run permission reset during uninstall"
                    ),
                }
            }
        }

        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            handle.exit(0);
        });
        Ok(())
    }
}

// ----- Reset TCC permissions -----

/// Every TCC service the app ever requests — one per `NS*UsageDescription` in
/// `Info.plist`. Both `reset_tcc_and_quit` (the "reset permissions" button) and
/// `uninstall_application` (delete-data path) reset all of them: a service
/// missing here keeps its old grant/denial across the reset — which reads as a
/// broken feature (a silently-denied Camera survived the reset and wedged the
/// webcam until Camera was added) — and, on uninstall, leaves a stale entry in
/// System Settings → Privacy & Security after the app is gone.
///
/// Maps to Info.plist: Microphone←NSMicrophone, Accessibility←NSAccessibility,
/// ScreenCapture←NSScreenCapture, Camera←NSCamera, AppleEvents←NSAppleEvents
/// (Automation — the app drives Finder/System Events via osascript for
/// uninstall, mute-while-recording, and quit).
pub(crate) const TCC_RESET_SERVICES: [&str; 5] = [
    "Microphone",
    "Accessibility",
    "ScreenCapture",
    "Camera",
    "AppleEvents",
];

/// Run `tccutil reset` for every service in [`TCC_RESET_SERVICES`] against
/// this app's bundle id, then quit the app. macOS keeps TCC grants attached
/// to the running process, so the user must relaunch to be re-prompted.
///
/// Equivalent to the manual `tccutil reset <Service> com.echoscribe.app`
/// flow documented in CLAUDE.md, but exposed in the UI so the user doesn't
/// need a terminal.
#[tauri::command]
pub async fn reset_tcc_and_quit(app: AppHandle) -> Result<(), String> {
    use std::process::Command;
    const BUNDLE_ID: &str = "com.echoscribe.app";
    info!(bundle = BUNDLE_ID, "reset_tcc_and_quit invoked");
    for service in TCC_RESET_SERVICES {
        let output = Command::new("/usr/bin/tccutil")
            .args(["reset", service, BUNDLE_ID])
            .output()
            .map_err(|e| {
                error!(?e, service, "failed to spawn tccutil");
                format!("failed to run tccutil for {service}: {e}")
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        info!(
            service,
            status = %output.status,
            stdout = %stdout.trim(),
            stderr = %stderr.trim(),
            "tccutil reset result"
        );
        if !output.status.success() {
            return Err(format!(
                "tccutil reset {service} failed: {} (stderr: {})",
                output.status,
                stderr.trim()
            ));
        }
    }
    info!("tcc reset complete; scheduling app exit");
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.exit(0);
    });
    Ok(())
}

// ----- LLM model commands -----

#[derive(Debug, Clone, Serialize)]
pub struct LlmModelStatus {
    pub id: String,
    pub display_name: String,
    pub family: String,
    pub size_label: String,
    pub size_bytes: u64,
    pub context_length: u32,
    pub downloaded: bool,
    pub active: bool,
    pub supported: bool,
    /// Bytes currently on disk for this model (includes any `.partial`).
    pub disk_bytes: u64,
    /// Dir has bytes on disk but the model isn't fully downloaded — an
    /// interrupted/orphaned download the user can reclaim.
    pub incomplete: bool,
}

fn llm_status_for(entry: &LlmModelEntry, active_id: Option<&str>) -> LlmModelStatus {
    LlmModelStatus {
        id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        family: entry.family.clone(),
        size_label: entry.size_label.clone(),
        size_bytes: entry.size_bytes,
        context_length: entry.context_length,
        downloaded: llm::is_downloaded(entry),
        active: active_id.map(|a| a == entry.id).unwrap_or(false),
        supported: llm::registry::is_supported(entry),
        disk_bytes: llm::disk_bytes(entry),
        incomplete: llm::has_incomplete_download(entry),
    }
}

#[tauri::command]
pub fn list_llm_models(state: State<'_, AppState>) -> Vec<LlmModelStatus> {
    let active = state.llm.active_model_id();
    llm::registry::registry()
        .iter()
        .map(|m| llm_status_for(m, active.as_deref()))
        .collect()
}

#[tauri::command]
pub fn get_active_llm_model_id(state: State<'_, AppState>) -> String {
    state
        .llm
        .active_model_id()
        .unwrap_or_else(|| state.settings.llm_model_id().unwrap_or_default())
}

#[tauri::command]
pub fn set_active_llm_model(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let entry = llm::registry::lookup(&id)
        .ok_or_else(|| format!("unknown llm model id: {id}"))?
        .clone();
    state
        .settings
        .set_llm_model_id(&entry.id)
        .map_err(|e| e.to_string())?;
    state.llm.set_active_model(entry);
    Ok(())
}

#[tauri::command]
pub async fn download_llm_model(
    state: State<'_, AppState>,
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    let entry = llm::registry::lookup(&id)
        .ok_or_else(|| format!("unknown llm model id: {id}"))?
        .clone();
    let target = llm::model_dir(&entry);
    let app_for_progress = app.clone();
    let res = llm::downloader::download_model(&entry, &target, move |p: LlmDownloadProgress| {
        let _ = app_for_progress.emit("llm_model:progress", &p);
    })
    .await;
    match res {
        Ok(_) => {
            // Refresh active entry if this id is the active one, so the
            // engine picks up the new on-disk state on the next generate.
            if state.llm.active_model_id().as_deref() == Some(entry.id.as_str()) {
                state.llm.set_active_model(entry);
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn delete_llm_model(id: String) -> Result<(), String> {
    let entry = llm::registry::lookup(&id).ok_or_else(|| format!("unknown llm model id: {id}"))?;
    let dir = llm::model_dir(entry);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ----- Settings flags: audio feedback + onboarding completion -----

#[tauri::command]
pub fn get_audio_feedback_enabled(state: State<'_, AppState>) -> bool {
    state.settings.audio_feedback_enabled()
}

#[tauri::command]
pub fn set_audio_feedback_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .settings
        .set_audio_feedback_enabled(enabled)
        .map_err(|e| e.to_string())?;
    crate::audio::feedback::set_enabled(enabled);
    Ok(())
}

#[tauri::command]
pub fn get_mute_while_recording(state: State<'_, AppState>) -> bool {
    state.settings.mute_while_recording()
}

#[tauri::command]
pub fn set_mute_while_recording(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .settings
        .set_mute_while_recording(enabled)
        .map_err(|e| e.to_string())?;
    crate::audio::mute::set_enabled(enabled);
    Ok(())
}

#[tauri::command]
pub fn get_filler_removal_enabled(state: State<'_, AppState>) -> bool {
    state.settings.filler_removal_enabled()
}

#[tauri::command]
pub fn set_filler_removal_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .settings
        .set_filler_removal_enabled(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_filler_words(state: State<'_, AppState>) -> Vec<String> {
    state.settings.filler_words()
}

#[tauri::command]
pub fn set_filler_words(state: State<'_, AppState>, words: Vec<String>) -> Result<(), String> {
    state
        .settings
        .set_filler_words(words)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_custom_words(state: State<'_, AppState>) -> Vec<String> {
    state.settings.custom_words()
}

#[tauri::command]
pub fn set_custom_words(state: State<'_, AppState>, words: Vec<String>) -> Result<(), String> {
    state
        .settings
        .set_custom_words(words)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_default_filler_words() -> Vec<String> {
    crate::asr::postprocess::DEFAULT_FILLERS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[tauri::command]
pub fn get_onboarding_completed(state: State<'_, AppState>) -> bool {
    state.settings.onboarding_completed()
}

#[tauri::command]
pub fn set_onboarding_completed(state: State<'_, AppState>, completed: bool) -> Result<(), String> {
    state
        .settings
        .set_onboarding_completed(completed)
        .map_err(|e| e.to_string())
}

// ----- Tray-driven UI events -----

#[tauri::command]
pub fn show_main_window(app: AppHandle) -> Result<(), String> {
    crate::ui::dock::set_dock_visible(true);
    if let Some(w) = app.get_webview_window("main") {
        w.show().map_err(|e| e.to_string())?;
        let _ = w.unminimize();
        w.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ----- Diagnostics -----

#[tauri::command]
pub fn diagnostics_log_dir() -> String {
    crate::log_dir().to_string_lossy().to_string()
}

/// Reveal the log folder in Finder (macOS) — uses the `open(1)` binary so we
/// don't need the shell plugin's allow-list. On other platforms this is a
/// best-effort no-op that returns Ok.
#[tauri::command]
pub fn diagnostics_open_log_folder() -> Result<(), String> {
    let dir = crate::log_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(&dir)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("open exited with {status}"));
        }
    }
    Ok(())
}

/// Read the last `max_lines` lines of today's log file. Returns an empty
/// string if no log file exists yet.
#[tauri::command]
pub fn diagnostics_recent_log(max_lines: Option<usize>) -> Result<String, String> {
    let max = max_lines.unwrap_or(200).min(2000);
    let dir = crate::log_dir();
    let today = chrono_today_for_filename();
    let path = dir.join(format!("echo-scribe.log.{today}"));
    if !path.exists() {
        // Daily appender uses .{YYYY-MM-DD}; if today's hasn't rolled yet the
        // file may also exist as "echo-scribe.log". Try both.
        let alt = dir.join("echo-scribe.log");
        if !alt.exists() {
            return Ok(String::new());
        }
        return tail_file(&alt, max);
    }
    tail_file(&path, max)
}

/// Locale-independent UTC date for tracing-appender's file suffix.
fn chrono_today_for_filename() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // tracing-appender uses local time for daily rotation; we approximate by
    // formatting the UTC date — close enough for filename guessing, and we
    // also fall back to the un-suffixed file. Use the chrono-style format
    // manually to avoid pulling in a chrono dep.
    let days = now / 86_400;
    // Days since Unix epoch -> calendar date via the civil-from-days
    // algorithm (Howard Hinnant). Returns YYYY-MM-DD.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Applies the staged update by writing a helper shell script, launching it
/// detached, then exiting the process. The helper swaps the .app bundle while
/// the process is gone, strips quarantine, and relaunches the app.
#[tauri::command]
pub fn apply_update_and_restart() {
    crate::updater::launch_update_helper();
}

/// The installed app version (e.g. "0.4.1"), for display in Settings.
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// User-triggered "Check for Updates" — forces an immediate check (ignoring the
/// background throttle) and reports the outcome so the UI can show feedback.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> crate::updater::ManualCheckResult {
    crate::updater::check_now(&app).await
}

/// Persists the dismissed version so the update banner doesn't reappear for it.
#[tauri::command]
pub fn dismiss_update(state: State<'_, AppState>, version: String) {
    if let Err(e) = state.settings.set_dismissed_update_version(&version) {
        tracing::error!(error = %e, "failed to persist dismissed update version");
    }
}

fn tail_file(path: &std::path::Path, max_lines: usize) -> Result<String, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    Ok(lines[start..].join("\n"))
}

#[tauri::command]
pub async fn test_llm_inference(
    state: State<'_, AppState>,
    prompt: String,
) -> Result<String, String> {
    if !state.llm.ready() {
        return Err("llm not ready".to_string());
    }
    let req = GenerateRequest {
        system: Some("You are a concise assistant. Reply briefly.".into()),
        user: prompt,
        history: Vec::new(),
        max_tokens: 128,
        temperature: 0.7,
        stop_strings: Vec::new(),
        grammar_gbnf: None,
        n_ctx: None,
    };
    state.llm.generate(req).await.map_err(|e| e.to_string())
}

// ----- Chat session management -----

#[tauri::command]
pub fn create_chat_session(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> Result<ChatSession, String> {
    let db = require_db(&state)?;
    let id = ulid::Ulid::new().to_string();
    db.with_conn(|c| chat::insert_session(c, &id, "New Chat", project_id.as_deref()))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_chat_sessions(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> Result<Vec<ChatSession>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| chat::list_sessions(c, project_id.as_deref()))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_chat_messages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<ChatMessage>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| chat::load_messages(c, &session_id, 20))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_chat_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| chat::delete_session(c, &session_id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_chat_session(
    state: State<'_, AppState>,
    session_id: String,
    name: String,
) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| chat::rename_session(c, &session_id, &name))
        .map_err(|e| e.to_string())
}

// ----- Item events & session links -----

#[tauri::command]
pub fn list_item_events(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<Vec<db::events::ItemEvent>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::events::list_events_for_item(c, &item_id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_sessions_for_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<Vec<ChatSession>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| db::events::list_sessions_for_item(c, &item_id))
        .map_err(|e| e.to_string())
}

// ----- Memory chat -----

/// Extract FTS5-safe keywords from a natural-language message.
///
/// Keeps words of 4+ chars, strips non-alphanumeric chars, quotes each word
/// to prevent FTS5 syntax errors, and caps at 6 keywords joined with OR.
/// Returns an empty string when no usable keywords remain.
#[derive(serde::Serialize, Clone)]
pub struct ContextSource {
    pub date: String,
    pub kind: String,
    pub content: String,
}

#[derive(serde::Serialize)]
pub struct ChatReply {
    pub reply: String,
    pub sources: Vec<ContextSource>,
}

pub(crate) fn build_rag_query(message: &str) -> String {
    let keywords: Vec<String> = message
        .split_whitespace()
        .filter(|w| w.len() >= 4)
        .map(|w| {
            let clean: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
            clean
        })
        .filter(|w| w.len() >= 4)
        .map(|w| format!("\"{}\"", w))
        .take(6)
        .collect();
    keywords.join(" OR ")
}

#[tauri::command]
pub async fn chat_with_memory(
    state: State<'_, AppState>,
    session_id: String,
    message: String,
    project_id: Option<String>,
) -> Result<ChatReply, String> {
    if !state.llm.ready() {
        return Ok(ChatReply {
            reply: "No local AI model is loaded. Please download one in Settings → AI Model."
                .to_string(),
            sources: Vec::new(),
        });
    }

    let db = require_db(&state)?;

    // Persist the user message.
    db.with_conn(|c| chat::insert_message(c, &session_id, "user", &message))
        .map_err(|e| e.to_string())?;

    // Check if this is the first message (session still has placeholder name).
    let is_new = db
        .with_conn(|c| chat::list_sessions(c, None))
        .unwrap_or_default()
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.name == "New Chat")
        .unwrap_or(false);

    // Compute current time for temporal parsing and system prompt.
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now_iso = crate::db::items::chrono_now_iso();
    let today_str = &now_iso[..10];

    // FTS5 retrieval, then chunked re-ranking. FTS finds the right items; rag
    // selects the most relevant passages within them under a token budget.
    let terms = rag::query_terms(&message);
    let date_window = extract_date_window(&message, now_secs);

    // Load history first so retrieval can be budgeted against its token cost.
    let history_msgs = db
        .with_conn(|c| chat::load_messages(c, &session_id, (rag::HISTORY_TURNS + 1) as u32))
        .unwrap_or_default();
    let hist: Vec<(String, String)> = history_msgs
        .into_iter()
        .rev()
        .skip(1) // drop the just-inserted current user message
        .rev()
        .map(|m| (m.role, m.content))
        .collect();
    let history_tokens: usize = hist.iter().map(|(_, c)| rag::estimate_tokens(c)).sum();
    let chunk_budget = rag::CONTEXT_BUDGET_TOKENS
        .saturating_sub(history_tokens)
        .max(rag::MIN_CHUNK_BUDGET_TOKENS);

    let chunks: Vec<rag::Chunk> = {
        let rag_query = build_rag_query(&message);
        if rag_query.is_empty() {
            Vec::new()
        } else {
            let (from, to) = match &date_window {
                Some((f, t)) => (Some(f.as_str()), Some(t.as_str())),
                None => (None, None),
            };
            let raw_items = db
                .with_conn(|c| {
                    db::search::search_items_with_date_window(
                        c,
                        &rag_query,
                        from,
                        to,
                        project_id.as_deref(),
                        rag::FTS_ITEM_LIMIT,
                    )
                })
                .unwrap_or_default();

            let item_sources: Vec<rag::ChunkSource> = raw_items
                .iter()
                .map(|item| {
                    let kind = item
                        .kind
                        .as_ref()
                        .map(|k| k.as_str())
                        .unwrap_or("note")
                        .to_string();
                    let date = item.captured_at[..10.min(item.captured_at.len())].to_string();
                    rag::ChunkSource {
                        item_id: item.id.clone(),
                        date,
                        kind,
                        content: item.content.clone(),
                    }
                })
                .collect();

            rag::build_context_chunks(&item_sources, &terms, chunk_budget)
        }
    };

    // Record which items actually contributed context to this session.
    {
        let mut linked = std::collections::HashSet::new();
        for ch in &chunks {
            if linked.insert(ch.item_id.clone()) {
                let iid = ch.item_id.clone();
                let sid = session_id.clone();
                let _ = db.with_conn(move |c| db::events::link_item_to_session(c, &iid, &sid));
            }
        }
    }

    // One source row per distinct item, joining its selected chunks for display.
    let sources: Vec<ContextSource> = {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for ch in &chunks {
            if seen.insert(ch.item_id.clone()) {
                let joined: Vec<String> = chunks
                    .iter()
                    .filter(|c| c.item_id == ch.item_id)
                    .map(|c| c.content.clone())
                    .collect();
                out.push(ContextSource {
                    date: ch.date.clone(),
                    kind: ch.kind.clone(),
                    content: joined.join("\n…\n"),
                });
            }
        }
        out
    };

    let temporal_note = if date_window.is_some() && sources.is_empty() {
        " No captures were found for the requested time period — do not guess or invent content."
    } else {
        ""
    };

    let system = if sources.is_empty() {
        format!(
            "You are a helpful assistant built into Echo Scribe, a voice note and task capture app. \
             Today is {today_str}. \
             No relevant notes were found for this question.{temporal_note} \
             Do not invent or fabricate any captures or activities. \
             If the user is asking what they did or said, tell them no matching captures were found."
        )
    } else {
        let context_lines: Vec<String> = chunks
            .iter()
            .map(|c| format!("[{}] ({}): {}", c.date, c.kind, c.content))
            .collect();
        format!(
            "You are a helpful assistant built into Echo Scribe. \
             Today is {today_str}. \
             Here are the user's relevant notes and captures:\n\n---\n{}\n---\n\n\
             Answer based only on these notes. \
             Do not invent or add content beyond what is shown above. \
             If the notes don't address the question, say so explicitly.",
            context_lines.join("\n")
        )
    };

    let req = GenerateRequest {
        system: Some(system),
        user: message.clone(),
        history: hist,
        max_tokens: 512,
        temperature: 0.7,
        stop_strings: Vec::new(),
        grammar_gbnf: None,
        n_ctx: Some(rag::CHAT_N_CTX),
    };

    let reply = state.llm.generate(req).await.map_err(|e| e.to_string())?;

    // Persist the assistant reply and touch the session.
    db.with_conn(|c| chat::insert_message(c, &session_id, "assistant", &reply))
        .map_err(|e| e.to_string())?;
    db.with_conn(|c| chat::touch_session(c, &session_id))
        .map_err(|e| e.to_string())?;

    // Auto-rename on first message: truncate user message to 50 chars.
    if is_new {
        let auto_name: String = message.chars().take(50).collect();
        let _ = db.with_conn(|c| chat::rename_session(c, &session_id, &auto_name));
    }

    Ok(ChatReply { reply, sources })
}

#[tauri::command]
pub fn get_llm_unload_secs(state: State<'_, AppState>) -> u64 {
    state.settings.llm_unload_secs()
}

#[tauri::command]
pub fn set_llm_unload_secs(state: State<'_, AppState>, secs: u64) -> Result<(), String> {
    state
        .settings
        .set_llm_unload_secs(secs)
        .map_err(|e| e.to_string())?;
    state
        .llm
        .set_unload_timeout(std::time::Duration::from_secs(secs));
    Ok(())
}

#[tauri::command]
pub fn get_asr_unload_secs(state: State<'_, AppState>) -> u64 {
    state.settings.asr_unload_secs()
}

#[tauri::command]
pub fn set_asr_unload_secs(state: State<'_, AppState>, secs: u64) -> Result<(), String> {
    state
        .settings
        .set_asr_unload_secs(secs)
        .map_err(|e| e.to_string())?;
    state
        .asr
        .set_unload_timeout(std::time::Duration::from_secs(secs));
    Ok(())
}

// ----- Dashboard analytics -----

#[tauri::command]
pub fn get_dashboard_stats(
    state: State<'_, AppState>,
) -> Result<db::stats::DashboardStats, String> {
    let db = require_db(&state)?;
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Local UTC offset (east-positive seconds) so day boundaries use local
    // midnight, not UTC midnight. See db::stats::dashboard_stats.
    let tz_offset_secs = chrono::Local::now().offset().local_minus_utc() as i64;
    db.with_conn(|c| db::stats::dashboard_stats(c, now, tz_offset_secs))
        .map_err(|e| {
            error!(target: "stats", error = %e, "failed to load dashboard statistics");
            "Stats could not be loaded. See Settings → Diagnostics → logs for details.".to_string()
        })
}

// ============================================================================
// Meetings
// ============================================================================

#[tauri::command]
pub async fn start_meeting_manual(state: tauri::State<'_, AppState>) -> Result<String, String> {
    // Capture frontmost context so manual-start meetings also get the
    // window-title / URL / tab-title hint in the synthesis prompt.
    let start_context = capture_meeting_start_context();
    let id = state
        .meeting_manager
        .clone()
        .start(None, None, start_context)
        .await
        .map_err(|e| e.to_string())?;
    // Spawn end-monitor so the meeting auto-stops when mic goes silent.
    crate::meeting::detector::spawn_end_monitor(state.meeting_manager.clone(), None);
    Ok(id)
}

/// Snapshot the frontmost window/URL/tab to feed the meeting synthesis prompt.
/// Best-effort; returns an empty context when AX/AppleScript fails or the app
/// is non-macOS.
fn capture_meeting_start_context() -> crate::meeting::MeetingStartContext {
    let ctx = crate::input::focus::capture_context();
    crate::meeting::MeetingStartContext {
        window_title: ctx.as_ref().and_then(|c| c.window_title.clone()),
        browser_url: ctx.as_ref().and_then(|c| c.browser_url.clone()),
        browser_tab_title: ctx.as_ref().and_then(|c| c.browser_tab_title.clone()),
    }
}

/// Start a manually-triggered guided session: a normal meeting recording
/// with an immutable snapshot of the chosen guide template frozen onto the
/// meeting row. The live HUD/guidance loop is Plan B2 — this only attaches
/// the template so the session is reviewable later.
#[tauri::command]
pub async fn start_guided_session(
    state: tauri::State<'_, AppState>,
    template_id: String,
) -> Result<String, String> {
    let db = require_db(&state)?;
    let tid = template_id.clone();
    let template = db
        .with_conn(move |c| crate::db::guide_templates::get_template(c, &tid))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("guide template {template_id} not found"))?;

    let start_context = capture_meeting_start_context();

    let id = state
        .meeting_manager
        .clone()
        .start(None, None, start_context)
        .await
        .map_err(|e| e.to_string())?;
    state.meeting_manager.attach_guide(template).await?;

    crate::meeting::detector::spawn_end_monitor(state.meeting_manager.clone(), None);
    Ok(id)
}

#[tauri::command]
pub async fn attach_guide(
    state: tauri::State<'_, AppState>,
    template_id: String,
) -> Result<String, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    let tid = template_id.clone();
    let template = db
        .with_conn(move |c| crate::db::guide_templates::get_template(c, &tid))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("guide template {template_id} not found"))?;
    state.meeting_manager.attach_guide(template).await
}

#[tauri::command]
pub async fn detach_guide(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state.meeting_manager.detach_guide(&session_id).await
}

#[tauri::command]
pub async fn get_live_transcript(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<crate::meeting::Segment>, String> {
    Ok(state.meeting_manager.transcript_snapshot().await)
}

#[tauri::command]
pub async fn get_active_guides(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(state.meeting_manager.active_guides_snapshot().await)
}

#[tauri::command]
pub fn show_meeting_hud(app: tauri::AppHandle, focus: Option<String>) {
    crate::overlay::show_meeting_hud(&app, focus.as_deref());
}

/// Persist the HUD's logical frame so the next show restores the user's
/// size/position instead of snapping back to the default slot.
#[tauri::command]
pub fn save_hud_frame(
    state: tauri::State<'_, AppState>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    state
        .settings
        .set_guide_overlay_frame(serde_json::json!({ "x": x, "y": y, "w": w, "h": h }))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn guide_set_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    let m = crate::meeting::guidance::Mode::parse(&mode)
        .ok_or_else(|| format!("unknown guide mode: {mode}"))?;
    if let Some(engine) = state.meeting_manager.guide_engine_by_id(&session_id).await {
        engine.set_mode(m);
    }
    // Persist as the default for future sessions even if the engine is gone.
    state
        .settings
        .set_guide_overlay_mode(m)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn guide_trigger_now(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    match state.meeting_manager.guide_engine_by_id(&session_id).await {
        Some(engine) => {
            engine.fire_cycle();
            Ok(())
        }
        None => Err("no active guide session".into()),
    }
}

#[tauri::command]
pub async fn stop_meeting(state: tauri::State<'_, AppState>) -> Result<String, String> {
    state
        .meeting_manager
        .stop()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn is_meeting_active(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.meeting_manager.is_active().await)
}

#[tauri::command]
pub fn get_meeting(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Option<crate::db::meetings::MeetingRow>, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn(move |conn| crate::db::meetings::get_meeting(conn, &id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_meetings(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<crate::db::meetings::MeetingRow>, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn(|conn| crate::db::meetings::list_meetings(conn))
        .map_err(|e| e.to_string())
}

/// Action items promoted out of a meeting into their own item rows. Used by
/// the dashboard meeting card to nest a meeting's tasks under it instead of
/// scattering them through the feed.
#[tauri::command]
pub fn list_meeting_action_items(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Vec<crate::db::items::Item>, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn(move |conn| crate::db::meetings::list_action_items(conn, &id))
        .map_err(|e| {
            tracing::error!(target: "meetings", error = %e, "list_meeting_action_items failed");
            "Couldn't load this meeting's action items. See Settings → Diagnostics → logs for details.".to_string()
        })
}

#[tauri::command]
pub fn update_meeting_notes(
    state: tauri::State<'_, AppState>,
    id: String,
    notes: String,
) -> Result<(), String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn(move |conn| crate::db::meetings::update_user_notes(conn, &id, &notes))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_meeting(
    state: tauri::State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE items SET content = ?1 WHERE id = ?2 AND kind = 'meeting'",
            rusqlite::params![title, id],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn meeting_consent(
    state: tauri::State<'_, AppState>,
    bundle_id: String,
    app_name: String,
    decision: String, // "always" | "once" | "never"
) -> Result<Option<String>, String> {
    use crate::settings::MeetingAppPref;
    let mut prefs = state.settings.meeting_app_prefs();
    match decision.as_str() {
        "always" => {
            // Start the meeting first; only persist the `Always` pref if
            // the start succeeds, so a transient AsrNotReady doesn't leave
            // a sticky pref the user never confirmed worked.
            let bundle_for_monitor = bundle_id.clone();
            let id = state
                .meeting_manager
                .clone()
                .start(
                    Some(bundle_id.clone()),
                    Some(app_name),
                    capture_meeting_start_context(),
                )
                .await
                .map_err(|e| e.to_string())?;
            // Window-based end detection (the auto-start path spawns this; the
            // consent path must too, or these meetings rely solely on the
            // inactivity backstop / hard cap to stop).
            crate::meeting::detector::spawn_end_monitor(
                state.meeting_manager.clone(),
                Some(bundle_for_monitor),
            );
            prefs.insert(bundle_id, MeetingAppPref::Always);
            state
                .settings
                .set_meeting_app_prefs(&prefs)
                .map_err(|e| e.to_string())?;
            Ok(Some(id))
        }
        "once" => {
            let bundle_for_monitor = bundle_id.clone();
            let id = state
                .meeting_manager
                .clone()
                .start(
                    Some(bundle_id),
                    Some(app_name),
                    capture_meeting_start_context(),
                )
                .await
                .map_err(|e| e.to_string())?;
            crate::meeting::detector::spawn_end_monitor(
                state.meeting_manager.clone(),
                Some(bundle_for_monitor),
            );
            Ok(Some(id))
        }
        "never" => {
            prefs.insert(bundle_id, MeetingAppPref::Never);
            state
                .settings
                .set_meeting_app_prefs(&prefs)
                .map_err(|e| e.to_string())?;
            Ok(None)
        }
        _ => Err("invalid decision".into()),
    }
}

#[tauri::command]
pub async fn hide_consent_overlay(app_handle: tauri::AppHandle) -> Result<(), String> {
    crate::overlay::hide_consent_overlay(&app_handle);
    Ok(())
}

#[tauri::command]
pub async fn meeting_clear_app_pref(
    state: tauri::State<'_, AppState>,
    bundle_id: String,
) -> Result<(), String> {
    let mut prefs = state.settings.meeting_app_prefs();
    prefs.remove(&bundle_id);
    state
        .settings
        .set_meeting_app_prefs(&prefs)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_meeting_settings(state: tauri::State<'_, AppState>) -> serde_json::Value {
    serde_json::json!({
        "auto_detect": state.settings.meeting_auto_detect(),
        "app_prefs": state.settings.meeting_app_prefs(),
        "soft_warn_min": state.settings.meeting_soft_warn_min(),
        "hard_cap_min": state.settings.meeting_hard_cap_min(),
        "summary_prompt": state.settings.meeting_summary_prompt(),
    })
}

#[tauri::command]
pub fn set_meeting_summary_prompt(
    state: tauri::State<'_, AppState>,
    prompt: String,
) -> Result<(), String> {
    state
        .settings
        .set_meeting_summary_prompt(&prompt)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_meeting_auto_detect(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state
        .settings
        .set_meeting_auto_detect(on)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_meeting_app_pref(
    state: tauri::State<'_, AppState>,
    bundle_id: String,
    pref: crate::settings::MeetingAppPref,
) -> Result<(), String> {
    let mut prefs = state.settings.meeting_app_prefs();
    prefs.insert(bundle_id, pref);
    state
        .settings
        .set_meeting_app_prefs(&prefs)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn retry_meeting_summary(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    state
        .meeting_manager
        .retry_summary(&id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn retry_meeting_chunks(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    state
        .meeting_manager
        .retry_chunks(&id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_meeting(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    db.with_conn_mut(move |conn| {
        let tx = conn.transaction()?;
        crate::db::meetings::delete_meeting_and_item(&tx, &id)?;
        tx.commit()?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

// ----- Input device selection -----

#[tauri::command]
pub fn list_input_devices() -> Vec<crate::audio::devices::InputDevice> {
    crate::audio::devices::list_input_devices()
}

#[tauri::command]
pub fn get_preferred_input_device(state: State<'_, AppState>) -> Option<String> {
    state.settings.preferred_input_device()
}

/// Persist the user's preferred input device. Pass `null` to clear (use system default).
/// Selecting a device also bumps it to the top of the recent-devices MRU list.
#[tauri::command]
pub fn set_preferred_input_device(
    state: State<'_, AppState>,
    name: Option<String>,
) -> Result<(), String> {
    state
        .settings
        .set_preferred_input_device(name.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_recent_input_devices(state: State<'_, AppState>) -> Vec<String> {
    state.settings.recent_input_devices()
}

#[tauri::command]
pub fn get_input_device_sort(state: State<'_, AppState>) -> String {
    match state.settings.input_device_sort() {
        crate::settings::InputDeviceSort::LastUsed => "last_used".to_string(),
        crate::settings::InputDeviceSort::Alphabetical => "alphabetical".to_string(),
    }
}

#[tauri::command]
pub fn set_input_device_sort(state: State<'_, AppState>, sort: String) -> Result<(), String> {
    let parsed = match sort.as_str() {
        "last_used" => crate::settings::InputDeviceSort::LastUsed,
        "alphabetical" => crate::settings::InputDeviceSort::Alphabetical,
        other => return Err(format!("unknown sort '{other}'")),
    };
    state
        .settings
        .set_input_device_sort(parsed)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_app_launcher_enabled(state: State<'_, AppState>) -> bool {
    state.settings.app_launcher_enabled()
}

#[tauri::command]
pub fn set_app_launcher_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .settings
        .set_app_launcher_enabled(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_action_counter(state: State<'_, AppState>) -> u32 {
    state.settings.action_counter()
}

#[tauri::command]
pub fn reset_action_counter(state: State<'_, AppState>) -> Result<(), String> {
    state
        .settings
        .set_action_counter(0)
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct CommonActionTemplate {
    pub category: String,
    pub description: String,
    pub voice_phrases: Vec<String>,
}

#[tauri::command]
pub fn get_common_actions() -> Vec<CommonActionTemplate> {
    vec![
        CommonActionTemplate {
            category: "Applications".to_string(),
            description: "Launch standard macOS applications or workspace web apps".to_string(),
            voice_phrases: vec![
                "open Slack".to_string(),
                "launch Safari".to_string(),
                "open Growthinator".to_string(),
                "launch LiveCase".to_string(),
            ],
        },
        CommonActionTemplate {
            category: "Emails".to_string(),
            description: "Draft emails inside the system default client prefilled".to_string(),
            voice_phrases: vec![
                "email denis about Growthinator saying tests passed".to_string(),
                "email John about meeting saying I will be there".to_string(),
            ],
        },
        CommonActionTemplate {
            category: "Web Browsing".to_string(),
            description: "Navigate directly to websites in your default browser".to_string(),
            voice_phrases: vec!["open google".to_string(), "go to github.com".to_string()],
        },
        CommonActionTemplate {
            category: "Persistent Counter".to_string(),
            description: "Increment, query, or reset the app action stats".to_string(),
            voice_phrases: vec![
                "increment counter".to_string(),
                "what is the count".to_string(),
                "reset action count".to_string(),
            ],
        },
    ]
}

#[tauri::command]
pub fn get_format_templates(state: State<'_, AppState>) -> Vec<crate::settings::FormatTemplate> {
    state.settings.format_templates()
}

#[tauri::command]
pub fn set_format_templates(
    state: State<'_, AppState>,
    templates: Vec<crate::settings::FormatTemplate>,
) -> Result<(), String> {
    state
        .settings
        .set_format_templates(&templates)
        .map_err(|e| e.to_string())
}

/// Global editor defaults ("auto-remember"): an opaque JSON string owned by the
/// frontend (schema lives in `editorProject.ts`). `null` until first saved.
#[tauri::command]
pub fn get_editor_defaults(state: State<'_, AppState>) -> Option<String> {
    state.settings.editor_defaults()
}

#[tauri::command]
pub fn set_editor_defaults(state: State<'_, AppState>, json: String) -> Result<(), String> {
    state
        .settings
        .set_editor_defaults(&json)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------

// Daily recap commands
// ---------------------------------------------------------------------------

use crate::daily_summary::{generate_for_date, DEFAULT_LLM_MODEL_ID};
use crate::db::daily_summaries::{self as daily_summaries_db, DailySummaryRow, SummaryStatus};

#[derive(serde::Serialize)]
pub struct DailySummaryDto {
    pub date: String,
    pub generated_at: String,
    pub status: String,
    pub narrative: String,
    pub sections: serde_json::Value,
    pub source_meeting_ids: Vec<String>,
    pub source_item_ids: Vec<String>,
    pub model_version: String,
}

fn to_dto(row: DailySummaryRow) -> DailySummaryDto {
    DailySummaryDto {
        date: row.date,
        generated_at: row.generated_at,
        status: match row.status {
            SummaryStatus::Generated => "generated".into(),
            SummaryStatus::SkippedEmpty => "skipped_empty".into(),
            SummaryStatus::Failed => "failed".into(),
        },
        narrative: row.narrative,
        sections: serde_json::from_str(&row.sections_json)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        source_meeting_ids: serde_json::from_str(&row.source_meeting_ids_json).unwrap_or_default(),
        source_item_ids: serde_json::from_str(&row.source_item_ids_json).unwrap_or_default(),
        model_version: row.model_version,
    }
}

#[tauri::command]
pub fn daily_summary_get(
    state: State<'_, AppState>,
    date: String,
) -> Result<Option<DailySummaryDto>, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    let row = db
        .with_conn(|conn| daily_summaries_db::get(conn, &date).map_err(crate::db::DbError::from))
        .map_err(|e| format!("{e:?}"))?;
    Ok(row.map(to_dto))
}

#[tauri::command]
pub fn daily_summary_list_recent(
    state: State<'_, AppState>,
    limit: u32,
) -> Result<Vec<DailySummaryDto>, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?;
    let rows = db
        .with_conn(|conn| {
            daily_summaries_db::list_recent(conn, limit).map_err(crate::db::DbError::from)
        })
        .map_err(|e| format!("{e:?}"))?;
    Ok(rows.into_iter().map(to_dto).collect())
}

#[tauri::command]
pub async fn daily_summary_regenerate(
    state: State<'_, AppState>,
    date: String,
) -> Result<DailySummaryDto, String> {
    let db = state.db.as_ref().ok_or("db unavailable")?.clone();
    let llm = state.llm.clone();
    generate_for_date(&db, &llm, &date, DEFAULT_LLM_MODEL_ID)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let row = db
        .with_conn(|conn| daily_summaries_db::get(conn, &date).map_err(crate::db::DbError::from))
        .map_err(|e| format!("{e:?}"))?
        .ok_or_else(|| "row missing after generate".to_string())?;
    Ok(to_dto(row))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DailyRecapSettings {
    pub enabled: bool,
    pub deliver_hour: u8,
    pub include_weekends: bool,
}

#[tauri::command]
pub fn daily_recap_settings_get(state: State<'_, AppState>) -> DailyRecapSettings {
    DailyRecapSettings {
        enabled: state.settings.daily_recap_enabled(),
        deliver_hour: state.settings.daily_recap_deliver_hour(),
        include_weekends: state.settings.daily_recap_include_weekends(),
    }
}

#[tauri::command]
pub fn daily_recap_notification_permission_status(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_notification::{NotificationExt, PermissionState};
    let state = app
        .notification()
        .permission_state()
        .map_err(|e| e.to_string())?;
    Ok(matches!(state, PermissionState::Granted))
}

#[tauri::command]
pub fn daily_recap_settings_set(
    state: State<'_, AppState>,
    settings: DailyRecapSettings,
) -> Result<(), String> {
    state
        .settings
        .set_daily_recap_enabled(settings.enabled)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_daily_recap_deliver_hour(settings.deliver_hour)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_daily_recap_include_weekends(settings.include_weekends)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ============================================================================
// Guide templates
// ============================================================================

#[tauri::command]
pub fn list_guide_templates(
    state: State<'_, AppState>,
) -> Result<Vec<crate::db::guide_templates::GuideTemplate>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::guide_templates::list_templates(c))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_guide_template(
    state: State<'_, AppState>,
    name: String,
    description: String,
    goal: String,
    notes: String,
) -> Result<crate::db::guide_templates::GuideTemplate, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("template name cannot be empty".into());
    }
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    let t = crate::db::guide_templates::GuideTemplate {
        id: ulid::Ulid::new().to_string(),
        name: trimmed,
        description,
        goal,
        notes,
        created_at: now.clone(),
        updated_at: now,
    };
    let t2 = t.clone();
    db.with_conn(move |c| crate::db::guide_templates::insert_template(c, &t2))
        .map_err(|e| e.to_string())?;
    Ok(t)
}

#[tauri::command]
pub fn update_guide_template(
    state: State<'_, AppState>,
    id: String,
    name: String,
    description: String,
    goal: String,
    notes: String,
) -> Result<(), String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("template name cannot be empty".into());
    }
    let db = require_db(&state)?;
    let now = chrono_now_iso();
    db.with_conn(move |c| {
        crate::db::guide_templates::update_template(
            c,
            &id,
            &trimmed,
            &description,
            &goal,
            &notes,
            &now,
        )
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_guide_template(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(move |c| crate::db::guide_templates::delete_template(c, &id))
        .map_err(|e| e.to_string())
}

// ============================================================================
// Guide runs (post-meeting review)
// ============================================================================

#[tauri::command]
pub fn list_guide_runs(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<Vec<crate::db::meeting_guide_runs::GuideRunRow>, String> {
    let db = require_db(&state)?;
    db.with_conn(move |c| crate::db::meeting_guide_runs::list_guide_runs_for_meeting(c, &meeting_id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn guide_runs_for_template(
    state: State<'_, AppState>,
    template_id: String,
    limit: i64,
) -> Result<Vec<crate::db::meeting_guide_runs::GuideRunRow>, String> {
    let db = require_db(&state)?;
    db.with_conn(move |c| {
        crate::db::meeting_guide_runs::list_guide_runs_for_template(c, &template_id, limit)
    })
    .map_err(|e| e.to_string())
}

/// Envelope for pulling `segments` out of a meeting's stored `transcript_json`.
#[derive(serde::Deserialize)]
struct TranscriptEnvelope {
    #[serde(default)]
    segments: Vec<crate::meeting::Segment>,
}

/// Guide template as stored in the run row's `template_json` snapshot.
#[tauri::command]
pub async fn regenerate_guide_review(
    state: State<'_, AppState>,
    run_id: String,
) -> Result<(), String> {
    let db = require_db(&state)?.clone();
    let llm = state.llm.clone();

    // Load the run row → meeting_id + template snapshot.
    let rid = run_id.clone();
    let run = db
        .with_conn(move |c| crate::db::meeting_guide_runs::get_guide_run(c, &rid))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("guide run {run_id} not found"))?;
    let template: crate::db::guide_templates::GuideTemplate =
        serde_json::from_str(&run.template_json).map_err(|e| format!("bad template snapshot: {e}"))?;

    // Load the meeting transcript → segments.
    let mid = run.meeting_id.clone();
    let meeting = db
        .with_conn(move |c| crate::db::meetings::get_meeting(c, &mid))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "meeting not found".to_string())?;
    let transcript_json = meeting
        .transcript_json
        .ok_or_else(|| "meeting has no transcript".to_string())?;
    let env: TranscriptEnvelope =
        serde_json::from_str(&transcript_json).map_err(|e| format!("bad transcript json: {e}"))?;

    // Mark pending, regenerate, persist.
    let rid = run_id.clone();
    if let Err(e) = db.with_conn(move |c| {
        crate::db::meeting_guide_runs::set_guide_run_status(c, &rid, "pending", None)
    }) {
        tracing::warn!(target: "guide", ?e, %run_id, "guide run status write failed");
    }

    match crate::meeting::guide_review::generate_review(llm, &template, &env.segments).await {
        Ok(review) => {
            let rj = serde_json::to_string(&review).unwrap_or_else(|_| "{}".into());
            let gen_at = chrono::Utc::now().to_rfc3339();
            let rid = run_id.clone();
            db.with_conn(move |c| {
                crate::db::meeting_guide_runs::update_guide_run_review(
                    c, &rid, Some(rj.as_str()), "ready", Some(gen_at.as_str()),
                )
            })
            .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(e) => {
            let rid = run_id.clone();
            let err = e.clone();
            if let Err(write_err) = db.with_conn(move |c| {
                crate::db::meeting_guide_runs::set_guide_run_status(c, &rid, "failed", Some(err.as_str()))
            }) {
                tracing::warn!(target: "guide", e = ?write_err, %run_id, "guide run status write failed");
            }
            Err(e)
        }
    }
}

// ----- Screen recording commands -----

#[tauri::command]
pub async fn start_screen_recording(
    state: State<'_, AppState>,
    app: AppHandle,
    display_id: Option<u32>,
    window_id: Option<u32>,
    mic_device: Option<String>,
    sysaudio: bool,
    source_label: String,
    hide_cursor: Option<bool>,
    camera_uid: Option<String>,
    rect: Option<Vec<f64>>,
) -> Result<(), String> {
    // Validate the optional crop rect up front: `[x, y, w, h]` (global points).
    // A malformed rect is a caller bug, so fail with a friendly message before
    // spawning anything rather than silently ignoring it. `None` = full display.
    let rect = match rect {
        Some(v) => Some(crate::screenrec::rect_from_vec(&v).map_err(|e| {
            warn!(target: "screenrec", err = %e, "invalid crop rect");
            "Couldn't start the area recording: the selected region is invalid.".to_string()
        })?),
        None => None,
    };
    // Resolve the Camera TCC grant up front when a webcam was requested. If
    // the grant is missing, ask macOS now — this prompts on a fresh state and
    // returns the cached decision otherwise, so a setup flow that never
    // triggered the prompt (or a stale denial) can't silently produce a
    // webcam-less recording plus a dead self-view. On anything but Granted we
    // record WITHOUT the webcam and tell the user, instead of spawning a
    // sidecar camera session that can only fail.
    let mut camera_uid = camera_uid;
    if camera_uid.as_deref().is_some_and(|u| !u.is_empty())
        && !permissions::camera_authorized()
    {
        let outcome = permissions::request_camera().await;
        info!(target: "screenrec", ?outcome, "camera not authorized at record start; requested access");
        if outcome != CameraAccessOutcome::Granted {
            warn!(target: "screenrec", ?outcome, "recording without webcam: camera access not granted");
            let _ = app.emit(
                "screenrec-warning",
                serde_json::json!({
                    "message": "Camera access is off for Echo Scribe, so this recording won't include the webcam. Enable it in System Settings → Privacy & Security → Camera, then quit and reopen the app.",
                }),
            );
            camera_uid = None;
        }
    }
    let mut guard = state
        .active_recording
        .lock()
        .map_err(|_| "lock poisoned".to_string())?;
    if guard.is_some() {
        return Err("a recording is already in progress".into());
    }
    let dir = crate::screenrec::recordings_dir().map_err(|e| e.to_string())?;
    let id = format!(
        "rec-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis()
    );
    let out_path = dir.join(format!("{id}.mp4"));
    let hide_cursor = hide_cursor.unwrap_or(false);
    let params = crate::screenrec::RecordParams {
        display_id,
        window_id,
        mic_device: mic_device.clone(),
        sysaudio,
        hide_cursor,
        camera_uid: camera_uid.clone(),
        rect,
    };
    let handle = crate::screenrec::ScreenrecHandle::start(out_path, params)?;
    let meta = RecordingMeta {
        source_label,
        has_mic: mic_device.is_some(),
        has_sysaudio: sysaudio,
        cursor_hidden: hide_cursor,
    };
    *guard = Some((handle, meta));
    // Flip tray icon to red and update menu label.
    if let Ok(t) = state.tray.lock() {
        t.set_screenrec_active(true);
    }
    // Drop the active-recording lock before touching windows so overlay work
    // can never contend with a concurrent stop.
    drop(guard);
    // Show the floating camera self-view when a webcam is being recorded. The
    // sidecar records the camera by `uniqueID`; the preview mirrors it by name
    // (see `show_camera_preview`). Best-effort: a failed lookup or missing
    // camera just means no self-view — the recording is unaffected.
    if let Some(uid) = camera_uid.as_deref() {
        if !uid.is_empty() {
            let camera_name = crate::screenrec::list_cameras()
                .ok()
                .and_then(|c| c.cameras.into_iter().find(|c| c.uid == uid).map(|c| c.name))
                .unwrap_or_default();
            info!(
                target: "screenrec",
                uid_len = uid.len(),
                has_name = !camera_name.is_empty(),
                "showing camera self-view"
            );
            crate::overlay::show_camera_preview(&app, &camera_name);
        }
    }
    // Notify the frontend so RecordingsView refreshes.
    let _ = app.emit("screenrec-changed", ());
    Ok(())
}

#[tauri::command]
pub fn is_screen_recording(state: State<'_, AppState>) -> Result<bool, String> {
    let guard = state
        .active_recording
        .lock()
        .map_err(|_| "lock poisoned".to_string())?;
    Ok(guard.is_some())
}

/// Pause the in-progress screen recording (SIGUSR1 to the sidecar). The sidecar
/// gates video/audio/events and freezes the pause clock; nothing is captured
/// while paused. Idempotent sidecar-side, so a redundant call is harmless.
/// Emits `screenrec-changed` so the UI can reflect the paused indicator.
#[tauri::command]
pub fn pause_screen_recording(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    {
        let guard = state
            .active_recording
            .lock()
            .map_err(|_| "lock poisoned".to_string())?;
        let (handle, _) = guard.as_ref().ok_or("no recording in progress")?;
        handle.pause()?;
    }
    let _ = app.emit("screenrec-changed", ());
    Ok(())
}

/// Resume the paused screen recording (SIGUSR2 to the sidecar). Mirrors
/// [`pause_screen_recording`].
#[tauri::command]
pub fn resume_screen_recording(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    {
        let guard = state
            .active_recording
            .lock()
            .map_err(|_| "lock poisoned".to_string())?;
        let (handle, _) = guard.as_ref().ok_or("no recording in progress")?;
        handle.resume()?;
    }
    let _ = app.emit("screenrec-changed", ());
    Ok(())
}

/// Whether the in-progress recording is currently paused. `false` when no
/// recording is active. Reflects the sidecar's confirmed state (its
/// `paused`/`resumed` events), not merely which signal was last sent.
#[tauri::command]
pub fn is_screen_recording_paused(state: State<'_, AppState>) -> Result<bool, String> {
    let guard = state
        .active_recording
        .lock()
        .map_err(|_| "lock poisoned".to_string())?;
    Ok(guard.as_ref().map(|(h, _)| h.is_paused()).unwrap_or(false))
}

/// Non-command inner implementation so the tray can reuse stop logic without
/// going through a `#[tauri::command]` wrapper (which requires `State<'_>`).
pub fn stop_screen_recording_inner(
    state: &AppState,
    app: &AppHandle<Wry>,
) -> Result<crate::db::recordings::RecordingRow, String> {
    // Tear down the floating self-view first, unconditionally. Doing it up front
    // (before any fallible work below) guarantees the camera window is released
    // on every stop — clean or errored — so a failed `handle.stop()` / DB insert
    // can never strand an always-on-top preview on screen.
    crate::overlay::hide_camera_preview(app);
    let (handle, meta) = {
        let mut guard = state
            .active_recording
            .lock()
            .map_err(|_| "lock poisoned".to_string())?;
        guard.take().ok_or("no recording in progress")?
    };
    let info = handle.stop()?;
    info!(target: "screenrec", n_events = ?info.n_events, n_clicks = ?info.n_clicks, "recording stopped with input events");
    let id = std::path::Path::new(&info.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rec-unknown")
        .to_string();
    let row = crate::db::recordings::RecordingRow {
        id,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis() as i64,
        file_path: info.path.clone(),
        duration_ms: Some(info.dur_ms),
        width: Some(info.width),
        height: Some(info.height),
        size_bytes: Some(info.size),
        source_label: Some(meta.source_label),
        has_mic: meta.has_mic,
        has_sysaudio: meta.has_sysaudio,
        thumb_path: if info.thumb.is_empty() {
            None
        } else {
            Some(info.thumb)
        },
        drive_file_id: None,
        drive_link: None,
        upload_status: "none".into(),
        upload_error: None,
        exports: "[]".into(),
        title: None,
        transcript: None,
        denoised_path: None,
        events_path: info.events_path.clone(),
        project_json: None,
        webcam_path: info.webcam_path.clone(),
        cursor_hidden: meta.cursor_hidden,
        webcam_offset_ms: info.webcam_offset_ms,
        n_events: info.n_events,
        n_clicks: info.n_clicks,
        project_id: None,
        confidence: None,
        classified_by: None,
    };
    let db = state
        .db
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;
    db.with_conn(|c| crate::db::recordings::insert(c, &row))
        .map_err(|e| e.to_string())?;
    // New recordings join the project-tagging queue (classified once a title
    // or transcript exists).
    {
        let rec_id = row.id.clone();
        let now = chrono_now_iso();
        let _ = db.with_conn(move |c| {
            crate::db::project_tag_jobs::enqueue_recording(c, &rec_id, &now)
        });
    }
    // Flip tray icon back to idle.
    if let Ok(t) = state.tray.lock() {
        t.set_screenrec_active(false);
    }
    Ok(row)
}

#[tauri::command]
pub fn stop_screen_recording(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<crate::db::recordings::RecordingRow, String> {
    let row = stop_screen_recording_inner(&state, &app)?;
    // Notify the frontend so RecordingsView refreshes.
    let _ = app.emit("screenrec-changed", ());
    spawn_auto_denoise(app, row.id.clone());
    Ok(row)
}

/// Run denoise as a background task. Full error detail goes to the log;
/// the frontend gets a `denoise-failed` event with a friendly message.
pub(crate) fn spawn_auto_denoise(app: AppHandle, id: String) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_denoise(app.clone(), id.clone()).await {
            tracing::warn!(target: "denoise", recording_id = %id, %e, "auto-denoise after recording stop failed");
            let _ = app.emit(
                "denoise-failed",
                serde_json::json!({
                    "id": id,
                    "message": "Audio cleanup failed — the original recording is untouched. See Settings → Diagnostics → logs for details.",
                }),
            );
        }
    });
}

#[tauri::command]
pub fn list_recordings(
    state: State<'_, AppState>,
) -> Result<Vec<crate::db::recordings::RecordingRow>, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::recordings::list(c))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_recording(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    let row = db
        .with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?;
    if let Some(row) = row {
        match std::fs::remove_file(&row.file_path) {
            Ok(()) => {
                info!(target: "screenrec", recording_id = %id, path = %row.file_path, "deleted recording file")
            }
            Err(e) => {
                tracing::warn!(target: "screenrec", recording_id = %id, path = %row.file_path, %e, "failed to delete recording file")
            }
        }
        if let Some(thumb) = &row.thumb_path {
            match std::fs::remove_file(thumb) {
                Ok(()) => {
                    info!(target: "screenrec", recording_id = %id, path = %thumb, "deleted thumbnail file")
                }
                Err(e) => {
                    tracing::warn!(target: "screenrec", recording_id = %id, path = %thumb, %e, "failed to delete thumbnail file")
                }
            }
        }
        if let Some(events) = &row.events_path {
            match std::fs::remove_file(events) {
                Ok(()) => {
                    info!(target: "screenrec", recording_id = %id, path = %events, "deleted events file")
                }
                Err(e) => {
                    tracing::warn!(target: "screenrec", recording_id = %id, path = %events, %e, "failed to delete events file")
                }
            }
        }
        if let Some(webcam) = &row.webcam_path {
            match std::fs::remove_file(webcam) {
                Ok(()) => {
                    info!(target: "screenrec", recording_id = %id, path = %webcam, "deleted webcam file")
                }
                Err(e) => {
                    tracing::warn!(target: "screenrec", recording_id = %id, path = %webcam, %e, "failed to delete webcam file")
                }
            }
        }
        // Editor/export artifacts have no dedicated DB column — sweep
        // `<id>.bg.*` imports and `<id>.rendered.mp4` by name, plus transcode
        // outputs recorded in the exports JSON, or they leak on delete.
        match crate::screenrec::recordings_dir() {
            Ok(dir) => {
                let mut leftovers = editor_artifact_files(&dir, &id);
                for p in export_paths(&row.exports) {
                    let p = std::path::PathBuf::from(p);
                    // Only touch files we placed in the recordings dir.
                    if p.parent() == Some(dir.as_path()) && p.exists() && !leftovers.contains(&p)
                    {
                        leftovers.push(p);
                    }
                }
                for path in leftovers {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            info!(target: "screenrec", recording_id = %id, path = %path.display(), "deleted editor/export artifact")
                        }
                        Err(e) => {
                            tracing::warn!(target: "screenrec", recording_id = %id, path = %path.display(), %e, "failed to delete editor/export artifact")
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "screenrec", recording_id = %id, %e, "recordings dir unavailable; skipped editor artifact cleanup")
            }
        }
    }
    db.with_conn(|c| crate::db::recordings::delete(c, &id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_recording(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::recordings::rename(c, &id, title.trim()))
        .map_err(|e| e.to_string())
}

/// Transcribe a recording's audio on demand and cache the result in the DB.
/// Emits `transcribe-progress` events `{ id, pct }` while running.
#[tauri::command]
pub async fn transcribe_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    // Look up the mp4 path, dropping the DB borrow before any await.
    let mp4: std::path::PathBuf = {
        let db = require_db(&state)?;
        let row = db
            .with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "recording not found".to_string())?;
        std::path::PathBuf::from(row.file_path)
    };
    if !mp4.exists() {
        return Err("recording file is missing on disk".into());
    }

    // Require a downloaded ASR model up front for a clear message.
    if !state.asr.ready() {
        return Err("Download a transcription model first".into());
    }

    // Extract audio to a temp WAV in the recordings dir.
    let wav = crate::screenrec::recordings_dir()
        .map_err(|e| e.to_string())?
        .join(format!("{id}.transcribe.wav"));
    let mp4_for_blocking = mp4.clone();
    let wav_for_blocking = wav.clone();
    let extract = tokio::task::spawn_blocking(move || {
        crate::screenrec::extract_audio(&mp4_for_blocking, &wav_for_blocking)
    })
    .await
    .map_err(|_| "extraction task panicked".to_string())?;
    if let Err(e) = extract {
        let _ = std::fs::remove_file(&wav);
        if e == "no_audio" {
            return Err("Recording has no audio".into());
        }
        return Err(e);
    }

    // Load + transcribe in ~60s windows, emitting progress.
    let (samples, rate, channels) = match AsrPipeline::load_wav_16k_mono_int16(&wav) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            return Err(e.to_string());
        }
    };
    let asr = std::sync::Arc::clone(&state.asr);
    let app_for_progress = app.clone();
    let id_for_progress = id.clone();
    let text = asr
        .transcribe_long(samples, rate, channels, move |pct| {
            let _ = app_for_progress.emit(
                "transcribe-progress",
                serde_json::json!({ "id": id_for_progress, "pct": pct }),
            );
        })
        .await
        .map_err(|e| e.to_string());

    // Always clean up the temp WAV.
    let _ = std::fs::remove_file(&wav);
    let text = text?;

    // Persist (re-borrow DB after the awaits).
    {
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::set_transcript(c, &id, &text))
            .map_err(|e| e.to_string())?;
        // A transcript is new classification signal — reopen the recording's
        // tag job in case an earlier pass ran on the bare title.
        let rec_id = id.clone();
        let now = chrono_now_iso();
        let _ = db.with_conn(move |c| {
            crate::db::project_tag_jobs::reopen(c, &rec_id, &now)
        });
    }

    Ok(text)
}

/// Generate timed caption segments for a recording from the app's local ASR.
///
/// Extracts the recording's audio (reusing `transcribe_recording`'s WAV extract
/// path), transcribes it with Parakeet's native sentence-level timestamps, and
/// returns segments whose `start_ms`/`end_ms` are relative to the recording's
/// t=0 (same base as `<id>.events.jsonl`). Empty-text segments are dropped.
///
/// Segments are returned to the frontend, which stores them in the project JSON
/// via the existing project-save path — this command persists nothing itself.
/// Emits `captions-progress` events `{ id, ratio }` (ratio 0..1) so the UI can
/// show a bar. On failure returns the friendly string "Caption generation
/// failed — see logs." with full detail logged at `error!`.
#[tauri::command]
pub async fn generate_captions(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<crate::asr::captions::CaptionSegment>, String> {
    // Look up the mp4 path, dropping the DB borrow before any await.
    let mp4: std::path::PathBuf = {
        let db = require_db(&state)?;
        let row = db
            .with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| {
                error!(target: "captions", %id, error = %e, "db lookup failed");
                "Caption generation failed — see logs.".to_string()
            })?
            .ok_or_else(|| {
                warn!(target: "captions", %id, "recording not found");
                "Caption generation failed — see logs.".to_string()
            })?;
        std::path::PathBuf::from(row.file_path)
    };
    if !mp4.exists() {
        warn!(target: "captions", %id, path = %mp4.display(), "recording file missing on disk");
        return Err("Caption generation failed — see logs.".into());
    }

    // Require a downloaded ASR model up front for a clear message.
    if !state.asr.ready() {
        warn!(target: "captions", %id, "no downloaded ASR model");
        return Err("Download a transcription model first".into());
    }

    // Extract audio to a temp WAV in the recordings dir (same path as transcribe).
    let wav = match crate::screenrec::recordings_dir() {
        Ok(d) => d.join(format!("{id}.captions.wav")),
        Err(e) => {
            error!(target: "captions", %id, error = %e, "recordings_dir failed");
            return Err("Caption generation failed — see logs.".into());
        }
    };
    let mp4_for_blocking = mp4.clone();
    let wav_for_blocking = wav.clone();
    let extract = tokio::task::spawn_blocking(move || {
        crate::screenrec::extract_audio(&mp4_for_blocking, &wav_for_blocking)
    })
    .await
    .map_err(|_| {
        error!(target: "captions", %id, "audio extraction task panicked");
        "Caption generation failed — see logs.".to_string()
    })?;
    if let Err(e) = extract {
        let _ = std::fs::remove_file(&wav);
        if e == "no_audio" {
            warn!(target: "captions", %id, "recording has no audio");
            return Err("Recording has no audio".into());
        }
        error!(target: "captions", %id, error = %e, "audio extraction failed");
        return Err("Caption generation failed — see logs.".into());
    }

    // Load the WAV and generate captions in ~60s windows, emitting progress.
    let (samples, rate, channels) = match AsrPipeline::load_wav_16k_mono_int16(&wav) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            error!(target: "captions", %id, error = %e, "WAV load failed");
            return Err("Caption generation failed — see logs.".into());
        }
    };
    let asr = std::sync::Arc::clone(&state.asr);
    let app_for_progress = app.clone();
    let id_for_progress = id.clone();
    let segments = asr
        .transcribe_segments_long(samples, rate, channels, move |ratio| {
            let _ = app_for_progress.emit(
                "captions-progress",
                serde_json::json!({ "id": id_for_progress, "ratio": ratio }),
            );
        })
        .await;

    // Always clean up the temp WAV.
    let _ = std::fs::remove_file(&wav);

    let segments = match segments {
        Ok(s) => s,
        Err(e) => {
            error!(target: "captions", %id, error = %e, "caption generation failed");
            return Err("Caption generation failed — see logs.".into());
        }
    };

    let total_speech_ms = crate::asr::captions::total_speech_ms(&segments);
    info!(
        target: "captions",
        %id,
        segments = segments.len(),
        total_speech_ms,
        "caption generation complete"
    );
    Ok(segments)
}

/// Denoise auto-runs after `stop_screen_recording`; the cleaned file replaces
/// the original so there's no UI toggle to maintain. Kept as a const so a
/// future regression that needs to compare original vs. cleaned can flip it.
const DELETE_ORIGINAL_AFTER_DENOISE: bool = true;

/// Denoise a recording's audio and mux it into a new cleaned MP4.
/// Emits `denoise-progress` events `{ id, pct }` while running.
/// Used by the `denoise_recording` command and by the auto-denoise spawned
/// after `stop_screen_recording` (both command and tray paths).
pub(crate) async fn run_denoise(app: AppHandle, id: String) -> Result<(), String> {
    // Look up the original mp4 path; drop the DB borrow before any await.
    let orig: std::path::PathBuf = {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        let row = db
            .with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "recording not found".to_string())?;
        std::path::PathBuf::from(row.file_path)
    };
    if !orig.exists() {
        return Err("recording file is missing on disk".into());
    }

    let dir = crate::screenrec::recordings_dir().map_err(|e| e.to_string())?;
    let src_wav = dir.join(format!("{id}.dn-src.wav"));
    let out_wav = dir.join(format!("{id}.dn-out.wav"));
    let clean_mp4 = dir.join(format!("{id}.cleaned.mp4"));

    let cleanup = |extra: Option<&std::path::Path>| {
        let _ = std::fs::remove_file(&src_wav);
        let _ = std::fs::remove_file(&out_wav);
        if let Some(p) = extra {
            let _ = std::fs::remove_file(p);
        }
    };

    // 1. Extract 48kHz mono audio.
    let orig_c = orig.clone();
    let src_wav_c = src_wav.clone();
    let extract = tokio::task::spawn_blocking(move || {
        crate::screenrec::extract_audio_at(&orig_c, &src_wav_c, 48_000)
    })
    .await
    .map_err(|_| "extraction task panicked".to_string())?;
    if let Err(e) = extract {
        cleanup(None);
        if e == "no_audio" {
            return Err("Recording has no audio".into());
        }
        return Err(e);
    }

    // 2. Denoise (progress emitted per frame batch).
    let app_p = app.clone();
    let id_p = id.clone();
    let src_wav_c = src_wav.clone();
    let out_wav_c = out_wav.clone();
    let denoise = tokio::task::spawn_blocking(move || {
        crate::denoise::denoise_wav(&src_wav_c, &out_wav_c, move |pct| {
            let _ = app_p.emit(
                "denoise-progress",
                serde_json::json!({ "id": id_p, "pct": pct }),
            );
        })
    })
    .await
    .map_err(|_| "denoise task panicked".to_string())?;
    if let Err(e) = denoise {
        cleanup(None);
        return Err(e.to_string());
    }

    // 3. Mux cleaned audio + original video → new mp4.
    let orig_c = orig.clone();
    let out_wav_c = out_wav.clone();
    let clean_mp4_c = clean_mp4.clone();
    let mux = tokio::task::spawn_blocking(move || {
        crate::screenrec::mux_audio(&orig_c, &out_wav_c, &clean_mp4_c)
    })
    .await
    .map_err(|_| "mux task panicked".to_string())?;
    if let Err(e) = mux {
        cleanup(Some(&clean_mp4));
        return Err(e);
    }

    // 4. Verify output before any DB / destructive step.
    match std::fs::metadata(&clean_mp4) {
        Ok(m) if m.len() > 0 => {}
        _ => {
            cleanup(Some(&clean_mp4));
            return Err("denoise produced an empty file".into());
        }
    }

    let clean_str = clean_mp4.to_string_lossy().to_string();

    // 5. Record the cleaned path.
    {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::set_denoised_path(c, &id, Some(&clean_str)))
            .map_err(|e| e.to_string())?;
    }

    // 6. Optionally drop the original and promote the cleaned file.
    if DELETE_ORIGINAL_AFTER_DENOISE {
        let _ = std::fs::remove_file(&orig);
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::promote_denoised(c, &id, &clean_str))
            .map_err(|e| e.to_string())?;
    }

    cleanup(None); // remove temp wavs; keep clean_mp4
    // Denoise swapped the file on disk (original deleted, cleaned promoted). Tell
    // the UI so it re-fetches the row and reloads the player off the new path —
    // otherwise the <video> is left pointing at the now-deleted original and
    // shows a broken/"not playable" state. `denoise-progress` alone is not a
    // reliable completion signal (its final tick may not land exactly at 100).
    let _ = app.emit("screenrec-changed", ());
    Ok(())
}

/// Manual denoise trigger. Auto-denoise after stop covers the common case; this
/// stays available for re-cleaning if the user needs it.
#[tauri::command]
pub async fn denoise_recording(app: AppHandle, id: String) -> Result<(), String> {
    run_denoise(app, id).await
}

#[tauri::command]
pub fn reveal_recording(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = require_db(&state)?;
    let row = db
        .with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or("recording not found")?;
    std::process::Command::new("open")
        .arg("-R")
        .arg(&row.file_path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reveal a specific file inside the recordings folder in Finder. Unlike
/// `reveal_recording` (which reveals a recording's ORIGINAL file), this takes
/// an explicit path so the post-export UI can point Finder at the just-created
/// `<id>.rendered.mp4`. The containment check keeps this from becoming a
/// reveal-any-path primitive.
#[tauri::command]
pub fn reveal_recording_file(path: String) -> Result<(), String> {
    let dir = crate::screenrec::recordings_dir().map_err(|e| {
        error!(target: "screenrec", error = %e, "reveal_recording_file: recordings_dir failed");
        "Could not locate the recordings folder.".to_string()
    })?;
    let target = validate_reveal_path(&dir, &path).map_err(|e| {
        error!(target: "screenrec", path = %path, error = %e, "reveal_recording_file: rejected path");
        "Could not find that file in the recordings folder.".to_string()
    })?;
    std::process::Command::new("open")
        .arg("-R")
        .arg(&target)
        .spawn()
        .map_err(|e| {
            error!(target: "screenrec", path = %target.display(), error = %e, "reveal_recording_file: open -R failed");
            "Could not open Finder. See Settings → Diagnostics → logs for details.".to_string()
        })?;
    info!(target: "screenrec", path = %target.display(), "revealed recordings file in Finder");
    Ok(())
}

/// Copy a file inside the recordings folder to the system clipboard as a file
/// reference (so a paste in Finder/Mail/Slack/etc. pastes the FILE, not a text
/// path). Used by the "Copy" button next to the export reveal affordance.
///
/// Path validation mirrors `reveal_recording_file`: only files that
/// canonicalize inside the recordings dir are eligible — this keeps the
/// command from becoming a copy-any-file-to-clipboard primitive.
///
/// macOS-only (NSPasteboard via objc2/objc2-app-kit); `cfg`-gated so the
/// command still compiles (and returns a friendly "not supported" error) on
/// a future Windows build rather than failing to build the crate at all.
#[tauri::command]
pub fn copy_export_to_clipboard(path: String) -> Result<(), String> {
    let dir = crate::screenrec::recordings_dir().map_err(|e| {
        error!(target: "screenrec", error = %e, "copy_export_to_clipboard: recordings_dir failed");
        "Could not locate the recordings folder.".to_string()
    })?;
    let target = validate_reveal_path(&dir, &path).map_err(|e| {
        error!(target: "screenrec", path = %path, error = %e, "copy_export_to_clipboard: rejected path");
        "Could not find that file in the recordings folder.".to_string()
    })?;
    clipboard_imp::copy_file_to_clipboard(&target).map_err(|e| {
        error!(target: "screenrec", path = %target.display(), error = %e, "copy_export_to_clipboard: failed");
        "Could not copy the file to the clipboard. See Settings → Diagnostics → logs for details.".to_string()
    })?;
    info!(target: "screenrec", path = %target.display(), "copied export file to clipboard");
    Ok(())
}

/// NSPasteboard file-reference clipboard write. Deliberately objc2/objc2-app-kit
/// (NOT the cross-platform `arboard` crate already in Cargo.toml for other
/// clipboard uses) — `arboard` can only write text/image bytes, not a file
/// reference, and a file reference is what makes "paste" in Finder/Mail/Slack
/// paste the ACTUAL FILE rather than a text path string.
#[cfg(target_os = "macos")]
mod clipboard_imp {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    /// Write `path` to the general pasteboard as a file URL. Returns `Err` if
    /// the pasteboard refuses the write (e.g. another app holds an exclusive
    /// pasteboard lock) — the caller treats this as a friendly-error case, not
    /// a panic.
    ///
    /// This runs on a Tauri worker thread, not the main thread — `#[tauri::command]`
    /// handlers execute off the main thread by default. No `MainThreadMarker` is
    /// needed here (contrast `ui/dock.rs`, which requires one for
    /// `setActivationPolicy`): Apple documents `NSPasteboard`'s basic operations
    /// (`generalPasteboard`, `clearContents`, `writeObjects`) as thread-safe.
    pub fn copy_file_to_clipboard(path: &std::path::Path) -> Result<(), String> {
        let path_str = path.to_string_lossy();
        let ns_path = NSString::from_str(&path_str);
        let url: Retained<NSURL> = NSURL::fileURLWithPath(&ns_path);
        let writing: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(url);
        let objects = NSArray::from_retained_slice(&[writing]);

        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let ok = pasteboard.writeObjects(&objects);
        if !ok {
            return Err("NSPasteboard writeObjects returned false".to_string());
        }
        Ok(())
    }
}

/// Non-macOS fallback: the command compiles everywhere, but clipboard writes
/// aren't implemented on other platforms yet.
#[cfg(not(target_os = "macos"))]
mod clipboard_imp {
    pub fn copy_file_to_clipboard(_path: &std::path::Path) -> Result<(), String> {
        Err("Copy to clipboard is not supported on this platform yet.".to_string())
    }
}

/// Transcode a recording to `quality` ("1080"|"720"|"480"), store the output
/// next to the source as `<stem>-<quality>.mp4`, and merge it into the row's
/// `exports` JSON (replacing any prior export of the same quality). Returns the
/// updated row.
#[tauri::command]
pub fn export_recording(
    state: State<'_, AppState>,
    id: String,
    quality: String,
) -> Result<crate::db::recordings::RecordingRow, String> {
    let db = require_db(&state)?;
    let row = db
        .with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or("recording not found")?;
    let src = std::path::PathBuf::from(&row.file_path);
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rec")
        .to_string();
    let dir = crate::screenrec::recordings_dir().map_err(|e| e.to_string())?;
    let out = dir.join(format!("{stem}-{quality}.mp4"));

    let done = crate::screenrec::export(&src, &out, &quality)?;

    let mut exports: Vec<serde_json::Value> =
        serde_json::from_str(&row.exports).unwrap_or_default();
    exports.retain(|e| e.get("quality").and_then(|q| q.as_str()) != Some(quality.as_str()));
    exports.push(serde_json::json!({
        "quality": quality,
        "path": done.path,
        "size": done.size,
    }));
    let exports_json = serde_json::to_string(&exports).map_err(|e| e.to_string())?;
    db.with_conn(|c| crate::db::recordings::update_exports(c, &id, &exports_json))
        .map_err(|e| e.to_string())?;
    db.with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "recording vanished".to_string())
}

/// Return the raw recorded-input events JSONL text for a recording, so the
/// frontend render pipeline can build the auto-zoom timeline. Returns an `Err`
/// (never panics) when the recording has no events path, or the file is
/// missing/unreadable — the UI treats any error as "render without zoom".
#[tauri::command]
pub fn read_recording_events(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let db = require_db(&state)?;
    let row = db
        .with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or("recording not found")?;

    let events_path = match row.events_path {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::warn!(target: "screenrec", rec = %id, "no events_path recorded; rendering without zoom");
            return Err("no recorded input events for this recording".into());
        }
    };

    match std::fs::read_to_string(&events_path) {
        Ok(text) => {
            info!(target: "screenrec", rec = %id, path = %events_path, bytes = text.len(), "read recording events");
            Ok(text)
        }
        Err(e) => {
            tracing::warn!(target: "screenrec", rec = %id, path = %events_path, error = %e, "failed to read events file; rendering without zoom");
            Err(format!("could not read events file: {e}"))
        }
    }
}

/// Fetch a recording's opaque editor-project settings JSON (see
/// `src/lib/editorProject.ts`). `None` means editor defaults — Rust never
/// parses this field, it's TS-side owned.
#[tauri::command]
pub fn get_recording_project(state: State<'_, AppState>, id: String) -> Result<Option<String>, String> {
    let db = require_db(&state)?;
    let row = db
        .with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or("recording not found")?;
    info!(target: "screenrec", rec = %id, present = row.project_json.is_some(), "read recording project");
    Ok(row.project_json)
}

/// Persist a recording's editor-project settings JSON verbatim (opaque TEXT).
#[tauri::command]
pub fn set_recording_project(
    state: State<'_, AppState>,
    id: String,
    project_json: String,
) -> Result<(), String> {
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::recordings::set_project_json(c, &id, &project_json))
        .map_err(|e| e.to_string())?;
    info!(target: "screenrec", rec = %id, bytes = project_json.len(), "saved recording project");
    Ok(())
}

/// Allowed image extensions for a custom editor background (lower-cased, no dot).
const EDITOR_BG_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

/// Validate a source path for use as an editor background image and return its
/// lower-cased extension. Pure/testable: checks the extension allowlist only
/// (existence/file-type are checked at the command boundary with real IO).
fn editor_bg_extension(src_path: &str) -> Result<String, String> {
    let ext = std::path::Path::new(src_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .ok_or_else(|| "image has no file extension".to_string())?;
    if EDITOR_BG_EXTS.contains(&ext.as_str()) {
        Ok(ext)
    } else {
        Err(format!(
            "unsupported image type .{ext} (use PNG, JPG, JPEG, or WebP)"
        ))
    }
}

/// Existing `<id>.bg.<ext>` background imports for `id` in `dir`, sorted by
/// file name. Swept by directory scan (not a DB column) because backgrounds
/// are only referenced from the opaque editor-project JSON.
fn editor_bg_files(dir: &std::path::Path, id: &str) -> Vec<std::path::PathBuf> {
    let prefix = format!("{id}.bg.");
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Editor artifacts of `id` in `dir` that no DB column tracks: imported
/// backgrounds (`<id>.bg.*`) plus the editor exports (`<id>.rendered.mp4`,
/// `<id>.rendered.gif`). Listing only — callers decide whether to delete.
fn editor_artifact_files(dir: &std::path::Path, id: &str) -> Vec<std::path::PathBuf> {
    let mut out = editor_bg_files(dir, id);
    for ext in ["rendered.mp4", "rendered.gif"] {
        let rendered = dir.join(format!("{id}.{ext}"));
        if rendered.exists() {
            out.push(rendered);
        }
    }
    out
}

/// File paths recorded in a recording's `exports` JSON (transcode + rendered
/// outputs). Tolerates malformed JSON by returning empty — callers are cleanup
/// paths that must never fail the surrounding delete.
fn export_paths(exports_json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(exports_json)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(str::to_string))
        .collect()
}

/// Resolve `path` and require it to be an existing file inside `dir` (the
/// recordings folder). Canonicalizes both sides so `..` segments and the
/// macOS `/var` → `/private/var` symlink can't dodge the containment check.
fn validate_reveal_path(
    dir: &std::path::Path,
    path: &str,
) -> Result<std::path::PathBuf, String> {
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("recordings dir unavailable: {e}"))?;
    let p = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| format!("file not found: {e}"))?;
    if !p.starts_with(&dir) {
        return Err(format!(
            "path {} is outside the recordings folder",
            p.display()
        ));
    }
    Ok(p)
}

/// Copy a user-picked image into the recordings dir as `<id>.bg.<ext>` so it can
/// be referenced by an editor project's `background: {type:"image", path}`.
/// Validates the source exists, is a file, and has an allowed extension.
/// Returns the absolute destination path. Friendly errors; full detail logged.
#[tauri::command]
pub fn import_editor_background(
    state: State<'_, AppState>,
    id: String,
    src_path: String,
) -> Result<String, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or("recording not found")?;

    let ext = editor_bg_extension(&src_path).map_err(|e| {
        error!(target: "screenrec", rec = %id, path = %src_path, error = %e, "import_editor_background: bad extension");
        e
    })?;

    let src = std::path::Path::new(&src_path);
    let meta = std::fs::metadata(src).map_err(|e| {
        error!(target: "screenrec", rec = %id, path = %src_path, error = %e, "import_editor_background: source missing/unreadable");
        "Could not read the selected image. See Settings → Diagnostics → logs for details.".to_string()
    })?;
    if !meta.is_file() {
        error!(target: "screenrec", rec = %id, path = %src_path, "import_editor_background: source is not a file");
        return Err("The selected path is not a file.".to_string());
    }

    let dir = crate::screenrec::recordings_dir().map_err(|e| {
        error!(target: "screenrec", rec = %id, error = %e, "import_editor_background: recordings_dir failed");
        "Could not locate the recordings folder.".to_string()
    })?;
    let dest = dir.join(format!("{id}.bg.{ext}"));

    // A recording has at most one background: sweep any previously imported
    // `<id>.bg.*` with a different extension so switching formats (png → jpg)
    // doesn't orphan the old image. `dest` itself is left for fs::copy to
    // overwrite (and skipping it also means we never delete the source when
    // the user re-picks the currently imported file).
    for stale in editor_bg_files(&dir, &id) {
        if stale == dest {
            continue;
        }
        match std::fs::remove_file(&stale) {
            Ok(()) => {
                info!(target: "screenrec", rec = %id, path = %stale.display(), "removed stale editor background")
            }
            Err(e) => {
                tracing::warn!(target: "screenrec", rec = %id, path = %stale.display(), error = %e, "failed to remove stale editor background")
            }
        }
    }

    std::fs::copy(src, &dest).map_err(|e| {
        error!(target: "screenrec", rec = %id, from = %src_path, to = %dest.display(), error = %e, "import_editor_background: copy failed");
        "Could not import the background image. See Settings → Diagnostics → logs for details.".to_string()
    })?;

    let out = dest.to_string_lossy().to_string();
    info!(target: "screenrec", rec = %id, path = %out, bytes = meta.len(), "imported editor background");
    Ok(out)
}

/// Merge (replace-not-duplicate) a single `{"quality":<quality>,...}` entry into
/// a recording's exports JSON and return the refreshed row. Shared tail of the
/// editor export commands: `finalize_rendered_recording` passes `"rendered"`
/// (the MP4), `save_rendered_gif` passes `"rendered-gif"`. The two qualities are
/// distinct rows, so exporting a GIF never clobbers the MP4 export (and vice
/// versa) — each replaces only its own prior entry.
fn record_rendered_export(
    db: &crate::db::Db,
    id: &str,
    existing_exports: &str,
    quality: &str,
    out_path: &str,
    size: u64,
) -> Result<crate::db::recordings::RecordingRow, String> {
    let mut exports: Vec<serde_json::Value> =
        serde_json::from_str(existing_exports).unwrap_or_default();
    exports.retain(|e| e.get("quality").and_then(|q| q.as_str()) != Some(quality));
    exports.push(serde_json::json!({
        "quality": quality,
        "path": out_path,
        "size": size,
    }));
    let exports_json = serde_json::to_string(&exports).map_err(|e| e.to_string())?;
    db.with_conn(|c| crate::db::recordings::update_exports(c, id, &exports_json))
        .map_err(|e| {
            error!(target: "screenrec", rec = %id, error = %e, "update_exports failed after render");
            e.to_string()
        })?;
    db.with_conn(|c| crate::db::recordings::get(c, id))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "recording vanished".to_string())
}

/// `x-music` finalize header shape: mirrors `EditorProject.audio.music` on the
/// frontend (`{ path: string; volume: number }`). `path` is an absolute path
/// to the user-picked music file, wherever it lives on disk (unlike editor
/// backgrounds, music is NOT copied into the recordings dir — it's read
/// once via `extract_audio_at` at export time). `volume` is the 0..1 gain fed
/// to `mix_wav_samples` (the frontend already clamps it, but we clamp again
/// here defensively since this is a network-shaped boundary); any failure to
/// read/decode the file degrades to music-less audio + `warn!`, so an
/// unreadable/missing path is never a hard error.
#[derive(Debug, Clone, serde::Deserialize)]
struct MusicHeader {
    path: String,
    volume: f64,
}

/// Finalize an editor export: take the frontend's video-only WebCodecs render
/// (raw IPC body) and mux the recording's (trim-aligned) soundtrack back in,
/// writing `<id>.rendered.mp4`.
///
/// The bytes ride as the raw IPC body (`InvokeBody::Raw`) — least-copy transfer;
/// the recording id + optional trim window ride in headers:
///   - `x-recording-id`   (required) — DB-gates the operation
///   - `x-trim-start-ms`  (optional) — absent/empty = no trim (full audio)
///   - `x-trim-end-ms`    (optional) — absent/empty = no trim (full audio)
///   - `x-speed-ranges`   (optional) — JSON array of `{startMs,endMs,rate}` in
///     POST-TRIM ms time base (the frontend shifts them via `shiftRangesForTrim`
///     so they already line up with the trimmed WAV). Malformed / unparseable
///     → logged `warn!` and skipped (retiming is dropped, export never fails).
///   - `x-normalize-loudness` (optional) — `"true"`/`"1"` enables the loudness
///     normalization polish pass; anything else / absent = off. Any failure of
///     the pass degrades to un-normalized audio + `warn!` (never fails export).
///   - `x-music` (optional) — JSON `{"path":..., "volume":...}` for a
///     background-music track to mix under the voice audio. Defensive parse:
///     any failure (absent, not UTF-8, not JSON, wrong shape) → skipped +
///     `warn!` (never fails the export). Any failure of the MIX itself
///     (missing/unreadable file, decode error, rate mismatch) ALSO degrades
///     to music-less audio + `warn!` — music can never fail an export.
///
/// Flow (mirrors `run_denoise`'s staged temp-file orchestration; every failure
/// path cleans up ALL temps):
///   1. DB-gate the id; write the render bytes to a temp `<id>.render-vid.mp4`.
///   2. `extract_audio_at` (48kHz mono) from the row's playable file
///      (`denoised_path ?? file_path`).
///        - `no_audio` → skip mux entirely, promote the temp video to
///          `<id>.rendered.mp4` (never fail the export for missing audio).
///   3. If a trim window is present, `trim_wav_samples` the extracted WAV to the
///      kept `[start, end)` range so the audio lines up with the trimmed video.
///   4. If speed ranges are present, `retime_wav_samples` the (trimmed) WAV —
///      AFTER trim, since the ranges arrive in post-trim time. Ranges that
///      extend past the (possibly shorter-than-video) audio length are
///      clamped internally rather than rejected; a *remaining* `Err` (e.g.
///      genuinely unsorted/overlapping ranges) FAILS the export with a
///      friendly message instead of muxing un-retimed audio onto
///      already-retimed video, which would silently desync A/V.
///   4b. If `x-normalize-loudness` is set, loudness-normalize the (trimmed +
///       retimed) WAV — AFTER retime so the measured level is the muxed audio.
///       FAIL-SAFE: any error degrades to the un-normalized WAV + `warn!`
///       (unlike retime, skipping this causes no A/V desync).
///   4c. If `x-music` is present, `extract_audio_at` the music file (48kHz
///       mono, AVFoundation decodes mp3/m4a/wav/aac) then `mix_wav_samples`
///       it under the (trimmed + retimed + normalized) voice WAV — AFTER
///       normalize so the music volume is relative to the FINAL speech level
///       (set the music level once, speech is always consistent). FAIL-SAFE:
///       any failure (extract or mix) degrades to music-less audio + `warn!`.
///   5. `mux_audio(temp video, wav, <id>.rendered.mp4)`.
///   6. Merge the "rendered" exports entry (replace-not-duplicate).
#[tauri::command]
pub async fn finalize_rendered_recording(
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<crate::db::recordings::RecordingRow, String> {
    let id = request
        .headers()
        .get("x-recording-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            error!(target: "screenrec", "finalize_rendered_recording missing x-recording-id header");
            "internal error: missing recording id".to_string()
        })?;

    // Optional trim window. Absent/empty header → None (no trim).
    let parse_trim_header = |name: &str| -> Option<u64> {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<u64>().ok())
    };
    let trim = match (
        parse_trim_header("x-trim-start-ms"),
        parse_trim_header("x-trim-end-ms"),
    ) {
        (Some(s), Some(e)) if e > s => Some((s, e)),
        _ => None,
    };

    // Optional speed ranges (POST-TRIM ms). Defensive parse: any failure (header
    // absent, not UTF-8, not JSON, empty array) → None (no retiming). We DON'T
    // fail the export on a malformed header — retiming is a best-effort layer.
    let speed_ranges: Option<Vec<crate::screenrec::SpeedRangeSamples>> = request
        .headers()
        .get("x-speed-ranges")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| match serde_json::from_str::<Vec<crate::screenrec::SpeedRangeSamples>>(s) {
            Ok(ranges) => Some(ranges),
            Err(e) => {
                warn!(target: "screenrec", rec = %id, error = %e, "finalize: malformed x-speed-ranges header; skipping retiming");
                None
            }
        })
        .filter(|ranges| !ranges.is_empty());

    // Optional loudness normalization (Task 4). Defensive parse mirroring the
    // speed-ranges header: absent / not UTF-8 / anything other than the literal
    // "true" → false (feature off). We never fail the export on a malformed
    // header — normalization is a best-effort polish layer.
    let normalize_loudness: bool = request
        .headers()
        .get("x-normalize-loudness")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
        .unwrap_or(false);

    // Optional RNNoise denoise pass (same defensive parse as the normalize
    // header: absent / not UTF-8 / anything other than "true"/"1" → false).
    // Best-effort like normalization — a failure never fails the export.
    let denoise: bool = request
        .headers()
        .get("x-denoise")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
        .unwrap_or(false);

    // Optional background-music track (Task 7). Defensive parse mirroring
    // x-speed-ranges: absent / not UTF-8 / not JSON / wrong shape → None
    // (feature off). We never fail the export on a malformed header — music
    // is a best-effort polish layer exactly like loudness normalization.
    let music: Option<MusicHeader> = request
        .headers()
        .get("x-music")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| match serde_json::from_str::<MusicHeader>(s) {
            Ok(m) => Some(m),
            Err(e) => {
                warn!(target: "screenrec", rec = %id, error = %e, "finalize: malformed x-music header; skipping music");
                None
            }
        });

    // Copy the render bytes out of the borrowed request before any await.
    let bytes: Vec<u8> = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.to_vec(),
        tauri::ipc::InvokeBody::Json(_) => {
            error!(target: "screenrec", rec = %id, "finalize_rendered_recording received JSON body, expected raw bytes");
            return Err("internal error: rendered bytes not received".to_string());
        }
    };
    if bytes.is_empty() {
        error!(target: "screenrec", rec = %id, "finalize_rendered_recording received empty body");
        return Err("render produced no data".to_string());
    }

    // DB-gate: look up the row (playable file + existing exports); drop the DB
    // borrow before any await.
    let (playable, existing_exports): (std::path::PathBuf, String) = {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        let row = db
            .with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                error!(target: "screenrec", rec = %id, "finalize: recording not found");
                "recording not found".to_string()
            })?;
        let playable = row.denoised_path.clone().unwrap_or(row.file_path.clone());
        (std::path::PathBuf::from(playable), row.exports)
    };

    let dir = crate::screenrec::recordings_dir().map_err(|e| {
        error!(target: "screenrec", rec = %id, error = %e, "finalize: recordings_dir failed");
        "could not locate the recordings folder".to_string()
    })?;
    let temp_vid = dir.join(format!("{id}.render-vid.mp4"));
    let wav_full = dir.join(format!("{id}.render-audio.wav"));
    let wav_trim = dir.join(format!("{id}.render-audio-trim.wav"));
    let wav_speed = dir.join(format!("{id}.render-audio-speed.wav"));
    let wav_dn = dir.join(format!("{id}.render-audio-dn.wav"));
    let wav_norm = dir.join(format!("{id}.render-audio-norm.wav"));
    let wav_music = dir.join(format!("{id}.render-music.wav"));
    let wav_mixed = dir.join(format!("{id}.render-audio-mixed.wav"));
    let out = dir.join(format!("{id}.rendered.mp4"));

    // Cleanup helper: remove every temp we might have created. `out` is the
    // final product and is never cleaned here.
    let cleanup = || {
        let _ = std::fs::remove_file(&temp_vid);
        let _ = std::fs::remove_file(&wav_full);
        let _ = std::fs::remove_file(&wav_trim);
        let _ = std::fs::remove_file(&wav_speed);
        let _ = std::fs::remove_file(&wav_dn);
        let _ = std::fs::remove_file(&wav_norm);
        let _ = std::fs::remove_file(&wav_music);
        let _ = std::fs::remove_file(&wav_mixed);
    };

    // 1. Write the render bytes to the temp video file.
    let vid_size = bytes.len() as u64;
    if let Err(e) = std::fs::write(&temp_vid, &bytes) {
        error!(target: "screenrec", rec = %id, path = %temp_vid.display(), error = %e, "finalize: failed to write temp render mp4");
        cleanup();
        return Err(format!("could not save the rendered video: {e}"));
    }
    info!(target: "screenrec", rec = %id, size = vid_size, has_trim = trim.is_some(), "finalize: wrote temp render video");

    // 2. Extract 48kHz mono audio from the recording's playable file.
    let playable_c = playable.clone();
    let wav_full_c = wav_full.clone();
    let extract = tokio::task::spawn_blocking(move || {
        crate::screenrec::extract_audio_at(&playable_c, &wav_full_c, 48_000)
    })
    .await
    .map_err(|_| {
        cleanup();
        "audio extraction task panicked".to_string()
    })?;

    // 2a. No audio track → keep it a video-only export. Promote temp video.
    if let Err(e) = extract {
        if e == "no_audio" {
            info!(target: "screenrec", rec = %id, "finalize: recording has no audio; saving video-only render");
            if let Err(e) = std::fs::rename(&temp_vid, &out) {
                error!(target: "screenrec", rec = %id, error = %e, "finalize: failed to promote video-only render");
                cleanup();
                return Err("could not save the rendered video. See Settings → Diagnostics → logs for details.".to_string());
            }
            cleanup();
            let out_path = out.to_string_lossy().to_string();
            info!(target: "screenrec", rec = %id, path = %out_path, size = vid_size, "finalize: saved video-only render");
            let state = app.state::<AppState>();
            let db = require_db(&state)?;
            let row =
                record_rendered_export(db, &id, &existing_exports, "rendered", &out_path, vid_size)?;
            // Refresh any open list/detail view — the upload button flips to
            // "Edited" only once it sees the new exports row.
            let _ = app.emit("screenrec-changed", ());
            return Ok(row);
        }
        error!(target: "screenrec", rec = %id, error = %e, "finalize: audio extraction failed");
        cleanup();
        return Err("could not prepare the recording's audio. See Settings → Diagnostics → logs for details.".to_string());
    }
    info!(target: "screenrec", rec = %id, "finalize: extracted audio");

    // 3. Optionally trim the WAV to the kept window so audio aligns with video.
    let audio_for_mux = if let Some((start_ms, end_ms)) = trim {
        let wav_full_c = wav_full.clone();
        let wav_trim_c = wav_trim.clone();
        let trimmed = tokio::task::spawn_blocking(move || {
            crate::screenrec::trim_wav_samples(&wav_full_c, &wav_trim_c, start_ms, end_ms)
        })
        .await
        .map_err(|_| {
            cleanup();
            "audio trim task panicked".to_string()
        })?;
        if let Err(e) = trimmed {
            error!(target: "screenrec", rec = %id, start_ms, end_ms, error = %e, "finalize: WAV trim failed");
            cleanup();
            return Err("could not trim the recording's audio. See Settings → Diagnostics → logs for details.".to_string());
        }
        info!(target: "screenrec", rec = %id, start_ms, end_ms, "finalize: trimmed audio to window");
        wav_trim.clone()
    } else {
        wav_full.clone()
    };

    // 3b. Optionally retime the (already-trimmed) audio for speed segments.
    // Ranges arrive in POST-TRIM time (frontend `shiftRangesForTrim`), so this
    // MUST run after the trim step and reads the trimmed WAV directly.
    // `retime_wav_samples` clamps ranges to the audio's actual length first
    // (the frontend builds ranges against the video's nominal duration, which
    // can slightly exceed the trimmed audio's real length), so a remaining
    // `Err` here means genuinely malformed input (unsorted/overlapping/bad
    // rate). We do NOT fall back to un-retimed audio in that case — muxing
    // un-retimed audio onto already-retimed video is a silent, permanent A/V
    // desync. Fail the export loudly instead.
    let audio_for_mux = if let Some(ranges) = speed_ranges {
        let n = ranges.len();
        let audio_in = audio_for_mux.clone();
        let wav_speed_c = wav_speed.clone();
        let retimed = tokio::task::spawn_blocking(move || {
            crate::screenrec::retime_wav_samples(&audio_in, &wav_speed_c, &ranges)
        })
        .await
        .map_err(|_| {
            cleanup();
            "audio retime task panicked".to_string()
        })?;
        match retimed {
            Ok(()) => {
                info!(target: "screenrec", rec = %id, n_ranges = n, "finalize: retimed audio for speed segments");
                wav_speed.clone()
            }
            Err(e) => {
                error!(target: "screenrec", rec = %id, error = %e, "finalize: audio retime failed; failing export instead of desyncing A/V");
                cleanup();
                return Err("Export failed while adjusting audio speed — try again or remove the speed segments. See logs.".to_string());
            }
        }
    } else {
        audio_for_mux
    };

    // 3b2. Optionally RNNoise-denoise the (trimmed + retimed) audio. Runs
    // BEFORE normalize so loudness is measured on the cleaned signal, and after
    // retime so denoise sees the exact samples that will be muxed. The WAV is
    // 48kHz mono (extract_audio_at) — exactly what `denoise_wav` expects.
    // FAIL-SAFE like normalize: ANY failure (read/DSP/write/panic) degrades to
    // the un-denoised audio + `warn!` and never fails the export (denoise
    // preserves sample timing, so skipping it causes no A/V desync).
    let audio_for_mux = if denoise {
        let audio_in = audio_for_mux.clone();
        let wav_dn_c = wav_dn.clone();
        let denoised = tokio::task::spawn_blocking(move || {
            crate::denoise::denoise_wav(&audio_in, &wav_dn_c, |_pct| {})
        })
        .await;
        match denoised {
            Ok(Ok(())) => {
                info!(target: "screenrec", rec = %id, "finalize: denoised export audio");
                wav_dn.clone()
            }
            Ok(Err(e)) => {
                warn!(target: "screenrec", rec = %id, error = %e, "finalize: denoise failed; muxing un-denoised audio");
                audio_for_mux
            }
            Err(_) => {
                warn!(target: "screenrec", rec = %id, "finalize: denoise task panicked; muxing un-denoised audio");
                audio_for_mux
            }
        }
    } else {
        audio_for_mux
    };

    // 3c. Optionally loudness-normalize the (trimmed + retimed + denoised) audio
    // (Task 4). Runs AFTER retime/denoise so the level measured is the audio
    // that will actually be muxed. FAIL-SAFE: normalization is a polish step, so
    // ANY failure (WAV read, DSP, write, task panic) degrades to the
    // un-normalized audio + `warn!` — it must NEVER fail the export the way a
    // retime error does. Unlike retime, skipping normalization causes no A/V
    // desync (same sample timing), so a silent fallback here is safe.
    let audio_for_mux = if normalize_loudness {
        let audio_in = audio_for_mux.clone();
        let wav_norm_c = wav_norm.clone();
        let normalized = tokio::task::spawn_blocking(move || {
            crate::screenrec::normalize_wav_loudness(&audio_in, &wav_norm_c)
        })
        .await;
        match normalized {
            Ok(Ok(report)) => {
                info!(
                    target: "screenrec",
                    rec = %id,
                    measured_dbfs = report.measured_dbfs,
                    target_dbfs = report.target_dbfs,
                    gain = report.gain,
                    limited = report.limited,
                    samples = report.sample_count,
                    "finalize: normalized audio loudness"
                );
                wav_norm.clone()
            }
            Ok(Err(e)) => {
                warn!(target: "screenrec", rec = %id, error = %e, "finalize: loudness normalization failed; muxing un-normalized audio");
                audio_for_mux
            }
            Err(_) => {
                warn!(target: "screenrec", rec = %id, "finalize: loudness normalization task panicked; muxing un-normalized audio");
                audio_for_mux
            }
        }
    } else {
        audio_for_mux
    };

    // 3d. Optionally mix a background-music track under the (trimmed + retimed
    // + normalized) voice audio (Task 7). Runs AFTER normalize so the music
    // volume is relative to the FINAL speech level — set the music level once,
    // speech is always consistent. FAIL-SAFE: BOTH the music extraction and
    // the mix itself are best-effort; ANY failure (missing/unreadable file,
    // decode error, rate mismatch, task panic) degrades to music-less audio +
    // `warn!`. Music must NEVER fail an export.
    let audio_for_mux = if let Some(m) = music {
        let gain = m.volume.clamp(0.0, 1.0);
        let music_src = std::path::PathBuf::from(&m.path);
        let wav_music_c = wav_music.clone();
        let extract_music = tokio::task::spawn_blocking(move || {
            crate::screenrec::extract_audio_at(&music_src, &wav_music_c, 48_000)
        })
        .await;
        match extract_music {
            Ok(Ok(())) => {
                let audio_in = audio_for_mux.clone();
                let wav_music_c2 = wav_music.clone();
                let wav_mixed_c = wav_mixed.clone();
                let mixed = tokio::task::spawn_blocking(move || {
                    crate::screenrec::mix_wav_samples(&audio_in, &wav_music_c2, &wav_mixed_c, gain)
                })
                .await;
                match mixed {
                    Ok(Ok(())) => {
                        info!(target: "screenrec", rec = %id, music_path = %m.path, gain, "finalize: mixed background music under voice audio");
                        wav_mixed.clone()
                    }
                    Ok(Err(e)) => {
                        warn!(target: "screenrec", rec = %id, error = %e, "finalize: music mix failed; muxing voice-only audio");
                        audio_for_mux
                    }
                    Err(_) => {
                        warn!(target: "screenrec", rec = %id, "finalize: music mix task panicked; muxing voice-only audio");
                        audio_for_mux
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(target: "screenrec", rec = %id, path = %m.path, error = %e, "finalize: music extraction failed; muxing voice-only audio");
                audio_for_mux
            }
            Err(_) => {
                warn!(target: "screenrec", rec = %id, "finalize: music extraction task panicked; muxing voice-only audio");
                audio_for_mux
            }
        }
    } else {
        audio_for_mux
    };

    // 4. Mux the (possibly trimmed + retimed + normalized + music-mixed) audio
    // into video → out.
    let temp_vid_c = temp_vid.clone();
    let audio_c = audio_for_mux.clone();
    let out_c = out.clone();
    let mux = tokio::task::spawn_blocking(move || {
        crate::screenrec::mux_audio(&temp_vid_c, &audio_c, &out_c)
    })
    .await
    .map_err(|_| {
        cleanup();
        "audio mux task panicked".to_string()
    })?;
    if let Err(e) = mux {
        error!(target: "screenrec", rec = %id, error = %e, "finalize: audio mux failed");
        cleanup();
        return Err("could not attach audio to the rendered video. See Settings → Diagnostics → logs for details.".to_string());
    }

    // 5. Verify the muxed output before touching the DB.
    let final_size = match std::fs::metadata(&out) {
        Ok(m) if m.len() > 0 => m.len(),
        _ => {
            error!(target: "screenrec", rec = %id, "finalize: mux produced an empty/missing file");
            cleanup();
            return Err("the rendered video could not be finalized. See Settings → Diagnostics → logs for details.".to_string());
        }
    };
    cleanup(); // muxed `out` written; temps no longer needed
    let out_path = out.to_string_lossy().to_string();
    info!(target: "screenrec", rec = %id, path = %out_path, size = final_size, "finalize: saved rendered mp4 with audio");

    let state = app.state::<AppState>();
    let db = require_db(&state)?;
    let row = record_rendered_export(db, &id, &existing_exports, "rendered", &out_path, final_size)?;
    // Refresh any open list/detail view — the upload button flips to "Edited"
    // only once it sees the new exports row (the editor runs in its own
    // window, so without this the dashboard kept a stale row and uploaded the
    // RAW capture even after an export).
    let _ = app.emit("screenrec-changed", ());
    Ok(row)
}

/// Replace a recording's thumbnail with the poster frame of an editor export
/// (raw JPEG bytes as the IPC body, `x-recording-id` header — same transport
/// as `save_rendered_gif`). Written to `<id>.edited.jpg` (a NEW filename, so
/// every `<img>` holding the old `convertFileSrc` URL refetches instead of
/// showing a stale cached image) and pointed at via `thumb_path`. Best-effort
/// from the caller's perspective: the editor fires it after a successful MP4
/// export and ignores failures (the export itself already succeeded).
#[tauri::command]
pub async fn set_recording_thumbnail(
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let id = request
        .headers()
        .get("x-recording-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            error!(target: "screenrec", "set_recording_thumbnail missing x-recording-id header");
            "internal error: missing recording id".to_string()
        })?;
    let bytes: Vec<u8> = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.to_vec(),
        tauri::ipc::InvokeBody::Json(_) => {
            error!(target: "screenrec", rec = %id, "set_recording_thumbnail received JSON body, expected raw bytes");
            return Err("internal error: thumbnail bytes not received".to_string());
        }
    };
    if bytes.is_empty() {
        error!(target: "screenrec", rec = %id, "set_recording_thumbnail received empty body");
        return Err("thumbnail render produced no data".to_string());
    }

    // DB-gate before writing anything.
    {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or("recording not found")?;
    }

    let dir = crate::screenrec::recordings_dir().map_err(|e| e.to_string())?;
    let dest = dir.join(format!("{id}.edited.jpg"));
    std::fs::write(&dest, &bytes).map_err(|e| {
        error!(target: "screenrec", rec = %id, path = %dest.display(), error = %e, "set_recording_thumbnail: write failed");
        "Could not save the updated thumbnail. See Settings → Diagnostics → logs for details."
            .to_string()
    })?;

    let thumb = dest.to_string_lossy().to_string();
    {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::update_thumb(c, &id, &thumb))
            .map_err(|e| {
                error!(target: "screenrec", rec = %id, error = %e, "set_recording_thumbnail: DB update failed");
                e.to_string()
            })?;
    }
    info!(target: "screenrec", rec = %id, path = %thumb, bytes = bytes.len(), "updated thumbnail from edited export");
    let _ = app.emit("screenrec-changed", ());
    Ok(())
}

/// Save an editor GIF export: take the frontend's fully-rendered animated GIF
/// (raw IPC body) and write it verbatim to `<id>.rendered.gif` in the recordings
/// dir. Unlike `finalize_rendered_recording`, there is NO audio path — a GIF has
/// no soundtrack, so the frontend hands over the finished container and Rust only
/// persists it + records the export row. Emits `screenrec-changed` so any list
/// view refreshes.
///
/// Transport mirrors `finalize_rendered_recording`: the bytes ride as the raw
/// IPC body (`InvokeBody::Raw`), the recording id in the `x-recording-id` header.
///
/// Flow:
///   1. Read + validate the `x-recording-id` header and the raw body.
///   2. DB-gate the id (must exist) and read its current `exports` JSON.
///   3. Write the bytes to `<id>.rendered.gif` (fsync via `std::fs::write`).
///   4. Merge the `"rendered-gif"` exports entry (replace-not-duplicate) — a
///      distinct quality from the MP4 `"rendered"` row, so the two coexist.
///   5. Emit `screenrec-changed` and return the refreshed row.
#[tauri::command]
pub async fn save_rendered_gif(
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<crate::db::recordings::RecordingRow, String> {
    let id = request
        .headers()
        .get("x-recording-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            error!(target: "screenrec", "save_rendered_gif missing x-recording-id header");
            "internal error: missing recording id".to_string()
        })?;

    // Copy the GIF bytes out of the borrowed request before any await.
    let bytes: Vec<u8> = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.to_vec(),
        tauri::ipc::InvokeBody::Json(_) => {
            error!(target: "screenrec", rec = %id, "save_rendered_gif received JSON body, expected raw bytes");
            return Err("internal error: rendered GIF bytes not received".to_string());
        }
    };
    if bytes.is_empty() {
        error!(target: "screenrec", rec = %id, "save_rendered_gif received empty body");
        return Err("render produced no data".to_string());
    }

    // DB-gate: confirm the recording exists and grab its current exports JSON.
    let existing_exports: String = {
        let state = app.state::<AppState>();
        let db = require_db(&state)?;
        let row = db
            .with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                error!(target: "screenrec", rec = %id, "save_rendered_gif: recording not found");
                "recording not found".to_string()
            })?;
        row.exports
    };

    let dir = crate::screenrec::recordings_dir().map_err(|e| {
        error!(target: "screenrec", rec = %id, error = %e, "save_rendered_gif: recordings_dir failed");
        "could not locate the recordings folder".to_string()
    })?;
    let out = dir.join(format!("{id}.rendered.gif"));

    let size = bytes.len() as u64;
    if let Err(e) = std::fs::write(&out, &bytes) {
        error!(target: "screenrec", rec = %id, path = %out.display(), error = %e, "save_rendered_gif: failed to write gif");
        return Err("could not save the rendered GIF. See Settings → Diagnostics → logs for details.".to_string());
    }
    let out_path = out.to_string_lossy().to_string();
    info!(target: "screenrec", rec = %id, path = %out_path, size, "save_rendered_gif: saved rendered gif");

    let state = app.state::<AppState>();
    let db = require_db(&state)?;
    let row = record_rendered_export(db, &id, &existing_exports, "rendered-gif", &out_path, size)?;
    let _ = app.emit("screenrec-changed", ());
    Ok(row)
}

#[tauri::command]
pub fn open_screenrec_setup(app: AppHandle) -> Result<(), String> {
    crate::overlay::show_screenrec_setup(&app);
    Ok(())
}

/// Show the floating camera self-view for `camera_name` — used by the setup
/// window to PRE-WARM the camera the moment it's enabled in the picker, so the
/// device is already powered and streaming when the recorder attaches at
/// capture start (previously the camera's first open happened after the
/// countdown, costing seconds of cold USB negotiation and an on/off/on cycle).
/// The self-view page is idempotent: showing again with the same camera keeps
/// the running stream.
#[tauri::command]
pub fn show_camera_preview(app: AppHandle, camera_name: String) -> Result<(), String> {
    info!(target: "screenrec", camera = %camera_name, "showing camera self-view (pre-warm)");
    crate::overlay::show_camera_preview(&app, &camera_name);
    Ok(())
}

/// Hide the self-view and release the camera. Called by the setup window when
/// the camera is disabled or setup is dismissed without starting a recording
/// (the recording stop path hides it independently).
#[tauri::command]
pub fn hide_camera_preview(app: AppHandle) -> Result<(), String> {
    info!(target: "screenrec", "hiding camera self-view");
    crate::overlay::hide_camera_preview(&app);
    Ok(())
}

#[tauri::command]
pub fn list_screen_sources() -> Result<crate::screenrec::Sources, String> {
    crate::screenrec::list_sources()
}

// ----- Area picker commands -----

/// Show the full-screen area-picker overlay on `display_id`. See
/// `overlay::show_area_picker` for the coordinate-space contract.
#[tauri::command]
pub fn show_area_picker(app: AppHandle, display_id: u32) -> Result<(), String> {
    crate::overlay::show_area_picker(&app, display_id)
}

/// Hide the area-picker overlay unconditionally. Called by the setup window
/// on every path that should dismiss the picker without a result (e.g. it
/// re-shows itself before the picker reports back, or the setup window
/// itself is closing).
#[tauri::command]
pub fn close_area_picker(app: AppHandle) {
    crate::overlay::hide_area_picker(&app);
}

/// Called by the area-picker page when the user confirms a drag (mouse-up)
/// or cancels (Esc). Hides the picker unconditionally (so a cancel can never
/// strand the always-on-top overlay) and, on confirm, forwards the rect to
/// the setup window via an `area-picker-result` event. `rect` is `None` on
/// cancel; `Some([x, y, w, h])` (GLOBAL points) on confirm.
#[tauri::command]
pub fn submit_area_picker_result(app: AppHandle, rect: Option<[f64; 4]>) {
    crate::overlay::hide_area_picker(&app);
    match rect {
        Some(r) => {
            info!(target: "screenrec", ?r, "area picker confirmed");
            if let Some(setup) = app.get_webview_window("screenrec_setup") {
                if let Err(e) = setup.emit("area-picker-result", serde_json::json!({ "rect": r })) {
                    warn!(target: "screenrec", ?e, "area-picker-result emit failed");
                }
            }
        }
        None => {
            info!(target: "screenrec", "area picker cancelled");
            if let Some(setup) = app.get_webview_window("screenrec_setup") {
                if let Err(e) = setup.emit("area-picker-result", serde_json::json!({ "rect": null })) {
                    warn!(target: "screenrec", ?e, "area-picker-result (cancel) emit failed");
                }
            }
        }
    }
}

// ----- Countdown commands -----

/// Show the pre-record countdown overlay centered on `display_id`, ticking
/// from `seconds`. See `overlay::show_countdown` for the coordinate-space
/// contract.
#[tauri::command]
pub fn show_countdown_overlay(app: AppHandle, display_id: u32, seconds: u32) -> Result<(), String> {
    crate::overlay::show_countdown(&app, display_id, seconds)
}

/// Hide the countdown overlay unconditionally. Called on natural completion,
/// Esc-cancel, and every recording-start failure path so the always-on-top
/// overlay can never be stranded.
#[tauri::command]
pub fn hide_countdown_overlay(app: AppHandle) {
    crate::overlay::hide_countdown(&app);
}

/// Called by the countdown page when the user presses Esc. Hides the
/// countdown and tells the setup window to re-show itself.
#[tauri::command]
pub fn cancel_countdown(app: AppHandle) {
    info!(target: "screenrec", "countdown cancelled");
    crate::overlay::hide_countdown(&app);
    if let Some(setup) = app.get_webview_window("screenrec_setup") {
        if let Err(e) = setup.emit("countdown-cancelled", ()) {
            warn!(target: "screenrec", ?e, "countdown-cancelled emit failed");
        }
    }
    crate::overlay::show_screenrec_setup(&app);
}

/// Called by the countdown page itself when its own visual tick reaches
/// zero. The countdown page is the SINGLE clock for "when does the
/// countdown end" — this event (not a second, independently-running timer
/// on the setup side) is what tells the setup window it may now call
/// `startScreenRecording`. This closes an Esc-cancel race: previously the
/// setup window ran its own parallel `setTimeout` of the same nominal
/// duration, so a very-late Esc could have `cancel_countdown` and the
/// setup window's timer fire within the same tick — recording could start
/// AND the setup window get re-shown over a live recording. With a single
/// event-driven source of truth, `countdown-cancelled` and
/// `countdown-finished` can never both "win": the setup window's own
/// cancel-wins guard (see `SetupWindow.tsx`) ignores a `countdown-finished`
/// that arrives after a cancel was already processed.
///
/// Does NOT hide the countdown window or touch the setup window here —
/// unlike `cancel_countdown`, the setup window still owns calling
/// `hide_countdown_overlay` (after it starts recording) so a
/// `startScreenRecording` failure can leave the countdown hidden but the
/// setup window re-shown with the error, exactly as before.
#[tauri::command]
pub fn finish_countdown(app: AppHandle) {
    info!(target: "screenrec", "countdown finished");
    if let Some(setup) = app.get_webview_window("screenrec_setup") {
        if let Err(e) = setup.emit("countdown-finished", ()) {
            warn!(target: "screenrec", ?e, "countdown-finished emit failed");
        }
    }
}

/// Bounds (`[x, y, width, height]`, GLOBAL POINTS, top-left origin) of the
/// display with the given `--list-sources` id — the SAME id `start_screen_recording`
/// takes as `display_id` and the coordinate space its `rect` param expects.
/// Backs both the area picker (sizes/positions the overlay to cover exactly
/// the target display) and the countdown window (centers on the target
/// display). See `crate::screenrec::display_bounds` for the source-of-truth
/// rationale (CGDisplayBounds, keyed by CGDirectDisplayID == SCDisplay.displayID).
#[tauri::command]
pub fn get_display_bounds(display_id: u32) -> Result<[f64; 4], String> {
    crate::screenrec::display_bounds(display_id)
        .map(|(x, y, w, h)| [x, y, w, h])
        .ok_or_else(|| {
            warn!(target: "screenrec", display_id, "get_display_bounds: display not found");
            "That display is no longer available. Reopen the recording setup and pick a display again.".to_string()
        })
}

/// List available cameras for webcam recording via the sidecar's
/// `--list-cameras`. On failure returns the friendly message from
/// `list_cameras_error` (raw detail is logged).
#[tauri::command]
pub fn list_cameras() -> Result<crate::screenrec::Cameras, String> {
    crate::screenrec::list_cameras()
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ScreenrecAudioPrefs {
    pub sysaudio: bool,
    pub mic_enabled: bool,
    pub mic_device: String,
    // `#[serde(default)]` so prefs objects written before this field existed
    // (and any partial payload from an older frontend) deserialize cleanly to
    // `false` instead of erroring on the missing key.
    #[serde(default)]
    pub hide_cursor: bool,
    // UID of the selected webcam, or empty string when webcam recording is off.
    // `#[serde(default)]` for the same forward/backward-compat reason as above.
    #[serde(default)]
    pub camera_uid: String,
    // Whether the pre-record countdown (3→2→1) is enabled. `#[serde(default)]`
    // for the same forward/backward-compat reason as `hide_cursor`/`camera_uid`.
    #[serde(default)]
    pub countdown: bool,
}

#[tauri::command]
pub fn get_screenrec_audio_prefs(
    state: State<'_, AppState>,
) -> Result<ScreenrecAudioPrefs, String> {
    Ok(ScreenrecAudioPrefs {
        sysaudio: state.settings.screenrec_sysaudio(),
        mic_enabled: state.settings.screenrec_mic_enabled(),
        mic_device: state.settings.screenrec_mic_device(),
        hide_cursor: state.settings.screenrec_hide_cursor(),
        camera_uid: state.settings.screenrec_camera_uid(),
        countdown: state.settings.screenrec_countdown(),
    })
}

#[tauri::command]
pub fn set_screenrec_audio_prefs(
    state: State<'_, AppState>,
    prefs: ScreenrecAudioPrefs,
) -> Result<(), String> {
    state
        .settings
        .set_screenrec_sysaudio(prefs.sysaudio)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_screenrec_mic_enabled(prefs.mic_enabled)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_screenrec_mic_device(prefs.mic_device)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_screenrec_hide_cursor(prefs.hide_cursor)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_screenrec_camera_uid(prefs.camera_uid)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_screenrec_countdown(prefs.countdown)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ----- Drive commands -----

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DriveStatus {
    pub connected: bool,
    pub email: Option<String>,
}

#[tauri::command]
pub fn drive_status(state: State<'_, AppState>) -> DriveStatus {
    DriveStatus {
        connected: crate::screenrec::drive::load_refresh_token().is_some(),
        email: state.settings.drive_account_email(),
    }
}

#[tauri::command]
pub async fn drive_connect(state: State<'_, AppState>) -> Result<DriveStatus, String> {
    // Extract owned creds BEFORE awaiting so no State borrow crosses .await.
    let (cid, csecret) = crate::screenrec::drive::effective_client(
        &state.settings.drive_client_id(),
        &state.settings.drive_client_secret(),
    );
    let email = match crate::screenrec::drive::connect(&cid, &csecret).await {
        Ok(e) => e,
        Err(e) => {
            error!(target: "drive", error = %e, "Drive connect failed");
            return Err(
                "Couldn't connect to Google Drive. See Settings → Diagnostics → logs for details."
                    .into(),
            );
        }
    };
    state
        .settings
        .set_drive_account_email(email.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(DriveStatus {
        connected: true,
        email,
    })
}

#[tauri::command]
pub fn drive_disconnect(state: State<'_, AppState>) -> Result<(), String> {
    crate::screenrec::drive::delete_refresh_token()?;
    state
        .settings
        .set_drive_account_email(None)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_drive_folder_id(None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn get_drive_client_id(state: State<'_, AppState>) -> String {
    state.settings.drive_client_id()
}

#[tauri::command]
pub fn set_drive_client_credentials(
    state: State<'_, AppState>,
    client_id: String,
    client_secret: String,
) -> Result<(), String> {
    state
        .settings
        .set_drive_client_id(&client_id)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_drive_client_secret(&client_secret)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DrivePrefs {
    pub folder_name: String,
    pub make_public: bool,
}

#[tauri::command]
pub fn get_drive_prefs(state: State<'_, AppState>) -> DrivePrefs {
    DrivePrefs {
        folder_name: state.settings.drive_folder_name(),
        make_public: state.settings.drive_make_public(),
    }
}

#[tauri::command]
pub fn set_drive_prefs(
    state: State<'_, AppState>,
    folder_name: String,
    make_public: bool,
) -> Result<(), String> {
    let prev = state.settings.drive_folder_name();
    state
        .settings
        .set_drive_folder_name(&folder_name)
        .map_err(|e| e.to_string())?;
    state
        .settings
        .set_drive_make_public(make_public)
        .map_err(|e| e.to_string())?;
    // Folder renamed → drop the cached id so the next upload resolves/creates it.
    if prev != folder_name {
        state
            .settings
            .set_drive_folder_id(None)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Strip characters that are unsafe/ugly in a filename, collapse whitespace
/// runs to a single space, trim, and cap the length. Used to turn a recording's
/// free-form display name (which may be a browser tab title with a URL, an app
/// window title, etc.) into a clean Drive filename component.
fn sanitize_filename_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        // Whitespace first — newlines/tabs are ALSO control chars, but we want
        // them to collapse to a single space (join with a gap), not vanish and
        // fuse adjacent words.
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
            continue;
        }
        if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            continue; // drop path/OS-reserved + other control characters
        }
        out.push(c);
        last_ws = false;
    }
    // Cap length (Drive allows long names, but keep it sane) and re-trim.
    out.trim().chars().take(120).collect::<String>().trim().to_string()
}

/// Build a human-readable Drive filename for an upload from the recording's
/// display name — its user title, else the captured source label (the app /
/// window / browser+URL context), else "Recording" — plus a version tag so the
/// original / edited / downscaled variants don't collide, plus the recording's
/// date for readability. Keeps the actual file's extension. Always returns a
/// non-empty name (falls back through every step).
fn drive_upload_name(
    row: &crate::db::recordings::RecordingRow,
    upload_path: &std::path::Path,
    quality: &str,
) -> String {
    let ext = upload_path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("mp4");
    // Display name: title → source_label → "Recording" (mirrors the frontend's
    // `recordingDisplayName`), then sanitized.
    let raw = row
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            row.source_label
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("Recording");
    let mut base = sanitize_filename_component(raw);
    if base.is_empty() {
        base = "Recording".to_string();
    }
    // Version tag so uploading original + edited + a downscale of the same
    // recording yields distinct, self-describing Drive names.
    let tag = match quality {
        "rendered" => " (edited)".to_string(),
        "original" => String::new(),
        q => format!(" ({q}p)"),
    };
    // Recording date (local), for readability + light disambiguation. Best
    // effort — omitted if the timestamp can't be interpreted.
    let date = chrono::DateTime::from_timestamp_millis(row.created_at)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default();
    let mut name = format!("{base}{tag}");
    if !date.is_empty() {
        name.push(' ');
        name.push_str(&date);
    }
    format!("{name}.{ext}")
}

/// Upload a recording at `quality` ("rendered"|"original"|"1080"|"720"|"480")
/// to Drive and return the updated row (with `drive_link`).
///
/// - `"rendered"` uploads the EDITED export produced by the editor
///   (`finalize_rendered_recording`'s entry in the row's `exports` JSON) — the
///   version with trim/zoom/webcam/background applied. Fails with a friendly
///   message when no edited export exists (or its file has been deleted), so
///   the UI can tell the user to export from the editor first.
/// - `"original"` uploads the raw recording file.
/// - Size presets transcode the ORIGINAL first (`crate::screenrec::export`).
#[tauri::command]
pub async fn upload_recording(
    state: State<'_, AppState>,
    app: AppHandle,
    id: String,
    quality: String,
    make_public: Option<bool>,
) -> Result<crate::db::recordings::RecordingRow, String> {
    // Gather everything needed from State up front (owned values), mirroring
    // how transcribe_recording handles the Db handle across awaits.
    let row = {
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::get(c, &id))
            .map_err(|e| e.to_string())?
            .ok_or("recording not found")?
    };
    let (cid, csecret) = crate::screenrec::drive::effective_client(
        &state.settings.drive_client_id(),
        &state.settings.drive_client_secret(),
    );
    let existing_folder: Option<String> = state.settings.drive_folder_id();
    let folder_name = state.settings.drive_folder_name();
    // Per-video override; falls back to the Settings default when not specified.
    let make_public = make_public.unwrap_or_else(|| state.settings.drive_make_public());

    // Mark uploading and notify UI.
    {
        let db = require_db(&state)?;
        db.with_conn(|c| crate::db::recordings::update_upload_status(c, &id, "uploading", None))
            .map_err(|e| e.to_string())?;
    }
    let _ = app.emit("screenrec-changed", ());

    // Resolve the file to upload (export first if a quality preset was chosen).
    let upload_path: std::path::PathBuf = if quality == "rendered" {
        // The edited export recorded by finalize_rendered_recording. Friendly
        // errors here — the raw detail (exports JSON state / missing path) goes
        // to the log, the returned string is shown in the UI as-is.
        let rendered = serde_json::from_str::<Vec<serde_json::Value>>(&row.exports)
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.get("quality").and_then(|q| q.as_str()) == Some("rendered"))
            .and_then(|e| e.get("path").and_then(|p| p.as_str()).map(|p| p.to_string()));
        let Some(path) = rendered else {
            error!(target: "drive", rec = %id, exports = %row.exports, "upload: no rendered export recorded");
            // Reset the "uploading" status set above before bailing.
            let db = require_db(&state)?;
            let friendly = "No edited version yet — open the editor and export first.";
            let _ = db.with_conn(|c| {
                crate::db::recordings::update_upload_status(c, &id, "error", Some(friendly))
            });
            let _ = app.emit("screenrec-changed", ());
            return Err(friendly.to_string());
        };
        let p = std::path::PathBuf::from(&path);
        if !p.is_file() {
            error!(target: "drive", rec = %id, path = %p.display(), "upload: rendered export file missing on disk");
            let db = require_db(&state)?;
            let friendly = "The edited export file is missing — re-export from the editor.";
            let _ = db.with_conn(|c| {
                crate::db::recordings::update_upload_status(c, &id, "error", Some(friendly))
            });
            let _ = app.emit("screenrec-changed", ());
            return Err(friendly.to_string());
        }
        info!(target: "drive", rec = %id, path = %p.display(), "uploading rendered (edited) export");
        p
    } else if quality == "original" {
        std::path::PathBuf::from(&row.file_path)
    } else {
        let src = std::path::PathBuf::from(&row.file_path);
        let stem = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rec")
            .to_string();
        let dir = crate::screenrec::recordings_dir().map_err(|e| e.to_string())?;
        let out = dir.join(format!("{stem}-{quality}.mp4"));
        let q = quality.clone();
        let out2 = out.clone();
        tokio::task::spawn_blocking(move || crate::screenrec::export(&src, &out2, &q))
            .await
            .map_err(|e| e.to_string())??;
        out
    };
    // Human-readable Drive name from the recording's display name (not the
    // opaque `<id>.mp4` on-disk filename) — so uploads read as, e.g.,
    // "Broker SAM (edited) 2026-07-23.mp4".
    let upload_name = drive_upload_name(&row, &upload_path, &quality);
    info!(target: "drive", rec = %id, upload_name = %upload_name, "resolved upload filename");

    // Drive sequence: refresh token → ensure folder → upload → share → get link.
    // Returns (file_id, link, folder_to_persist) where folder_to_persist is Some
    // only when we created a new folder (so we can cache it in settings).
    let drive_result: Result<(String, String, Option<String>), String> = async {
        let access = crate::screenrec::drive::refresh_access_token(&cid, &csecret).await?;
        let (folder, folder_to_persist) = match existing_folder {
            Some(f) => (f, None),
            None => {
                let f = crate::screenrec::drive::ensure_folder(&access, &folder_name).await?;
                let persist = Some(f.clone());
                (f, persist)
            }
        };
        let file_id =
            crate::screenrec::drive::upload_resumable(&access, &folder, &upload_path, &upload_name)
                .await?;
        info!(target: "drive", file_id = %file_id, make_public, "setting file visibility");
        if make_public {
            crate::screenrec::drive::make_anyone_reader(&access, &file_id).await?;
        }
        let link = crate::screenrec::drive::web_view_link(&access, &file_id).await?;
        Ok((file_id, link, folder_to_persist))
    }
    .await;

    match drive_result {
        Ok((file_id, link, folder_to_persist)) => {
            // Persist the newly-created folder id so future uploads reuse it.
            if let Some(ref folder_id) = folder_to_persist {
                state
                    .settings
                    .set_drive_folder_id(Some(folder_id.as_str()))
                    .map_err(|e| e.to_string())?;
            }
            {
                let db = require_db(&state)?;
                db.with_conn(|c| crate::db::recordings::update_drive_link(c, &id, &file_id, &link))
                    .map_err(|e| e.to_string())?;
            }
        }
        Err(e) => {
            // Full technical detail to the log; a short, friendly message to the UI.
            error!(target: "drive", error = %e, "Drive upload failed");
            // Both shapes mean the same thing to the user: there is no usable
            // Drive authorization (token revoked server-side, or none stored —
            // e.g. right after an invalid_grant cleared it). The frontend
            // matches the sentinel prefix to offer a reconnect flow instead of
            // a dead-end error.
            let needs_reconnect = e.contains(crate::screenrec::drive::RECONNECT_REQUIRED)
                || e.contains("not connected to Drive");
            let friendly = if needs_reconnect {
                "Google Drive isn't connected — reconnect to upload."
            } else {
                "Upload to Drive failed. See Settings → Diagnostics → logs for details."
            };
            {
                let db = require_db(&state)?;
                db.with_conn(|c| {
                    crate::db::recordings::update_upload_status(c, &id, "error", Some(friendly))
                })
                .map_err(|e| e.to_string())?;
            }
            let _ = app.emit("screenrec-changed", ());
            return Err(if needs_reconnect {
                format!("{}: {friendly}", crate::screenrec::drive::RECONNECT_REQUIRED)
            } else {
                friendly.to_string()
            });
        }
    }
    let _ = app.emit("screenrec-changed", ());
    let db = require_db(&state)?;
    db.with_conn(|c| crate::db::recordings::get(c, &id))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "recording vanished".to_string())
}

/// Download the embedding model (user-triggered). Emits
/// `embed-model-download-progress` events; the background indexer picks up
/// once the file exists on disk.
#[tauri::command]
pub async fn download_embedding_model(app: AppHandle) -> Result<(), String> {
    let entry = crate::embed::catalog::model();
    let dir = crate::llm::downloader::model_dir(entry);
    let app_for_cb = app.clone();
    crate::llm::downloader::download_model(entry, &dir, move |p| {
        let _ = app_for_cb.emit("embed-model-download-progress", p);
    })
    .await
    .map_err(|e| {
        tracing::error!(target: "embed", error = %e, "embedding model download failed");
        "Embedding model download failed. See Settings → Diagnostics → logs for details.".to_string()
    })?;
    tracing::info!(target: "embed", "embedding model downloaded");
    Ok(())
}

#[derive(serde::Serialize)]
pub struct EmbeddingIndexStatus {
    pub model_downloaded: bool,
    pub embeddings: i64,
    pub indexed_sources: i64,
    pub total_sources: i64,
}

/// Report embedding-index progress for the Settings UI / chat banner.
#[tauri::command]
pub fn embedding_index_status(
    state: State<'_, AppState>,
) -> Result<EmbeddingIndexStatus, String> {
    let db = require_db(&state)?;
    db.with_conn(|c| {
        let embeddings = crate::db::embeddings::count_embeddings(c)?;
        let indexed_sources = crate::db::embeddings::count_indexed_sources(c)?;
        let docs = crate::chat_memory::source::collect_source_docs(c)?;
        Ok(EmbeddingIndexStatus {
            model_downloaded: crate::embed::catalog::is_downloaded(),
            embeddings,
            indexed_sources,
            total_sources: docs.len() as i64,
        })
    })
    .map_err(|e: crate::db::DbError| {
        tracing::error!(target: "embed", error = %e, "embedding_index_status failed");
        "Could not read index status.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdev::Key;

    /// Minimal RecordingRow for the Drive-name tests (only the fields
    /// `drive_upload_name` reads matter; the rest are inert placeholders).
    fn row_for(
        title: Option<&str>,
        source_label: Option<&str>,
        created_at: i64,
    ) -> crate::db::recordings::RecordingRow {
        crate::db::recordings::RecordingRow {
            id: "rec-1".into(),
            created_at,
            file_path: "/tmp/rec-1.mp4".into(),
            duration_ms: None,
            width: None,
            height: None,
            size_bytes: None,
            source_label: source_label.map(|s| s.to_string()),
            has_mic: false,
            has_sysaudio: false,
            thumb_path: None,
            drive_file_id: None,
            drive_link: None,
            upload_status: "none".into(),
            upload_error: None,
            exports: "[]".into(),
            title: title.map(|s| s.to_string()),
            transcript: None,
            denoised_path: None,
            events_path: None,
            project_json: None,
            webcam_path: None,
            cursor_hidden: false,
            webcam_offset_ms: None,
            n_events: None,
            n_clicks: None,
            project_id: None,
            confidence: None,
            classified_by: None,
        }
    }

    #[test]
    fn sanitize_filename_component_strips_and_collapses() {
        // Path/OS-reserved characters dropped; whitespace runs collapsed.
        assert_eq!(
            sanitize_filename_component("Foo/Bar: \"baz\"  <qux>"),
            "FooBar baz qux"
        );
        // Newlines/tabs are control/whitespace → collapse to single spaces.
        assert_eq!(sanitize_filename_component("a\n\tb"), "a b");
        // Leading/trailing whitespace trimmed.
        assert_eq!(sanitize_filename_component("  hello  "), "hello");
        // Length capped at 120.
        let long: String = "x".repeat(200);
        assert_eq!(sanitize_filename_component(&long).len(), 120);
    }

    #[test]
    fn drive_upload_name_uses_display_name_tag_and_date() {
        let path = std::path::Path::new("/tmp/abc123.rendered.mp4");
        // 2024-05-21 in UTC; the formatter uses local tz, so assert on the parts
        // that are tz-independent (name, tag, extension) and that a date is present.
        let row = row_for(Some("Broker SAM"), Some("Chrome — example.com"), 1_716_300_000_000);
        let edited = drive_upload_name(&row, path, "rendered");
        assert!(edited.starts_with("Broker SAM (edited) "), "{edited}");
        assert!(edited.ends_with(".mp4"), "{edited}");

        // No title → falls back to the source label (app/window context).
        let row2 = row_for(None, Some("Chrome — example.com"), 1_716_300_000_000);
        let orig = drive_upload_name(&row2, std::path::Path::new("/tmp/abc.mp4"), "original");
        assert!(orig.starts_with("Chrome — example.com "), "{orig}");
        assert!(!orig.contains("(edited)"), "{orig}");

        // Downscale quality → "(1080p)" tag; extension preserved from the path.
        let dn = drive_upload_name(&row, std::path::Path::new("/tmp/abc-1080.mp4"), "1080");
        assert!(dn.contains("(1080p)"), "{dn}");

        // Nothing usable → "Recording" fallback.
        let row3 = row_for(Some("   "), None, 1_716_300_000_000);
        let fb = drive_upload_name(&row3, std::path::Path::new("/tmp/x.gif"), "original");
        assert!(fb.starts_with("Recording "), "{fb}");
        assert!(fb.ends_with(".gif"), "{fb}");
    }

    #[test]
    fn key_from_code_handles_known_and_unknown_codes() {
        assert_eq!(key_from_code("ControlRight"), Some(Key::ControlRight));
        assert_eq!(key_from_code("KeyA"), Some(Key::KeyA));
        assert_eq!(key_from_code("Digit0"), Some(Key::Num0));
        assert_eq!(key_from_code("F12"), Some(Key::F12));
        assert_eq!(key_from_code("AltLeft"), Some(Key::Alt));
        assert_eq!(key_from_code("AltRight"), Some(Key::AltGr));
        assert_eq!(key_from_code("Enter"), Some(Key::Return));
        // Unknown codes return None.
        assert_eq!(key_from_code(""), None);
        assert_eq!(key_from_code("NotARealCode"), None);
        assert_eq!(key_from_code("F30"), None);
    }

    #[test]
    fn camera_access_outcome_str_maps_all_variants() {
        assert_eq!(
            camera_access_outcome_str(CameraAccessOutcome::Granted),
            "granted"
        );
        assert_eq!(
            camera_access_outcome_str(CameraAccessOutcome::Denied),
            "denied"
        );
        assert_eq!(
            camera_access_outcome_str(CameraAccessOutcome::Undetermined),
            "undetermined"
        );
    }

    #[test]
    fn containing_app_bundle_finds_only_the_echo_scribe_bundle() {
        let installed =
            std::path::Path::new("/Applications/Echo Scribe.app/Contents/MacOS/echo-scribe");
        assert_eq!(
            containing_app_bundle(installed),
            Some(std::path::PathBuf::from("/Applications/Echo Scribe.app"))
        );
        assert!(containing_app_bundle(std::path::Path::new(
            "/tmp/Another.app/Contents/MacOS/echo-scribe"
        ))
        .is_none());
        assert!(containing_app_bundle(std::path::Path::new(
            "/repo/src-tauri/target/debug/echo-scribe"
        ))
        .is_none());
    }

    #[test]
    fn uninstall_data_paths_cover_user_data_without_dev_signing_material() {
        let home = std::path::Path::new("/Users/tester");
        let paths = uninstall_data_paths(home);
        assert!(paths.contains(
            &home.join("Library/Application Support/EchoScribe")
        ));
        assert!(paths.contains(
            &home.join("Library/Application Support/com.echoscribe.app")
        ));
        assert!(paths.contains(&home.join("EchoScribe")));
        assert!(!paths
            .iter()
            .any(|path| path.to_string_lossy().contains("EchoScribeSigning")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(
            applescript_string(r#"/Users/a\b/"Echo".app"#),
            r#"/Users/a\\b/\"Echo\".app"#
        );
    }

    #[test]
    fn screenrec_audio_prefs_serde_defaults() {
        // A prefs blob written before camera_uid/hide_cursor/countdown existed
        // must still deserialize cleanly (all #[serde(default)]) so upgrading
        // users don't hit an error on the missing keys.
        let old = r#"{"sysaudio":true,"mic_enabled":false,"mic_device":"Mic X"}"#;
        let prefs: ScreenrecAudioPrefs = serde_json::from_str(old).expect("old prefs deserialize");
        assert!(prefs.sysaudio);
        assert!(!prefs.mic_enabled);
        assert_eq!(prefs.mic_device, "Mic X");
        assert!(!prefs.hide_cursor);
        assert_eq!(prefs.camera_uid, "");
        assert!(!prefs.countdown);

        // A blob written after hide_cursor/camera_uid but before countdown
        // existed must also still deserialize cleanly (countdown -> false).
        let mid = r#"{"sysaudio":true,"mic_enabled":false,"mic_device":"Mic X","hide_cursor":true,"camera_uid":"cam-1"}"#;
        let prefs: ScreenrecAudioPrefs = serde_json::from_str(mid).expect("mid prefs deserialize");
        assert!(prefs.hide_cursor);
        assert_eq!(prefs.camera_uid, "cam-1");
        assert!(!prefs.countdown);

        // Full round-trip with every field set survives serialize -> deserialize.
        let full = ScreenrecAudioPrefs {
            sysaudio: false,
            mic_enabled: true,
            mic_device: "Mic Y".into(),
            hide_cursor: true,
            camera_uid: "cam-uid-123".into(),
            countdown: true,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: ScreenrecAudioPrefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.camera_uid, "cam-uid-123");
        assert!(back.hide_cursor);
        assert_eq!(back.mic_device, "Mic Y");
        assert!(back.countdown);
    }

    #[test]
    fn editor_bg_extension_allowlist() {
        // Allowed extensions, case-insensitive, normalized to lower-case.
        assert_eq!(editor_bg_extension("/a/b/photo.png").unwrap(), "png");
        assert_eq!(editor_bg_extension("/a/b/photo.JPG").unwrap(), "jpg");
        assert_eq!(editor_bg_extension("/a/b/photo.jpeg").unwrap(), "jpeg");
        assert_eq!(editor_bg_extension("/a/b/photo.WEBP").unwrap(), "webp");
        // Rejected: wrong type, no extension.
        assert!(editor_bg_extension("/a/b/movie.mp4").is_err());
        assert!(editor_bg_extension("/a/b/doc.gif").is_err());
        assert!(editor_bg_extension("/a/b/noext").is_err());
    }

    #[test]
    fn js_binding_round_trips_through_binding() {
        // single-key
        let single = Binding::single(Key::ControlRight);
        let js: JsBinding = single.clone().into();
        assert_eq!(js.primary, "ControlRight");
        assert!(js.modifiers.is_empty());
        let back: Binding = js.try_into().expect("should convert back");
        assert_eq!(back, single);

        // combo with modifier
        let combo = Binding {
            primary: SerKey(Key::KeyL),
            modifiers: vec![(ModifierKind::Meta, ModifierSide::Right)],
        };
        let js: JsBinding = combo.clone().into();
        assert_eq!(js.primary, "KeyL");
        assert_eq!(js.modifiers.len(), 1);
        assert_eq!(js.modifiers[0].kind, "Meta");
        assert_eq!(js.modifiers[0].side, "Right");
        let back: Binding = js.try_into().expect("should convert back");
        assert_eq!(back, combo);
    }

    #[test]
    fn js_binding_rejects_unknown_key() {
        let js = JsBinding {
            primary: "BogusCode".to_string(),
            modifiers: vec![],
        };
        let err = Binding::try_from(js).unwrap_err();
        assert!(matches!(err, BindingConversionError::UnknownKey(_)));
    }

    #[test]
    fn build_rag_query_extracts_long_words() {
        let q = build_rag_query("what did I say about the project meeting yesterday");
        assert!(
            q.contains("\"about\"")
                || q.contains("\"project\"")
                || q.contains("\"meeting\"")
                || q.contains("\"yesterday\"")
        );
        assert!(!q.contains("\"did\""));
        assert!(!q.contains("\"the\""));
    }

    #[test]
    fn build_rag_query_returns_empty_for_short_message() {
        assert_eq!(build_rag_query("hi"), "");
        assert_eq!(build_rag_query("ok go"), "");
    }

    #[test]
    fn build_rag_query_strips_punctuation() {
        let q = build_rag_query("meeting! project?");
        assert!(q.contains("\"meeting\""));
        assert!(q.contains("\"project\""));
    }

    /// Create an empty file named `name` inside `dir`.
    fn touch(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    fn file_names(paths: &[std::path::PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn editor_bg_files_lists_only_this_ids_backgrounds() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "rec1.bg.png");
        touch(dir.path(), "rec1.bg.jpg");
        touch(dir.path(), "rec1.mp4"); // main recording, not a background
        touch(dir.path(), "rec1.rendered.mp4"); // rendered export, not a background
        touch(dir.path(), "rec12.bg.png"); // id sharing a prefix must not match
        touch(dir.path(), "other.bg.png");

        let names = file_names(&editor_bg_files(dir.path(), "rec1"));
        assert_eq!(names, vec!["rec1.bg.jpg", "rec1.bg.png"]);
        assert!(editor_bg_files(dir.path(), "rec99").is_empty());
    }

    #[test]
    fn editor_artifact_files_includes_backgrounds_and_rendered_export() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "rec1.bg.webp");
        touch(dir.path(), "rec1.rendered.mp4");
        touch(dir.path(), "rec1.mp4"); // main file is deleted via its DB path, not the sweep
        touch(dir.path(), "rec2.rendered.mp4"); // other recording's export

        let names = file_names(&editor_artifact_files(dir.path(), "rec1"));
        assert_eq!(names, vec!["rec1.bg.webp", "rec1.rendered.mp4"]);
        // No artifacts → empty, never an error.
        assert!(editor_artifact_files(dir.path(), "rec3").is_empty());
    }

    #[test]
    fn export_paths_reads_paths_and_tolerates_bad_json() {
        let json = r#"[
            {"quality":"720","path":"/r/rec1-720.mp4","size":1},
            {"quality":"rendered","path":"/r/rec1.rendered.mp4","size":2},
            {"quality":"nopath","size":3}
        ]"#;
        assert_eq!(
            export_paths(json),
            vec!["/r/rec1-720.mp4", "/r/rec1.rendered.mp4"]
        );
        assert!(export_paths("").is_empty());
        assert!(export_paths("not json").is_empty());
        assert!(export_paths("{}").is_empty());
    }

    #[test]
    fn validate_reveal_path_requires_existing_file_inside_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside_root = tempfile::tempdir().unwrap();
        let inside = root.path().join("a.mp4");
        std::fs::write(&inside, b"x").unwrap();
        let outside = outside_root.path().join("b.mp4");
        std::fs::write(&outside, b"x").unwrap();

        // A real file inside the dir passes (canonicalized).
        assert!(validate_reveal_path(root.path(), inside.to_str().unwrap()).is_ok());
        // A real file elsewhere is rejected.
        assert!(validate_reveal_path(root.path(), outside.to_str().unwrap()).is_err());
        // `..` traversal that escapes the dir is rejected even when it
        // resolves to a real file (both tempdirs share a parent).
        let sneaky = format!(
            "{}/../{}/b.mp4",
            root.path().display(),
            outside_root
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        assert!(validate_reveal_path(root.path(), &sneaky).is_err());
        // Missing files are rejected.
        let missing = root.path().join("nope.mp4");
        assert!(validate_reveal_path(root.path(), missing.to_str().unwrap()).is_err());
    }
}
