use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager, Wry};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::asr::pipeline::AsrPipeline;
use crate::audio::feedback::{self, Sfx};
use crate::audio::recorder::{Recorder, RecorderError};
use crate::classifier::{self, Classification};
use crate::db::items::{chrono_now_iso, Item, ItemSource};
use crate::db::Db;
use crate::event_log::{self, EventEnvelope};
use crate::input::focus::{self, FocusContext, FocusElement};
use crate::input::hotkeys::HotkeyEvent;
use crate::input::paste::paste_at_cursor;
use crate::llm::Llm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    VoiceAtCursor,
    ActionCommand,
    LogCapture,
    /// Voice-edit the current text selection in place ("Command Mode").
    EditSelection,
    /// Reserved for Phase 4+: cancel an in-flight capture (e.g. via Esc).
    /// Currently a no-op stub — the wiring is in place but we don't bind it
    /// to anything yet.
    #[allow(dead_code)]
    Cancel,
}

#[derive(Debug, Clone)]
pub enum CoordinatorMsg {
    Hotkey(Action, HotkeyEvent),
    /// User accepted the LogCapture overlay; persist with the supplied fields.
    /// `content` is the (possibly user-edited) transcript.
    ConfirmLogCapture {
        content: String,
        kind: crate::db::items::ItemKind,
        project_id: Option<String>,
        new_project_name: Option<String>,
        tags: Vec<String>,
        deadline_iso: Option<String>,
        reply: tokio::sync::mpsc::UnboundedSender<Result<String, String>>,
    },
    /// User rejected the LogCapture overlay; discard the buffered transcript.
    CancelLogCapture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineState {
    Idle,
    Recording(Action),
    Processing(Action),
    AwaitingConfirmation,
}

impl PipelineState {
    /// Tray icon mapping. Recording always shows the listening icon. The
    /// Processing phase is split downstream by direct `on_state_change`
    /// calls into Transcribing (ASR) and Thinking (LLM). This default
    /// returns Transcribing for the brief window between Released and
    /// the first explicit state change; AwaitingConfirmation reuses the
    /// Thinking glyph since classification has already completed.
    pub fn tray_state(&self) -> TrayPipelineState {
        match self {
            PipelineState::Idle => TrayPipelineState::Idle,
            PipelineState::Recording(_) => TrayPipelineState::Recording,
            PipelineState::Processing(_) => TrayPipelineState::Transcribing,
            PipelineState::AwaitingConfirmation => TrayPipelineState::Thinking,
        }
    }
}

/// Subset of pipeline state that the tray cares about (no payloads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayPipelineState {
    Idle,
    /// Microphone active (any of the recording-mode actions).
    Recording,
    /// ASR is running on the captured audio.
    Transcribing,
    /// An LLM stage is running (classifier, action launcher, or format stage 2).
    Thinking,
}

pub type StateHandle = Arc<Mutex<PipelineState>>;

pub fn new_state_handle() -> StateHandle {
    Arc::new(Mutex::new(PipelineState::Idle))
}

/// Spawn the coordinator. It owns a Recorder and consumes [`CoordinatorMsg`]s
/// from the multiplexed channel. The two hotkey listeners feed into this
/// channel via a small adapter (see `commands.rs::ensure_pipeline_started`).
///
/// State machine:
/// ```text
/// Idle
///   → (VoiceAtCursor, Pressed) → Recording(VoiceAtCursor)
///       → (VoiceAtCursor, Released) → Processing(VoiceAtCursor)
///           → transcribe + persist hidden + paste → Idle
///   → (LogCapture, Pressed) → Recording(LogCapture) [emit log_capture:recording_started]
///       → (LogCapture, Released) → Processing(LogCapture)
///           → transcribe + classify [emit log_capture:classification_ready]
///               → (auto-file shortcut: high confidence + existing project → persist + emit
///                  log_capture:auto_filed → Idle, skipping AwaitingConfirmation)
///               → AwaitingConfirmation
///                   → ConfirmLogCapture → persist visible → Idle
///                   → CancelLogCapture → discard → Idle
/// ```
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    mut rx: mpsc::UnboundedReceiver<CoordinatorMsg>,
    state: StateHandle,
    asr: Arc<AsrPipeline>,
    llm: Arc<Llm>,
    app: AppHandle<Wry>,
    db: Option<Db>,
    event_log_root: Option<PathBuf>,
    paused: Arc<AtomicBool>,
    on_state_change: impl Fn(TrayPipelineState) + 'static,
) {
    // NOTE: `Recorder` owns a `cpal::Stream`, which is `!Send`. We therefore
    // use `tokio::task::spawn_local`, which requires a `LocalSet` to be active
    // on the runtime. `commands::ensure_pipeline_started` sets up that
    // `LocalSet` on a dedicated thread.
    tokio::task::spawn_local(async move {
        let mut recorder = Recorder::new();
        // Frontmost-app snapshot taken at hotkey-press time. We restore it
        // before synthesizing Cmd+V so the paste lands in whichever app the
        // user was in when they started talking — even if our recording
        // overlay (a sibling window in this same process) momentarily stole
        // key-window status. Without this, dictating into our own chat input
        // fails because opening the overlay drops first-responder.
        let mut pending_context: Option<FocusContext> = None;
        // Held alongside `pending_context`. Non-Send, but the coordinator runs
        // on a `LocalSet` so that's fine. Restoring focus via AX element rather
        // than re-activating the app fixes "paste lands in previous field"
        // bugs in multi-window apps where NSApp picks the wrong NSWindow.
        let mut pending_focus_element: Option<FocusElement> = None;
        // Selection captured at EditSelection press time (text + capture method).
        let mut pending_selection: Option<focus::SelectionSnapshot> = None;

        // Set up audio level callback to feed the overlay waveform.
        {
            let app_for_levels = app.clone();
            recorder.set_level_callback(move |levels| {
                crate::overlay::emit_levels(&app_for_levels, &levels);
            });
        }

        while let Some(msg) = rx.recv().await {
            match msg {
                CoordinatorMsg::Hotkey(action, HotkeyEvent::Pressed) => {
                    if paused.load(Ordering::SeqCst) {
                        info!(?action, "hotkey Pressed dropped: paused via tray");
                        continue;
                    }
                    // Everything between here and `recorder.start()` returning
                    // is time the user hears the start cue but the microphone
                    // is not yet capturing — i.e. speech that never reaches the
                    // transcript. That silently eats the FIRST word, which is
                    // where the trigger word lives.
                    let pressed_at = std::time::Instant::now();

                    // Mid-recording upgrade: if LogCapture fires while
                    // VoiceAtCursor is already recording, promote the
                    // in-progress recording to LogCapture without restarting
                    // the recorder. The user pressed Option to start talking,
                    // then pressed / to signal "this should be a log entry".
                    if action == Action::LogCapture {
                        let upgraded = {
                            let mut s = state.lock().unwrap();
                            if *s == PipelineState::Recording(Action::VoiceAtCursor) {
                                *s = PipelineState::Recording(Action::LogCapture);
                                true
                            } else {
                                false
                            }
                        };
                        if upgraded {
                            info!("upgraded in-progress VoiceAtCursor recording to LogCapture");
                            let _ = app.emit("voice:recording_stopped", ());
                            let _ = app.emit("log_capture:recording_started", ());
                            crate::overlay::show_log_recording_overlay(&app);
                            continue;
                        }
                    }

                    if !transition_from_idle_to_recording(&state, action) {
                        warn!(?action, "ignored Pressed: not in Idle state");
                        continue;
                    }
                    // Snapshot the frontmost app *before* we touch any UI —
                    // showing the overlay can shift key-window status away
                    // from the user's text field.
                    pending_context = focus::capture_context();
                    pending_focus_element = pending_context
                        .as_ref()
                        .and_then(|c| focus::capture_focused_element(c.pid));
                    if let Some(s) = &pending_context {
                        info!(
                            pid = s.pid,
                            bundle = ?s.bundle_id,
                            app = ?s.app_name,
                            window = ?s.window_title,
                            content = ?s.content_title,
                            content_source = ?s.content_source,
                            ax_element_role = ?pending_focus_element.as_ref().and_then(|e| e.role().map(|r| r.to_string())),
                            "captured frontmost app + AX focus"
                        );
                    }
                    // EditSelection needs a live selection. Capture it now (the
                    // selection is active and the overlay hasn't stolen focus),
                    // and abort cleanly if there's nothing to edit — before we
                    // start the mic or show any recording UI.
                    if action == Action::EditSelection {
                        match focus::capture_selection(pending_focus_element.as_ref()) {
                            Some(sel) if crate::llm::edit::within_length_limit(&sel.text) => {
                                info!(chars = sel.text.len(), method = ?sel.method, "edit selection captured");
                                pending_selection = Some(sel);
                            }
                            Some(sel) => {
                                warn!(chars = sel.text.len(), "edit selection too long; aborting");
                                notify_edit_failure(&app, "Selection too long to edit (max ~1000 words).");
                                pending_context = None;
                                pending_focus_element = None;
                                force_state(&state, PipelineState::Idle);
                                on_state_change(TrayPipelineState::Idle);
                                continue;
                            }
                            None => {
                                info!("edit selection: nothing selected; showing hint");
                                feedback::play(Sfx::Stop);
                                let _ = app.emit("edit:hint", "Select text first, then hold the Edit hotkey.");
                                pending_context = None;
                                pending_focus_element = None;
                                force_state(&state, PipelineState::Idle);
                                on_state_change(TrayPipelineState::Idle);
                                continue;
                            }
                        }
                    }
                    on_state_change(TrayPipelineState::Recording);
                    crate::audio::mute::on_recording_start();
                    feedback::play(Sfx::Start);
                    match action {
                        Action::VoiceAtCursor => crate::overlay::show_recording_overlay(&app),
                        Action::ActionCommand => crate::overlay::show_action_recording_overlay(&app),
                        Action::LogCapture => crate::overlay::show_log_recording_overlay(&app),
                        Action::EditSelection => crate::overlay::show_action_recording_overlay(&app),
                        _ => {}
                    }
                    if matches!(action, Action::LogCapture) {
                        let _ = app.emit("log_capture:recording_started", ());
                    }
                    // Apply the user's preferred input device (if any) before
                    // each start. Reading it fresh each time means a settings
                    // change takes effect on the next press without a restart.
                    let preferred = app
                        .try_state::<crate::commands::AppState>()
                        .and_then(|s| s.settings.preferred_input_device());
                    recorder.set_preferred_device(preferred.clone());
                    if let Err(e) = recorder.start() {
                        error!(?e, ?action, "failed to start recorder; returning to Idle");
                        crate::overlay::hide_recording_overlay(&app);
                        force_state(&state, PipelineState::Idle);
                        pending_selection = None;
                        on_state_change(TrayPipelineState::Idle);
                        if matches!(action, Action::LogCapture) {
                            let _ = app.emit("log_capture:cancelled", ());
                        }
                        notify_recorder_failure(&app, &e, preferred.as_deref());
                    } else {
                        info!(
                            target: "trigger",
                            ?action,
                            press_to_capture_ms = pressed_at.elapsed().as_millis() as u64,
                            "recorder live"
                        );
                        if matches!(action, Action::VoiceAtCursor) {
                            let _ = app.emit("voice:recording_started", ());
                        }
                        // Recording started — pre-load the ASR engine in the
                        // background so it's warm by the time the user releases.
                        asr.warm_up();
                    }
                }
                CoordinatorMsg::Hotkey(action, HotkeyEvent::Released) => {
                    if paused.load(Ordering::SeqCst) {
                        // Drop the released event too — keep state consistent.
                        info!(?action, "hotkey Released dropped: paused via tray");
                        continue;
                    }
                    if !transition_from_recording_to_processing(&state, action) {
                        warn!(?action, "ignored Released: not Recording for this action");
                        continue;
                    }
                    if matches!(action, Action::VoiceAtCursor) {
                        let _ = app.emit("voice:recording_stopped", ());
                    }
                    on_state_change(TrayPipelineState::Transcribing);
                    crate::audio::mute::on_recording_stop();
                    feedback::play(Sfx::Stop);
                    crate::overlay::show_transcribing_overlay(&app);
                    let channels = recorder.channels();
                    let stop_result = recorder.stop();
                    match stop_result {
                        Ok((samples, sr)) => {
                            info!(
                                samples = samples.len(),
                                sample_rate = sr,
                                channels,
                                ?action,
                                "transcribing"
                            );
                            match asr.transcribe(samples, sr, channels.max(1)).await {
                                Ok(text) if text.is_empty() => {
                                    warn!("transcription produced empty text; nothing to do");
                                    if matches!(action, Action::LogCapture) {
                                        let _ = app.emit(
                                            "log_capture:classification_ready",
                                            serde_json::json!({
                                                "transcript": "",
                                                "classification": null,
                                                "error": "empty transcription",
                                            }),
                                        );
                                    }
                                    crate::overlay::hide_recording_overlay(&app);
                                    force_state(&state, PipelineState::Idle);
                                    pending_selection = None;
                                    on_state_change(TrayPipelineState::Idle);
                                }
                                Ok(text) => {
                                    if matches!(action, Action::EditSelection) {
                                        run_edit_selection(
                                            &app,
                                            &llm,
                                            &text,
                                            pending_selection.take(),
                                            pending_focus_element.take(),
                                            pending_context.take(),
                                        )
                                        .await;
                                        crate::overlay::hide_recording_overlay_now(&app);
                                        force_state(&state, PipelineState::Idle);
                                        on_state_change(TrayPipelineState::Idle);
                                        continue;
                                    }
                                    let text = postprocess_with_settings(&app, text);
                                    let text = match try_intercept_action(&app, &llm, &text, action).await {
                                        InterceptOutcome::Consumed => {
                                            crate::overlay::hide_recording_overlay_now(&app);
                                            force_state(&state, PipelineState::Idle);
                                            on_state_change(TrayPipelineState::Idle);
                                            continue;
                                        }
                                        InterceptOutcome::Reformatted(s) => s,
                                        InterceptOutcome::Passthrough => text,
                                    };
                                    match action {
                                    Action::VoiceAtCursor | Action::ActionCommand => {
                                        // Phase 1 behavior: persist hidden + paste.
                                        persist_capture(
                                            &text,
                                            db.as_ref(),
                                            event_log_root.as_deref(),
                                            &app,
                                            pending_context.as_ref().and_then(serialise_context),
                                        );
                                        // Hide the overlay synchronously so it
                                        // can't interfere with focus routing.
                                        crate::overlay::hide_recording_overlay_now(&app);
                                        // Restore focus surgically: prefer the
                                        // captured AX element (lands in the
                                        // exact field), fall back to app
                                        // activation. Skip activation when the
                                        // captured app is already frontmost
                                        // (e.g. dictating into Echo Scribe
                                        // itself) — re-activating cycles key
                                        // windows and is the regression source.
                                        if let Some(snap) = pending_context.take() {
                                            let element = pending_focus_element.take();
                                            let outcome = focus::restore_focus(&snap, element.as_ref());
                                            info!(
                                                pid = snap.pid,
                                                same_app = outcome.same_app,
                                                activated = outcome.activated_app,
                                                ax_focused = outcome.ax_focused,
                                                ax_error = ?outcome.ax_error,
                                                element_captured = outcome.element_captured,
                                                ax_role = ?outcome.element_role,
                                                frontmost_before = ?outcome.frontmost_pid_before,
                                                "focus restored before paste"
                                            );
                                            let _ = app.emit("voice:paste_pending", ());
                                            // Settle delay. Same-app skips the
                                            // app-activation round-trip so we
                                            // can wait less; cross-app needs
                                            // WindowServer time to route key.
                                            let settle_ms = if outcome.same_app { 60 } else { 250 };
                                            std::thread::sleep(std::time::Duration::from_millis(settle_ms));
                                        }
                                        info!(chars = text.len(), "pasting transcription");
                                        if let Err(e) = paste_at_cursor(&text) {
                                            error!(?e, "paste failed");
                                            let _ =
                                                app.emit("asr:error", format!("Paste failed: {e}"));
                                        }
                                        force_state(&state, PipelineState::Idle);
                                        on_state_change(TrayPipelineState::Idle);
                                    }
                                    Action::LogCapture => {
                                        crate::overlay::show_processing_overlay(&app, "Filing note…");
                                        on_state_change(TrayPipelineState::Thinking);
                                        let cls = run_classifier(&llm, &text, db.as_ref(), pending_context.as_ref().map(|c| c as &_)).await;
                                        feedback::play(Sfx::Ready);
                                        crate::overlay::hide_recording_overlay(&app);

                                        let enabled = app
                                            .try_state::<crate::commands::AppState>()
                                            .map(|s| s.settings.auto_file_enabled())
                                            .unwrap_or(true);

                                        if enabled {
                                            // Auto-file: the user never wants a confirm popup. If the
                                            // classifier errored (no model / parse failure), fall back to
                                            // a plain note with no project so the capture is never lost.
                                            let c = cls.unwrap_or_else(|e| {
                                                warn!(?e, "classify failed; filing capture as a plain note");
                                                Classification {
                                                    kind: crate::db::items::ItemKind::Note,
                                                    project_id: None,
                                                    new_project_name: None,
                                                    tags: Vec::new(),
                                                    deadline_iso: None,
                                                    confidence: 0.0,
                                                }
                                            });

                                            let project_name: Option<String> = c
                                                .project_id
                                                .as_deref()
                                                .and_then(|pid| {
                                                    db.as_ref().and_then(|db| {
                                                        db.with_conn(|conn| {
                                                            crate::db::projects::get_project(conn, pid)
                                                        })
                                                        .ok()
                                                        .flatten()
                                                        .map(|p| p.name)
                                                    })
                                                })
                                                .or_else(|| c.new_project_name.clone());

                                            let res = persist_log_capture(
                                                &text,
                                                c.kind,
                                                c.project_id.clone(),
                                                c.new_project_name.clone(),
                                                c.tags.clone(),
                                                c.deadline_iso.clone(),
                                                Some(c.confidence),
                                                Some("ai"),
                                                pending_context.take().as_ref().and_then(serialise_context),
                                                db.as_ref(),
                                                event_log_root.as_deref(),
                                            );
                                            pending_focus_element = None;
                                            match res {
                                                Ok(item_id) => {
                                                    info!(item_id = %item_id, "auto-saved log capture");
                                                    let _ = app.emit("item:created", ());
                                                    notify_auto_filed(
                                                        &app,
                                                        &item_id,
                                                        project_name.as_deref(),
                                                        c.kind,
                                                        &text,
                                                        c.confidence,
                                                    );
                                                    try_export_persisted_item(&app, db.as_ref(), &item_id);
                                                }
                                                Err(e) => {
                                                    error!(?e, "log capture auto-save failed");
                                                    let _ = app.emit("asr:error", format!("Save failed: {e}"));
                                                }
                                            }
                                            force_state(&state, PipelineState::Idle);
                                            on_state_change(TrayPipelineState::Idle);
                                        } else {
                                            // Review mode (auto-file disabled in Settings): show the
                                            // confirm overlay so the user can edit before saving.
                                            let _ = app.emit(
                                                "log_capture:classification_ready",
                                                serde_json::json!({
                                                    "transcript": text,
                                                    "classification": cls.as_ref().ok(),
                                                }),
                                            );
                                            force_state(&state, PipelineState::AwaitingConfirmation);
                                            on_state_change(TrayPipelineState::Thinking);
                                        }
                                    }
                                    Action::EditSelection => {
                                        // Handled above via the early `continue`;
                                        // this arm keeps the match exhaustive.
                                        crate::overlay::hide_recording_overlay(&app);
                                        force_state(&state, PipelineState::Idle);
                                        on_state_change(TrayPipelineState::Idle);
                                    }
                                    Action::Cancel => {
                                        crate::overlay::hide_recording_overlay(&app);
                                        force_state(&state, PipelineState::Idle);
                                        on_state_change(TrayPipelineState::Idle);
                                    }
                                    }
                                }
                                Err(e) => {
                                    error!(?e, "transcription failed");
                                    let _ = app.emit(
                                        "asr:error",
                                        format!("Transcription failed: {e}"),
                                    );
                                    if matches!(action, Action::LogCapture) {
                                        let _ = app.emit(
                                            "log_capture:classification_ready",
                                            serde_json::json!({
                                                "transcript": "",
                                                "classification": null,
                                                "error": e.to_string(),
                                            }),
                                        );
                                    }
                                    crate::overlay::hide_recording_overlay(&app);
                                    force_state(&state, PipelineState::Idle);
                                    pending_selection = None;
                                    on_state_change(TrayPipelineState::Idle);
                                }
                            }
                        }
                        Err(e) => {
                            error!(?e, "recorder.stop failed");
                            let _ = app.emit("asr:error", format!("Recorder error: {e}"));
                            if matches!(action, Action::LogCapture) {
                                let _ = app.emit("log_capture:cancelled", ());
                            }
                            crate::overlay::hide_recording_overlay(&app);
                            force_state(&state, PipelineState::Idle);
                            pending_selection = None;
                            on_state_change(TrayPipelineState::Idle);
                        }
                    }
                }
                CoordinatorMsg::ConfirmLogCapture {
                    content,
                    kind,
                    project_id,
                    new_project_name,
                    tags,
                    deadline_iso,
                    reply,
                } => {
                    let res = persist_log_capture(
                        &content,
                        kind,
                        project_id,
                        new_project_name,
                        tags,
                        deadline_iso,
                        None,
                        Some("user"),
                        pending_context.take().as_ref().and_then(serialise_context),
                        db.as_ref(),
                        event_log_root.as_deref(),
                    );
                    pending_focus_element = None;
                    match &res {
                        Ok(id) => {
                            info!(item_id = %id, "log capture persisted");
                            let _ = app.emit("item:created", ());
                            try_export_persisted_item(&app, db.as_ref(), id);
                            if kind == crate::db::items::ItemKind::Note {
                                use tauri_plugin_notification::NotificationExt;
                                let preview = preview_first_chars(&content, 120);
                                if let Err(e) = app
                                    .notification()
                                    .builder()
                                    .title("Note Saved")
                                    .body(preview)
                                    .show()
                                {
                                    warn!(?e, "failed to show note-saved OS notification");
                                }
                            }
                        }
                        Err(e) => error!(?e, "log capture persistence failed"),
                    }
                    let _ = reply.send(res);
                    force_state(&state, PipelineState::Idle);
                    on_state_change(TrayPipelineState::Idle);
                }
                CoordinatorMsg::CancelLogCapture => {
                    pending_context = None;
                    pending_focus_element = None;
                    let _ = app.emit("log_capture:cancelled", ());
                    force_state(&state, PipelineState::Idle);
                    on_state_change(TrayPipelineState::Idle);
                }
            }
        }
    });
}

/// Look up the just-persisted item and, if its project has an export folder
/// and the confidence clears the user's threshold, write it as markdown.
/// Tolerates missing settings/state (e.g. during early boot) by silently
/// skipping.
fn try_export_persisted_item(app: &AppHandle<Wry>, db: Option<&Db>, item_id: &str) {
    let Some(db) = db else { return };
    let threshold = app
        .try_state::<crate::commands::AppState>()
        .map(|s| s.settings.export_confidence_threshold())
        .unwrap_or(crate::settings::DEFAULT_EXPORT_CONFIDENCE_THRESHOLD);
    let item = match db.with_conn(|c| crate::db::items::get_item(c, item_id)) {
        Ok(Some(it)) => it,
        Ok(None) => return,
        Err(e) => {
            warn!(target: "export", error = %e, "lookup item for export failed");
            return;
        }
    };
    crate::export::try_export_item(db, &item, threshold);
}

fn transition_from_idle_to_recording(state: &StateHandle, action: Action) -> bool {
    let mut s = match state.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if matches!(*s, PipelineState::Idle) {
        *s = PipelineState::Recording(action);
        true
    } else {
        false
    }
}

fn transition_from_recording_to_processing(state: &StateHandle, action: Action) -> bool {
    let mut s = match state.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if let PipelineState::Recording(current) = &*s {
        if *current == action {
            *s = PipelineState::Processing(action);
            return true;
        }
    }
    false
}

fn force_state(state: &StateHandle, to: PipelineState) {
    if let Ok(mut s) = state.lock() {
        *s = to;
    }
}

/// Serialise a `FocusContext` to a compact JSON string for storage.
fn serialise_context(ctx: &crate::input::focus::FocusContext) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "app_name":          ctx.app_name,
        "window_title":      ctx.window_title,
        "browser_url":       ctx.browser_url,
        "browser_tab_title": ctx.browser_tab_title,
        "content_title":     ctx.content_title,
        "content_url":       ctx.content_url,
        "content_source":    ctx.content_source,
        "bundle_id":         ctx.bundle_id,
    }))
    .ok()
}

/// Insert an item row + append a `voice.captured` event to the disk log.
/// Best-effort, see Phase 1 docs.
fn persist_capture(
    text: &str,
    db: Option<&Db>,
    event_log_root: Option<&std::path::Path>,
    app: &AppHandle<Wry>,
    capture_context: Option<String>,
) {
    let id = ulid::Ulid::new().to_string();
    let now = chrono_now_iso();

    if let Some(db) = db {
        let item = Item {
            id: id.clone(),
            content: text.to_string(),
            source: ItemSource::VoiceAtCursor,
            kind: Some(crate::db::items::ItemKind::Transcription),
            project_id: None,
            captured_at: now.clone(),
            created_at: now.clone(),
            deleted_at: None,
            confidence: None,
            classified_by: None,
            capture_context,
        };
        let res = db.with_conn(|c| crate::db::items::insert_item(c, &item));
        match res {
            Ok(_) => {
                let id_for_event = id.clone();
                let _ = db.with_conn(|c| {
                    crate::db::events::insert_event(c, &id_for_event, "created", Some("via voice_at_cursor"))
                });
                let id_for_tag = id.clone();
                let now_for_tag = now.clone();
                let _ = db.with_conn(|c| {
                    crate::db::project_tag_jobs::enqueue(c, &id_for_tag, &now_for_tag)
                });
                let _ = app.emit("item:created", ());
            }
            Err(e) => {
                error!(?e, "failed to persist item");
                let _ = app.emit("asr:error", format!("Persist failed: {e}"));
            }
        }
    }

    if let Some(root) = event_log_root {
        let preview = preview_first_chars(text, 200);
        let envelope = EventEnvelope {
            id: id.clone(),
            event_type: "voice.captured".to_string(),
            created_at: now,
            payload: serde_json::json!({
                "item_id": id,
                "preview": preview,
                "char_count": text.chars().count(),
            }),
        };
        if let Err(e) = event_log::append_event(root, &envelope) {
            error!(?e, "failed to append event log entry");
            let _ = app.emit("asr:error", format!("Persist failed: {e}"));
        }
    }
}

fn preview_first_chars(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    for c in text.chars() {
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

/// Apply the user-configured filler-removal + custom-word passes. Looks up
/// the latest settings via the managed `AppState` so toggles take effect on
/// the next transcription without restarting the pipeline.
fn postprocess_with_settings(app: &AppHandle<Wry>, text: String) -> String {
    let state = match app.try_state::<crate::commands::AppState>() {
        Some(s) => s,
        None => return text,
    };
    let fillers = if state.settings.filler_removal_enabled() {
        state.settings.filler_words()
    } else {
        Vec::new()
    };
    let custom = state.settings.custom_words();
    if fillers.is_empty() && custom.is_empty() {
        return text;
    }
    crate::asr::postprocess::postprocess(&text, &fillers, &custom)
}

/// Run the classifier with light context (existing projects + last 5 items).
async fn run_classifier(
    llm: &Llm,
    transcript: &str,
    db: Option<&Db>,
    focus: Option<&crate::input::focus::FocusContext>,
) -> Result<Classification, classifier::ClassifierError> {
    if !llm.ready() {
        return Err(classifier::ClassifierError::Parse(
            "no llm model is active".into(),
        ));
    }
    let (projects, recents) = match db {
        Some(db) => db
            .with_conn(|c| {
                let projects = crate::db::projects::list_projects(c, false)?;
                let recents = crate::db::items::list_items(c, None, None, 5, 0)?;
                Ok::<_, crate::db::DbError>((projects, recents))
            })
            .unwrap_or_else(|e| {
                warn!(?e, "failed to load classifier context; using empty");
                (Vec::new(), Vec::new())
            }),
        None => (Vec::new(), Vec::new()),
    };
    let now = crate::db::items::chrono_now_iso();
    let dow = classifier::dow_from_iso(&now);
    classifier::classify(llm, transcript, &projects, &recents, &now, dow, focus).await
}

/// Outcome of running the action launcher against a fresh transcription.
/// Lets the caller decide whether to drop the pipeline, paste an altered
/// string, or paste the original text unchanged.
pub enum InterceptOutcome {
    /// The launcher executed an action (launch_app / draft_email / counters
    /// / etc.). The caller should hide the overlay and return to Idle.
    Consumed,
    /// The LLM matched a format template; the returned string is the
    /// reformatted body that should be pasted at the user's cursor in
    /// place of the raw transcription.
    Reformatted(String),
    /// No intercept fired. Caller should use the original transcription.
    Passthrough,
}

/// Try to detect and execute a system action from voice dictation.
/// Returns an [`InterceptOutcome`] describing what the caller should do next.
async fn try_intercept_action(
    app: &tauri::AppHandle,
    llm: &crate::llm::Llm,
    text: &str,
    action: Action,
) -> InterceptOutcome {
    let (enabled, trigger_enabled, trigger_word, format_templates) = app
        .try_state::<crate::commands::AppState>()
        .map(|s| (
            s.settings.app_launcher_enabled(),
            s.settings.trigger_word_routing_enabled(),
            s.settings.action_trigger_word(),
            s.settings.format_templates(),
        ))
        .unwrap_or((true, true, "echo".to_string(), Vec::new()));

    if !enabled {
        return InterceptOutcome::Passthrough;
    }

    let stripped_text = if action == Action::ActionCommand {
        // Dedicated Action Hotkey: bypass trigger word prefix check entirely
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return InterceptOutcome::Passthrough;
        }
        trimmed.to_string()
    } else if action == Action::VoiceAtCursor {
        // Prefix-Based Routing
        if !trigger_enabled {
            // Option 2 fallback bypass: standard voice typing completely bypasses LLM
            info!(
                target: "trigger",
                "trigger word routing is disabled in settings; pasting as plain text"
            );
            return InterceptOutcome::Passthrough;
        }

        let text_trimmed = text.trim();
        let text_lower = text_trimmed.to_lowercase();
        let trigger_lower = trigger_word.trim().to_lowercase();

        let mut matched_trigger = None;
        if trigger_lower == "echo" {
            for trig in &["echo", "eco", "hecho", "ekko"] {
                if text_lower.starts_with(trig) {
                    matched_trigger = Some(trig.len());
                    break;
                }
            }
        } else {
            if text_lower.starts_with(&trigger_lower) {
                matched_trigger = Some(trigger_lower.len());
            }
        }

        match matched_trigger {
            Some(len) => {
                info!(
                    target: "trigger",
                    trigger = %trigger_lower,
                    "trigger word matched; routing dictation to the action launcher"
                );
                if text_lower.len() == len {
                    String::new()
                } else {
                    let remaining = &text_trimmed[len..];
                    let remaining_trimmed = remaining.trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
                    remaining_trimmed.to_string()
                }
            }
            None => {
                // No trigger prefix found: bypass the LLM entirely (instant paste!)
                //
                // Log what we actually saw. Without this, a miss is
                // indistinguishable from "the hotkey never fired" in the log,
                // and the two have completely different fixes: the trigger word
                // is spoken FIRST, so it is exactly the word most likely to be
                // clipped by microphone start-up latency. `head` is the prefix
                // we compared, so a log showing `head="open google."` proves the
                // word never reached the transcript rather than failing to match.
                info!(
                    target: "trigger",
                    trigger = %trigger_lower,
                    head = %text_lower.chars().take(24).collect::<String>(),
                    chars = text_trimmed.len(),
                    "no trigger word at start of dictation; pasting as plain text"
                );
                return InterceptOutcome::Passthrough;
            }
        }
    } else {
        // For other actions (e.g. LogCapture), we don't intercept action commands here
        return InterceptOutcome::Passthrough;
    };

    if stripped_text.is_empty() {
        debug!("Command text is empty; bypassing LLM");
        return InterceptOutcome::Passthrough;
    }

    crate::overlay::show_processing_overlay(app, "Processing…");
    bump_tray(app, TrayPipelineState::Thinking);

    info!(target: "format", "Checking dictation for action launcher intent: '{}'", stripped_text);
    match crate::llm::action_launcher::detect_action(llm, &stripped_text, &format_templates).await {
        Ok(cmd) => {
            if cmd.is_action && cmd.confidence >= 0.75 {
                info!(
                    action_type = ?cmd.action_type,
                    confidence = cmd.confidence,
                    "Detected voice action command"
                );

                // Format-text action: stage-2 reformat then hand back the
                // rewritten string for the caller to paste via the normal
                // focus-restore path. On any failure here we fall back to
                // pasting the raw transcription so dictation is never lost.
                if cmd.action_type.as_deref() == Some("format_text") {
                    let body = cmd
                        .format_body
                        .clone()
                        .unwrap_or_else(|| stripped_text.clone());
                    let body_trimmed = body.trim();
                    if body_trimmed.is_empty() {
                        warn!(target: "format", "format_text matched but body is empty; falling back to raw paste");
                        return InterceptOutcome::Passthrough;
                    }
                    let template = cmd
                        .format_id
                        .as_deref()
                        .and_then(|id| format_templates.iter().find(|t| t.id == id))
                        .cloned();
                    let Some(template) = template else {
                        warn!(
                            target: "format",
                            format_id = ?cmd.format_id,
                            "format_text matched unknown template id; falling back to raw paste"
                        );
                        notify_format_failure(app, "Format template not found. Pasting raw transcription instead.");
                        return InterceptOutcome::Passthrough;
                    };
                    info!(
                        target: "format",
                        template_id = %template.id,
                        body_chars = body_trimmed.len(),
                        "running stage-2 format reformat"
                    );
                    crate::overlay::show_processing_overlay(app, "Formatting…");
                    match crate::llm::action_launcher::format_text(llm, &template, body_trimmed).await {
                        Ok(formatted) if !formatted.is_empty() => {
                            info!(
                                target: "format",
                                template_id = %template.id,
                                output_chars = formatted.len(),
                                "format_text produced output"
                            );
                            feedback::play(Sfx::Ready);
                            if let Some(s) = app.try_state::<crate::commands::AppState>() {
                                let _ = s.settings.increment_action_counter();
                            }
                            return InterceptOutcome::Reformatted(formatted);
                        }
                        Ok(_) => {
                            warn!(target: "format", "format_text produced empty output; falling back to raw paste");
                            notify_format_failure(app, "Format produced no output. Pasting raw transcription instead.");
                            return InterceptOutcome::Passthrough;
                        }
                        Err(e) => {
                            error!(target: "format", error = %e, "format_text stage-2 failed; falling back to raw paste");
                            notify_format_failure(app, "Format failed. Pasting raw transcription instead. See logs for details.");
                            return InterceptOutcome::Passthrough;
                        }
                    }
                }

                match crate::llm::action_launcher::execute_action(app, &cmd) {
                    Ok(msg) => {
                        info!(msg, "Voice action executed successfully");
                        feedback::play(Sfx::Ready);
                        let _ = app.emit("action:executed", serde_json::json!({
                            "message": msg,
                            "command": cmd,
                        }));

                        // Fire a macOS system notification
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app.notification()
                            .builder()
                            .title("Echo Scribe Action")
                            .body(&msg)
                            .show();

                        // Increment action counter
                        if let Some(s) = app.try_state::<crate::commands::AppState>() {
                            let _ = s.settings.increment_action_counter();
                        }

                        return InterceptOutcome::Consumed;
                    }
                    Err(e) => {
                        error!(error = %e, "Voice action execution failed; falling back to standard pipeline");
                    }
                }
            } else {
                debug!(
                    confidence = cmd.confidence,
                    "No high-confidence action intent found; continuing standard pipeline"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "Action classification failed; continuing standard pipeline");
        }
    }
    InterceptOutcome::Passthrough
}

/// Orchestrate a voice edit of the captured selection: run the local LLM edit
/// pass, sanitize the output, and apply it — via AX write-back when the
/// selection was captured through AX, otherwise by restoring focus and pasting
/// over the still-active selection. Every failure leaves the text untouched and
/// surfaces a friendly message.
async fn run_edit_selection(
    app: &AppHandle<Wry>,
    llm: &crate::llm::Llm,
    instruction: &str,
    selection: Option<crate::input::focus::SelectionSnapshot>,
    element: Option<FocusElement>,
    ctx: Option<FocusContext>,
) {
    let Some(selection) = selection else {
        warn!(target: "edit", "no captured selection at apply time");
        notify_edit_failure(app, "Nothing was selected — text left unchanged.");
        return;
    };
    let instruction = instruction.trim();
    if instruction.is_empty() {
        info!(target: "edit", "empty instruction; aborting");
        notify_edit_failure(app, "Didn't catch an instruction — text left unchanged.");
        return;
    }
    if !llm.ready() {
        warn!(target: "edit", "no LLM model active");
        notify_edit_failure(app, "Load a language model in Settings to use Edit Selection.");
        return;
    }

    crate::overlay::show_processing_overlay(app, "Editing selection…");
    bump_tray(app, TrayPipelineState::Thinking);

    let raw = match crate::llm::edit::run(llm, instruction, &selection.text).await {
        Ok(r) => r,
        Err(e) => {
            error!(target: "edit", error = %e, "edit LLM generation failed");
            notify_edit_failure(app, "Couldn't apply that edit — text left unchanged.");
            return;
        }
    };
    let Some(result) = crate::llm::edit::sanitize_edit_output(&raw, &selection.text) else {
        warn!(target: "edit", raw = %raw, "edit output rejected by sanitizer; leaving text unchanged");
        notify_edit_failure(app, "Couldn't apply that edit — text left unchanged.");
        return;
    };

    // Apply: AX write-back first (clean, no keystrokes), else Cmd+V paste.
    let applied_via_ax = selection.method == crate::input::focus::SelectionMethod::Ax
        && element
            .as_ref()
            .map(|el| el.replace_selected_text(&result) == 0)
            .unwrap_or(false);

    if !applied_via_ax {
        if let Some(ctx) = ctx.as_ref() {
            let outcome = crate::input::focus::restore_focus(ctx, element.as_ref());
            info!(target: "edit", same_app = outcome.same_app, activated = outcome.activated_app, "restored focus before edit paste");
            let settle_ms = if outcome.same_app { 60 } else { 250 };
            std::thread::sleep(std::time::Duration::from_millis(settle_ms));
        }
        if let Err(e) = crate::input::paste::paste_at_cursor(&result) {
            error!(target: "edit", error = %e, "edit paste failed");
            let _ = app.emit("asr:error", format!("Paste failed: {e}"));
            return;
        }
    }

    feedback::play(Sfx::Ready);
    info!(
        target: "edit",
        method = ?selection.method,
        via_ax = applied_via_ax,
        out_chars = result.len(),
        "applied selection edit"
    );
}

/// Friendly surface for an edit-selection failure: in-app event + OS
/// notification when the main window is hidden. Raw detail stays in the log.
fn notify_edit_failure(app: &AppHandle<Wry>, friendly: &str) {
    let _ = app.emit("edit:failed", friendly);
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title("Echo Scribe Edit")
        .body(friendly)
        .show();
}

/// Bump the tray icon to the given state via the managed `AppState.tray`
/// handle. Used by helpers (e.g. `try_intercept_action`) that don't carry
/// the coordinator's `on_state_change` closure but still need to reflect
/// LLM-stage transitions in the menu bar.
fn bump_tray(app: &tauri::AppHandle, state: TrayPipelineState) {
    if let Some(s) = app.try_state::<crate::commands::AppState>() {
        if let Ok(tray) = s.tray.lock() {
            tray.set_state(state);
        }
    }
}

/// Best-effort UI surface for a format-text failure: emits a Tauri event the
/// frontend can toast on, plus an OS notification so the user notices when no
/// Echo Scribe window is visible. The raw error stays in the daily log.
fn notify_format_failure(app: &tauri::AppHandle, friendly: &str) {
    let _ = app.emit("format:failed", friendly);
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title("Echo Scribe Format")
        .body(friendly)
        .show();
}

/// Surface a recorder-start failure to the user: emits a Tauri event for any
/// listening UI and fires an OS notification so the user notices even when
/// no Echo Scribe window is visible. Best-effort — both are fire-and-forget.
fn notify_recorder_failure(
    app: &AppHandle<Wry>,
    err: &RecorderError,
    preferred: Option<&str>,
) {
    use tauri_plugin_notification::NotificationExt;

    let kind = err.kind();
    let payload = serde_json::json!({
        "kind": kind,
        "error": err.to_string(),
        "preferred_device": preferred,
    });
    let _ = app.emit("recorder:start_failed", payload);

    let (title, body) = match err {
        RecorderError::PreferredDeviceMissing(name) => (
            "Microphone unavailable".to_string(),
            format!("Saved mic '{name}' isn't connected. Pick another in Settings → Voice."),
        ),
        RecorderError::NoDevice => (
            "No microphone detected".to_string(),
            "macOS reports no input device. Connect a mic and try again.".to_string(),
        ),
        RecorderError::BuildStream(msg) | RecorderError::StartStream(msg) => (
            "Microphone unavailable".to_string(),
            format!("Couldn't open the input device: {msg}. Try a different mic in Settings → Voice."),
        ),
        RecorderError::NotRunning => return, // shouldn't happen here; nothing to notify
    };
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        warn!(?e, "failed to show recorder-failure OS notification");
    }
}

/// Emit the in-app toast event AND, when the main window is not visible,
/// fire an OS notification so the user sees what was filed. When
/// `project_name` is `None` (lookup failed because the DB is unavailable or
/// the project was deleted), the in-app event still fires with `"Unknown"`
/// in its payload but the OS notification is skipped.
fn notify_auto_filed(
    app: &AppHandle<Wry>,
    item_id: &str,
    project_name: Option<&str>,
    kind: crate::db::items::ItemKind,
    content: &str,
    confidence: f32,
) {
    let preview = preview_first_chars(content, 120);
    let display_name = project_name.unwrap_or("Unknown");
    let payload = serde_json::json!({
        "item_id": item_id,
        "project_name": display_name,
        "kind": kind.as_str(),
        "preview": preview,
        "confidence": confidence,
    });
    let _ = app.emit("log_capture:auto_filed", payload);

    // If the main window isn't visible, the user won't see the in-app toast.
    // Fall back to an OS notification (no Undo button — the user can open
    // the app to find/edit/delete the item).
    let main_visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if !main_visible {
        let Some(name) = project_name else {
            warn!(
                "auto-filed item has no resolvable project name; skipping OS notification"
            );
            return;
        };
        use tauri_plugin_notification::NotificationExt;
        let kind_label = match kind {
            crate::db::items::ItemKind::Task => "Task",
            crate::db::items::ItemKind::Note => "Note",
            crate::db::items::ItemKind::Transcription => "Transcription",
        };
        let title = format!("Filed to {name}");
        let body = format!("{kind_label}: {preview}");
        if let Err(e) = app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            warn!(?e, "failed to show OS notification for auto-file");
        }
    }
}

/// Persist a confirmed LogCapture as a Visible item, plus tags/task row, plus
/// a `log.captured` event-log envelope. Returns the new item id on success.
#[allow(clippy::too_many_arguments)]
fn persist_log_capture(
    content: &str,
    kind: crate::db::items::ItemKind,
    project_id: Option<String>,
    new_project_name: Option<String>,
    tags: Vec<String>,
    deadline_iso: Option<String>,
    confidence: Option<f32>,
    classified_by: Option<&str>,
    capture_context: Option<String>,
    db: Option<&Db>,
    event_log_root: Option<&std::path::Path>,
) -> Result<String, String> {
    let id = ulid::Ulid::new().to_string();
    let now = chrono_now_iso();

    let db = db.ok_or_else(|| "database not available".to_string())?;

    // Resolve project id: if `new_project_name` is set, create the project
    // and use its id. Otherwise use the supplied (validated) project_id, if
    // any.
    let final_project_id: Option<String> = if let Some(name) = new_project_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // Get-or-create: reuse an existing project with the same name
        // (case-insensitive) instead of blindly inserting, which would hit the
        // UNIQUE(name) constraint when the name already exists.
        let existing = db
            .with_conn(|c| crate::db::projects::get_project_by_name(c, name))
            .map_err(|e| format!("lookup project: {e}"))?;
        if let Some(p) = existing {
            info!(project_id = %p.id, name = %p.name, "reused existing project for capture");
            Some(p.id)
        } else {
            let pid = ulid::Ulid::new().to_string();
            let proj = crate::db::projects::Project {
                id: pid.clone(),
                name: name.to_string(),
                created_at: now.clone(),
                archived_at: None,
                ..Default::default()
            };
            db.with_conn(|c| crate::db::projects::insert_project(c, &proj))
                .map_err(|e| format!("create project: {e}"))?;
            info!(project_id = %pid, name = %name, "created new project for capture");
            Some(pid)
        }
    } else {
        project_id
    };

    let item = Item {
        id: id.clone(),
        content: content.to_string(),
        source: ItemSource::LogCapture,
        kind: Some(kind),
        project_id: final_project_id.clone(),
        captured_at: now.clone(),
        created_at: now.clone(),
        deleted_at: None,
        confidence,
        classified_by: classified_by.map(|s| s.to_string()),
        capture_context,
    };

    db.with_conn(|c| crate::db::items::insert_item(c, &item))
        .map_err(|e| format!("insert item: {e}"))?;

    // Unclassified captures join the auto-tagging queue.
    if final_project_id.is_none() {
        let id_for_tag = id.clone();
        let now_for_tag = now.clone();
        let _ = db.with_conn(move |c| {
            crate::db::project_tag_jobs::enqueue(c, &id_for_tag, &now_for_tag)
        });
    }

    // Record lifecycle event.
    {
        let detail = format!("via log_capture as {}", kind.as_str());
        let id_for_event = id.clone();
        let _ = db.with_conn(move |c| {
            crate::db::events::insert_event(c, &id_for_event, "created", Some(&detail))
        });
        if let Some(ref pid) = final_project_id {
            let id_for_event = id.clone();
            let pid_clone = pid.clone();
            let _ = db.with_conn(move |c| {
                crate::db::events::insert_event(
                    c,
                    &id_for_event,
                    "project_assigned",
                    Some(&pid_clone),
                )
            });
        }
    }

    // Tags.
    if !tags.is_empty() {
        let tags_clone = tags.clone();
        let id_clone = id.clone();
        db.with_conn(move |c| {
            for t in &tags_clone {
                c.execute(
                    "INSERT OR IGNORE INTO item_tags(item_id, tag) VALUES(?1, ?2)",
                    rusqlite::params![id_clone, t],
                )?;
            }
            Ok(())
        })
        .map_err(|e| format!("insert tags: {e}"))?;
    }

    // Task row, only for tasks.
    if matches!(kind, crate::db::items::ItemKind::Task) {
        let task = crate::db::tasks::Task {
            item_id: id.clone(),
            deadline: deadline_iso.clone(),
            completed_at: None,
        };
        db.with_conn(|c| crate::db::tasks::upsert_task(c, &task))
            .map_err(|e| format!("upsert task: {e}"))?;
    }

    // Event log.
    if let Some(root) = event_log_root {
        let preview = preview_first_chars(content, 200);
        let envelope = EventEnvelope {
            id: id.clone(),
            event_type: "log.captured".to_string(),
            created_at: now,
            payload: serde_json::json!({
                "item_id": id,
                "kind": kind.as_str(),
                "project_id": final_project_id,
                "tags": tags,
                "deadline_iso": deadline_iso,
                "preview": preview,
                "char_count": content.chars().count(),
            }),
        };
        if let Err(e) = event_log::append_event(root, &envelope) {
            // Don't roll back the DB row — the event log is additive history.
            warn!(?e, "failed to append log.captured event");
        }
    }

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_to_recording_transition() {
        let s = new_state_handle();
        assert!(transition_from_idle_to_recording(&s, Action::VoiceAtCursor));
        assert_eq!(*s.lock().unwrap(), PipelineState::Recording(Action::VoiceAtCursor));
    }

    #[test]
    fn idle_to_recording_rejects_when_busy() {
        let s = new_state_handle();
        force_state(&s, PipelineState::Recording(Action::VoiceAtCursor));
        assert!(!transition_from_idle_to_recording(&s, Action::LogCapture));
    }

    #[test]
    fn recording_to_processing_only_for_matching_action() {
        let s = new_state_handle();
        force_state(&s, PipelineState::Recording(Action::VoiceAtCursor));
        // Mismatched action is rejected.
        assert!(!transition_from_recording_to_processing(&s, Action::LogCapture));
        // Matching action succeeds.
        assert!(transition_from_recording_to_processing(&s, Action::VoiceAtCursor));
        assert_eq!(
            *s.lock().unwrap(),
            PipelineState::Processing(Action::VoiceAtCursor)
        );
    }

    #[test]
    fn force_state_overrides_unconditionally() {
        let s = new_state_handle();
        force_state(&s, PipelineState::Processing(Action::LogCapture));
        assert_eq!(
            *s.lock().unwrap(),
            PipelineState::Processing(Action::LogCapture)
        );
        force_state(&s, PipelineState::Idle);
        assert_eq!(*s.lock().unwrap(), PipelineState::Idle);
    }

    #[test]
    fn edit_selection_runs_through_the_state_machine() {
        let s = new_state_handle();
        assert!(transition_from_idle_to_recording(&s, Action::EditSelection));
        assert_eq!(*s.lock().unwrap(), PipelineState::Recording(Action::EditSelection));
        assert!(transition_from_recording_to_processing(&s, Action::EditSelection));
        assert_eq!(*s.lock().unwrap(), PipelineState::Processing(Action::EditSelection));
        // A mismatched action must not drive this one.
        force_state(&s, PipelineState::Recording(Action::EditSelection));
        assert!(!transition_from_recording_to_processing(&s, Action::VoiceAtCursor));
    }

    #[test]
    fn tray_state_collapses_actions() {
        assert_eq!(PipelineState::Idle.tray_state(), TrayPipelineState::Idle);
        assert_eq!(
            PipelineState::Recording(Action::LogCapture).tray_state(),
            TrayPipelineState::Recording
        );
        assert_eq!(
            PipelineState::Processing(Action::VoiceAtCursor).tray_state(),
            TrayPipelineState::Transcribing
        );
        assert_eq!(
            PipelineState::AwaitingConfirmation.tray_state(),
            TrayPipelineState::Thinking
        );
    }
}
