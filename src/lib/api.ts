import { invoke } from "@tauri-apps/api/core";

export type ModKind = "Control" | "Shift" | "Alt" | "Meta";
export type ModSide = "Left" | "Right" | "Either";

export type JsBinding = {
  primary: string;
  modifiers: { kind: ModKind; side: ModSide }[];
};

export type PermissionsStatus = {
  microphone: boolean;
  accessibility: boolean;
  screen_recording: boolean;
  camera: boolean;
};

export type PlatformCapabilities = {
  direct_voice_capture: boolean;
  local_database: boolean;
  meeting_auto_detect: boolean;
  system_audio_capture: boolean;
  screen_recording: boolean;
  bundle_self_update: boolean;
};

export const platformCapabilities = (): Promise<PlatformCapabilities> =>
  invoke("platform_capabilities");

export const permissionsStatus = (): Promise<PermissionsStatus> =>
  invoke("permissions_status");

export const openMicrophoneSettings = (): Promise<void> =>
  invoke("open_microphone_settings");

export const openAccessibilitySettings = (): Promise<void> =>
  invoke("open_accessibility_settings");

export const openScreenRecordingSettings = (): Promise<void> =>
  invoke("open_screen_recording_settings");

export const openCameraSettings = (): Promise<void> =>
  invoke("open_camera_settings");

export const requestMicrophoneAccess = (): Promise<boolean> =>
  invoke("request_microphone_access");

export type CameraAccessResult = "granted" | "denied" | "undetermined";

export const requestCameraAccess = (): Promise<CameraAccessResult> =>
  invoke("request_camera_access");

// Report a camera self-view getUserMedia failure to the backend daily log —
// the webview console is invisible in a production bundle.
export const logCameraPreviewError = (message: string): Promise<void> =>
  invoke("log_camera_preview_error", { message });

// Report a WebCodecs export/render failure to the backend daily log — the
// render runs in the webview, so its errors otherwise only reach the webview
// console (which the "See logs" toast can't actually point at).
export const logExportError = (message: string): Promise<void> =>
  invoke("log_export_error", { message });

export const promptAccessibilityAccess = (): Promise<boolean> =>
  invoke("prompt_accessibility_access");

export const requestScreenRecordingAccess = (): Promise<boolean> =>
  invoke("request_screen_recording_access");

export const getVoiceAtCursorBinding = (): Promise<JsBinding> =>
  invoke("get_voice_at_cursor_binding");

/** A configured hotkey the OS refused to register, tagged by action slug. */
export type HotkeyProblem = { action: string; message: string };

// Always empty on macOS (the event tap registers nothing); on Windows/Linux a
// combination another app already owns shows up here.
export const getHotkeyProblems = (): Promise<HotkeyProblem[]> =>
  invoke("get_hotkey_problems");

export const updateVoiceAtCursorBinding = (binding: JsBinding): Promise<void> =>
  invoke("update_voice_at_cursor_binding", { binding });

export const getLogCaptureBinding = (): Promise<JsBinding> =>
  invoke("get_log_capture_binding");

export const updateLogCaptureBinding = (binding: JsBinding): Promise<void> =>
  invoke("update_log_capture_binding", { binding });

export const getActionBinding = (): Promise<JsBinding> =>
  invoke("get_action_binding");

export const updateActionBinding = (binding: JsBinding): Promise<void> =>
  invoke("update_action_binding", { binding });

export const getEditSelectionBinding = (): Promise<JsBinding> =>
  invoke("get_edit_selection_binding");

export const updateEditSelectionBinding = (binding: JsBinding): Promise<void> =>
  invoke("update_edit_selection_binding", { binding });

export const getTriggerWordRoutingEnabled = (): Promise<boolean> =>
  invoke("get_trigger_word_routing_enabled");

export const setTriggerWordRoutingEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_trigger_word_routing_enabled", { enabled });

export const getActionTriggerWord = (): Promise<string> =>
  invoke("get_action_trigger_word");

export const setActionTriggerWord = (word: string): Promise<void> =>
  invoke("set_action_trigger_word", { word });

export type Classification = {
  kind: "note" | "task";
  project_id: string | null;
  new_project_name: string | null;
  tags: string[];
  deadline_iso: string | null;
  confidence: number;
};

export type LogCaptureClassificationReady = {
  transcript: string;
  classification: Classification | null;
  error?: string;
};

export const confirmLogCapture = (args: {
  content: string;
  kind: "note" | "task";
  project_id: string | null;
  new_project_name: string | null;
  tags: string[];
  deadline_iso: string | null;
}): Promise<string> =>
  invoke("confirm_log_capture", {
    content: args.content,
    kind: args.kind,
    projectId: args.project_id,
    newProjectName: args.new_project_name,
    tags: args.tags,
    deadlineIso: args.deadline_iso,
  });

export const cancelLogCapture = (): Promise<void> =>
  invoke("cancel_log_capture");

export const startPipeline = (): Promise<void> => invoke("start_pipeline");

export const isPipelineRunning = (): Promise<boolean> =>
  invoke("is_pipeline_running");

export type SpeechModelStatus = {
  id: string;
  display_name: string;
  version_label: string;
  description: string;
  language_label: string;
  english_only: boolean;
  accuracy_bars: number;
  speed_bars: number;
  size_label: string;
  size_bytes: number;
  downloaded: boolean;
  active: boolean;
  supported: boolean;
};

export type DownloadProgress = {
  id: string;
  bytes_downloaded: number;
  bytes_total: number;
};

export const listSpeechModels = (): Promise<SpeechModelStatus[]> =>
  invoke("list_speech_models");

export const downloadSpeechModel = (id: string): Promise<void> =>
  invoke("download_speech_model", { id });

export const getActiveSpeechModelId = (): Promise<string> =>
  invoke("get_active_speech_model_id");

export const setActiveSpeechModel = (id: string): Promise<void> =>
  invoke("set_active_speech_model", { id });

export const deleteSpeechModel = (id: string): Promise<void> =>
  invoke("delete_speech_model", { id });

// ----- Items / projects / tasks -----

export type ItemKind = "note" | "task" | "meeting" | "transcription";
export type ItemSource = "voice_at_cursor" | "log_capture" | "meeting";

export type Item = {
  id: string;
  content: string;
  source: ItemSource;
  kind: ItemKind | null;
  project_id: string | null;
  captured_at: string;
  created_at: string;
  deleted_at: string | null;
  confidence: number | null;
  classified_by: string | null;
  /** JSON-serialized FocusContext, if captured at recording time. */
  capture_context: string | null;
};

/** Parsed shape of the JSON stored in `Item.capture_context`. All fields optional. */
export type ParsedCaptureContext = {
  pid?: number;
  bundle_id?: string | null;
  app_name?: string | null;
  window_title?: string | null;
  browser_url?: string | null;
  browser_tab_title?: string | null;
  content_title?: string | null;
  content_url?: string | null;
  content_source?: string | null;
};

export function parseCaptureContext(raw: string | null | undefined): ParsedCaptureContext | null {
  if (!raw) return null;
  try {
    const obj = JSON.parse(raw);
    if (typeof obj !== "object" || obj === null) return null;
    return obj as ParsedCaptureContext;
  } catch {
    return null;
  }
}

export type Project = {
  id: string;
  name: string;
  created_at: string;
  archived_at: string | null;
  description: string | null;
  keywords: string[];
  color: string | null;
  emoji: string | null;
  updated_at: string | null;
  /** Absolute filesystem path where high-confidence items routed to this
   *  project are exported as markdown. `null` = export disabled. */
  export_folder: string | null;
  routing_aliases: string[];
  routing_app_hints: string[];
  routing_url_hints: string[];
  routing_window_hints: string[];
  routing_positive_examples: string[];
  routing_negative_examples: string[];
};

/** Partial update payload mirroring `ProjectPatch` on the Rust side.
 *  Omit a field to leave it alone; set to null to clear; set to value to update.
 *  `keywords` has no clear semantic — pass `[]` to empty it. */
export type ProjectPatch = {
  name?: string;
  description?: string | null;
  keywords?: string[];
  color?: string | null;
  emoji?: string | null;
  export_folder?: string | null;
  routing_aliases?: string[];
  routing_app_hints?: string[];
  routing_url_hints?: string[];
  routing_window_hints?: string[];
  routing_positive_examples?: string[];
  routing_negative_examples?: string[];
};

export type CreateProjectInput = {
  name: string;
  description?: string;
  keywords?: string[];
  routing_aliases?: string[];
  routing_app_hints?: string[];
  routing_url_hints?: string[];
  routing_window_hints?: string[];
  routing_positive_examples?: string[];
  routing_negative_examples?: string[];
  color?: string;
  emoji?: string;
};

export type ProjectTaggerStatus = {
  enabled: boolean;
  pending: number;
  deferred: number;
  done: number;
  failed: number;
  llm_ready: boolean;
  deterministic_batch_size: number;
  interval_minutes: number;
};

export type ProjectTaggerRunSummary = {
  scanned: number;
  assigned: number;
  deferred: number;
  failed: number;
  /** First LLM classification error of the run, if any. */
  sample_error: string | null;
};

export type TaskWithItem = {
  item: Item;
  deadline: string | null;
  completed_at: string | null;
};

export const listItems = (args: {
  project_id?: string | null;
  /** Restrict to one kind. "meeting" also matches items captured during a
   *  meeting (source = "meeting"). Omit for all kinds. */
  kind?: ItemKind | null;
  limit?: number;
  offset?: number;
}): Promise<Item[]> =>
  invoke("list_items", {
    projectId: args.project_id ?? null,
    kind: args.kind ?? null,
    limit: args.limit ?? 50,
    offset: args.offset ?? 0,
  });

export const getItem = (id: string): Promise<Item | null> =>
  invoke("get_item", { id });

export const searchItems = (
  query: string,
  opts: { kind?: ItemKind | null; limit?: number } = {},
): Promise<Item[]> =>
  invoke("search_items", {
    query,
    kind: opts.kind ?? null,
    limit: opts.limit ?? 50,
  });

export const deleteItem = (id: string): Promise<void> =>
  invoke("delete_item", { id });

export type ActivityExportOutcome = { path: string; count: number };

/** Export all activity (transcriptions, notes, tasks, meetings) captured at or
 *  after `since` (ISO-8601 UTC; null = all time) to a Markdown or CSV file in
 *  ~/Downloads. The backend reveals the file in Finder on success. */
export const exportActivity = (args: {
  since: string | null;
  format: "markdown" | "csv";
  rangeLabel: string;
}): Promise<ActivityExportOutcome> =>
  invoke("export_activity", {
    since: args.since,
    format: args.format,
    rangeLabel: args.rangeLabel,
  });

export const restoreItem = (id: string): Promise<void> =>
  invoke("restore_item", { id });

export const listTagsForItem = (item_id: string): Promise<string[]> =>
  invoke("list_tags_for_item", { itemId: item_id });

/** Update an item. Each field is optional: omit to leave alone.
 *  For project_id: pass `undefined` to leave alone, `null` to clear,
 *  or a string id to set. */
export type UpdateItemInput = {
  id: string;
  content?: string;
  /** undefined = leave alone, null = clear, string = set */
  project_id?: string | null;
  /** undefined = leave alone, "" = clear, "note"|"task" = set */
  kind?: "" | ItemKind;
  /** undefined = leave alone, [] / [...] = replace tag set */
  tags?: string[];
};

export const updateItem = (input: UpdateItemInput): Promise<Item> => {
  // Convert TS undefined-vs-null semantics into the double-Option JSON shape:
  // - field absent → backend `Option<None>` (leave alone)
  // - field null   → backend `Option<Some(None)>` (clear)
  // - field set    → backend `Option<Some(value))` (set)
  const args: Record<string, unknown> = { id: input.id };
  if (input.content !== undefined) args.content = input.content;
  if ("project_id" in input) {
    // explicit (null or string)
    args.project_id = input.project_id;
  }
  if (input.kind !== undefined) args.kind = input.kind;
  if (input.tags !== undefined) args.tags = input.tags;
  return invoke("update_item", { args });
};

export const listProjects = (include_archived = false): Promise<Project[]> =>
  invoke("list_projects", { includeArchived: include_archived });

export const createProject = (input: CreateProjectInput | string): Promise<Project> => {
  const payload: CreateProjectInput =
    typeof input === "string" ? { name: input } : input;
  return invoke("create_project", { input: payload });
};

/** Patch a project's metadata. Each field in `patch` is optional. */
export const updateProject = (id: string, patch: ProjectPatch): Promise<Project> =>
  invoke("update_project", { input: { id, ...patch } });

export const renameProject = (id: string, name: string): Promise<void> =>
  invoke("rename_project", { id, name });

export const archiveProject = (id: string): Promise<void> =>
  invoke("archive_project", { id });

export const unarchiveProject = (id: string): Promise<void> =>
  invoke("unarchive_project", { id });

/** Hard-delete a project. `reassignTo` moves the project's items to a different
 *  project; pass `null` to detach (items become unassigned). */
export const deleteProject = (
  id: string,
  reassignTo: string | null = null,
): Promise<void> =>
  invoke("delete_project", { id, reassignTo });

export const countItemsForProject = (id: string): Promise<number> =>
  invoke("count_items_for_project", { id });

export const getProjectAutoTaggingEnabled = (): Promise<boolean> =>
  invoke("get_project_auto_tagging_enabled");

export const setProjectAutoTaggingEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_project_auto_tagging_enabled", { enabled });

export const projectTaggerStatus = (): Promise<ProjectTaggerStatus> =>
  invoke("project_tagger_status");

export const projectTaggerBackfill = (args: {
  source?: ItemSource;
  limit?: number;
} = {}): Promise<number> =>
  invoke("project_tagger_backfill", {
    source: args.source ?? "voice_at_cursor",
    limit: args.limit ?? 500,
  });

export const runProjectTaggerDeterministicOnce = (
  limit?: number,
): Promise<ProjectTaggerRunSummary> =>
  invoke("run_project_tagger_deterministic_once", { limit: limit ?? null });

export const runProjectTaggerLlmOnce = (
  limit?: number,
): Promise<ProjectTaggerRunSummary> =>
  invoke("run_project_tagger_llm_once", { limit: limit ?? null });

/** Payload of `tagger:progress` events emitted during runProjectTaggerAll. */
export type ProjectTaggerProgress = {
  processed: number;
  total: number;
  assigned: number;
};

/** Backfill every untagged item + recording into the tag queue, then walk the
 *  whole queue once (router, then local AI). Long-running; listen for
 *  `tagger:progress` events for a live counter. */
export const runProjectTaggerAll = (): Promise<ProjectTaggerRunSummary> =>
  invoke("run_project_tagger_all");

export const listTasks = (args: {
  include_completed?: boolean;
  project_id?: string | null;
}): Promise<TaskWithItem[]> =>
  invoke("list_tasks", {
    includeCompleted: args.include_completed ?? false,
    projectId: args.project_id ?? null,
  });

export const completeTask = (item_id: string): Promise<void> =>
  invoke("complete_task", { itemId: item_id });

export const uncompleteTask = (item_id: string): Promise<void> =>
  invoke("uncomplete_task", { itemId: item_id });

export const setTaskDeadline = (
  item_id: string,
  deadline_iso: string | null,
): Promise<void> =>
  invoke("set_task_deadline", { itemId: item_id, deadlineIso: deadline_iso });

// ----- LLM -----

export type LlmModelStatus = {
  id: string;
  display_name: string;
  family: string;
  size_label: string;
  size_bytes: number;
  context_length: number;
  downloaded: boolean;
  active: boolean;
  supported: boolean;
  disk_bytes: number;
  incomplete: boolean;
};

export const listLlmModels = (): Promise<LlmModelStatus[]> =>
  invoke("list_llm_models");

export const downloadLlmModel = (id: string): Promise<void> =>
  invoke("download_llm_model", { id });

export const getActiveLlmModelId = (): Promise<string> =>
  invoke("get_active_llm_model_id");

export const setActiveLlmModel = (id: string): Promise<void> =>
  invoke("set_active_llm_model", { id });

export const deleteLlmModel = (id: string): Promise<void> =>
  invoke("delete_llm_model", { id });

export const testLlmInference = (prompt: string): Promise<string> =>
  invoke("test_llm_inference", { prompt });

// ----- Memory chat -----

export type ChatTurn = { role: "user" | "assistant"; content: string };

export type ChatSession = {
  id: string;
  name: string;
  project_id: string | null;
  created_at: string;
  updated_at: string;
};

export type ChatMessage = {
  id: string;
  session_id: string;
  role: "user" | "assistant";
  content: string;
  created_at: string;
};

export const createChatSession = (
  projectId: string | null,
): Promise<ChatSession> =>
  invoke("create_chat_session", { projectId });

export const listChatSessions = (
  projectId: string | null,
): Promise<ChatSession[]> =>
  invoke("list_chat_sessions", { projectId });

export const loadChatMessages = (sessionId: string): Promise<ChatMessage[]> =>
  invoke("load_chat_messages", { sessionId });

export const deleteChatSession = (sessionId: string): Promise<void> =>
  invoke("delete_chat_session", { sessionId });

export const renameChatSession = (
  sessionId: string,
  name: string,
): Promise<void> => invoke("rename_chat_session", { sessionId, name });

export type ContextSource = {
  date: string;
  kind: string;
  content: string;
};

export type ChatReply = {
  reply: string;
  sources: ContextSource[];
};

export const chatWithMemory = (
  sessionId: string,
  message: string,
  projectId?: string | null,
): Promise<ChatReply> =>
  invoke("chat_with_memory", {
    sessionId,
    message,
    projectId: projectId ?? null,
  });

export const resetOnboardingAndQuit = (): Promise<void> =>
  invoke("reset_onboarding_and_quit");

export const uninstallApplication = (deleteData: boolean): Promise<void> =>
  invoke("uninstall_application", { deleteData });

// ----- Audio feedback + onboarding flag -----

export const getAudioFeedbackEnabled = (): Promise<boolean> =>
  invoke("get_audio_feedback_enabled");

export const setAudioFeedbackEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_audio_feedback_enabled", { enabled });

export const getMuteWhileRecording = (): Promise<boolean> =>
  invoke("get_mute_while_recording");

export const setMuteWhileRecording = (enabled: boolean): Promise<void> =>
  invoke("set_mute_while_recording", { enabled });

// ----- Transcription post-processing -----

export const getFillerRemovalEnabled = (): Promise<boolean> =>
  invoke("get_filler_removal_enabled");

export const setFillerRemovalEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_filler_removal_enabled", { enabled });

export const getFillerWords = (): Promise<string[]> => invoke("get_filler_words");

export const setFillerWords = (words: string[]): Promise<void> =>
  invoke("set_filler_words", { words });

export const getCustomWords = (): Promise<string[]> => invoke("get_custom_words");

export const setCustomWords = (words: string[]): Promise<void> =>
  invoke("set_custom_words", { words });

export const getDefaultFillerWords = (): Promise<string[]> =>
  invoke("get_default_filler_words");

export const resetTccAndQuit = (): Promise<void> => invoke("reset_tcc_and_quit");

export const getOnboardingCompleted = (): Promise<boolean> =>
  invoke("get_onboarding_completed");

export const setOnboardingCompleted = (completed: boolean): Promise<void> =>
  invoke("set_onboarding_completed", { completed });

/** CI smoke-mode check-in; no-op in normal runs (see src-tauri/src/smoke.rs). */
export const smokeCheckpoint = (view: string): Promise<void> =>
  invoke("smoke_checkpoint", { view });

// ----- Tray-driven UI helpers -----

export const showMainWindow = (): Promise<void> => invoke("show_main_window");

// ----- Diagnostics -----

export const diagnosticsLogDir = (): Promise<string> =>
  invoke("diagnostics_log_dir");

export const diagnosticsRecentLog = (maxLines = 200): Promise<string> =>
  invoke("diagnostics_recent_log", { maxLines });

export const diagnosticsOpenLogFolder = (): Promise<void> =>
  invoke("diagnostics_open_log_folder");

export const getLlmUnloadSecs = (): Promise<number> =>
  invoke("get_llm_unload_secs");

export const setLlmUnloadSecs = (secs: number): Promise<void> =>
  invoke("set_llm_unload_secs", { secs });

export const getAsrUnloadSecs = (): Promise<number> =>
  invoke("get_asr_unload_secs");

export const setAsrUnloadSecs = (secs: number): Promise<void> =>
  invoke("set_asr_unload_secs", { secs });

export const applyUpdateAndRestart = (): Promise<void> =>
  invoke("apply_update_and_restart");

export const dismissUpdate = (version: string): Promise<void> =>
  invoke("dismiss_update", { version });

/** The installed app version, e.g. "0.4.1". */
export const getAppVersion = (): Promise<string> => invoke("app_version");

/** Outcome of a user-triggered "Check for Updates" (see updater.rs). */
export type ManualUpdateCheck =
  | { status: "up-to-date"; version: string }
  | { status: "downloading"; version: string }
  | { status: "error"; message: string };

export const checkForUpdate = (): Promise<ManualUpdateCheck> =>
  invoke("check_for_update");

// ----- Auto-file (confident captures) -----

export type LogCaptureAutoFiled = {
  item_id: string;
  project_name: string;
  kind: "note" | "task";
  preview: string;
  confidence: number;
};

export const undoLogCapture = (itemId: string): Promise<void> =>
  invoke("undo_log_capture", { itemId });

export const getAutoFileEnabled = (): Promise<boolean> =>
  invoke("get_auto_file_enabled");

export const setAutoFileEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_auto_file_enabled", { enabled });

export const getAutoFileThreshold = (): Promise<number> =>
  invoke("get_auto_file_threshold");

export const setAutoFileThreshold = (threshold: number): Promise<void> =>
  invoke("set_auto_file_threshold", { threshold });

// ----- Markdown export (per-project folder) -----

export const getExportConfidenceThreshold = (): Promise<number> =>
  invoke("get_export_confidence_threshold");

export const setExportConfidenceThreshold = (threshold: number): Promise<void> =>
  invoke("set_export_confidence_threshold", { threshold });

/** Open a native folder picker. Returns the chosen absolute path, or `null`
 *  if the user cancels. */
export const pickExportFolder = (): Promise<string | null> =>
  invoke("pick_export_folder");

/** Backfill: re-export every non-deleted item + meeting for a project to its
 *  configured folder. Returns the count of files written. */
export const exportProjectBackfill = (projectId: string): Promise<number> =>
  invoke("export_project_backfill", { projectId });

// ----- Item events (lifecycle log) -----

export type ItemEvent = {
  id: string;
  item_id: string;
  event_type: string;
  detail: string | null;
  created_at: string;
};

export const listItemEvents = (itemId: string): Promise<ItemEvent[]> =>
  invoke("list_item_events", { itemId });

export const listSessionsForItem = (
  itemId: string,
): Promise<ChatSession[]> =>
  invoke("list_sessions_for_item", { itemId });

// ----- Claude Code session transcript -----

export type ClaudeSessionSummary = {
  session_id: string;
  preview: string;
  message_count: number;
  timestamp: string;
};

export type ClaudeSessionMessage = {
  role: string;
  content: string;
  timestamp: string;
};

export const listClaudeSessions = (): Promise<ClaudeSessionSummary[]> =>
  invoke("list_claude_sessions");

export const loadClaudeSession = (
  sessionId: string,
): Promise<ClaudeSessionMessage[]> =>
  invoke("load_claude_session", { sessionId });

// ----- Dashboard analytics -----

export type PeriodStats = {
  transcriptions: number;
  words: number;
};

export type CategoryPeriodStats = {
  count: number;
  words: number;
  duration_ms: number;
};

export type CategoryStats = {
  today: CategoryPeriodStats;
  week: CategoryPeriodStats;
  month: CategoryPeriodStats;
  all_time: CategoryPeriodStats;
};

export type StatsCategoryKey =
  | "transcriptions"
  | "notes"
  | "tasks"
  | "meetings"
  | "recordings";

export type DailyActivity = {
  date: string;
  transcriptions: number;
  notes: number;
  tasks: number;
  meetings: number;
  recordings: number;
};

export type DashboardStats = {
  today: PeriodStats;
  week: PeriodStats;
  month: PeriodStats;
  all_time: PeriodStats;
  /** [date_str, count] tuples for last 90 days, oldest first */
  daily_counts: [string, number][];
  current_streak: number;
  longest_streak: number;
  avg_words_per_capture: number;
  /** Busiest hour 0-23, or null if no data */
  busiest_hour: number | null;
  categories: Record<StatsCategoryKey, CategoryStats>;
  /** One row per local day for the last 90 days, including zero-activity days. */
  daily_activity: DailyActivity[];
};

export const getDashboardStats = (): Promise<DashboardStats> =>
  invoke("get_dashboard_stats");

// ============= Meetings =============

export type MeetingStatus =
  | "recording"
  | "transcribing"
  | "summarizing"
  | "complete"
  | "failed"
  | "recovered";

export type MeetingRow = {
  item_id: string;
  started_at: string;
  ended_at: string | null;
  duration_ms: number | null;
  detected_app: string | null;
  detected_app_name: string | null;
  status: MeetingStatus;
  transcript_json: string | null;
  summary_json: string | null;
  user_notes: string | null;
  failed_chunk_count: number;
  mic_only: boolean;
  /// Project name resolved via the meeting's item.project_id at read time.
  /// `null` when unassigned. Reflects later reassignment from the detail panel.
  project_name: string | null;
};

export type Segment = {
  speaker: "you" | "them";
  start_ms: number;
  end_ms: number;
  text: string;
};

export type StoredTranscript = {
  segments: Segment[];
  duration_ms: number;
  asr_model: string;
  chunk_seconds: number;
  failed_chunk_count: number;
  mic_only: boolean;
};

export type StoredSummary = {
  summary: string[];
  action_items: {
    text: string;
    owner: "you" | "them" | "unspecified";
    tags?: string[];
    project_name?: string | null;
  }[];
  suggested_title: string;
  raw?: string | null;
  tags?: string[];
  project_name?: string | null;
};

export const startMeetingManual = (): Promise<string> => invoke("start_meeting_manual");
export const stopMeeting = (): Promise<string> => invoke("stop_meeting");
export const isMeetingActive = (): Promise<boolean> => invoke("is_meeting_active");
export const getMeeting = (id: string): Promise<MeetingRow | null> =>
  invoke("get_meeting", { id });
export const listMeetings = (): Promise<MeetingRow[]> => invoke("list_meetings");
/** Action items promoted out of a meeting into their own item rows. Empty for
 *  meetings whose summary actions were never promoted. */
export const listMeetingActionItems = (id: string): Promise<Item[]> =>
  invoke("list_meeting_action_items", { id });
export const updateMeetingNotes = (id: string, notes: string): Promise<void> =>
  invoke("update_meeting_notes", { id, notes });
export const renameMeeting = (id: string, title: string): Promise<void> =>
  invoke("rename_meeting", { id, title });
export const deleteMeeting = (id: string): Promise<void> =>
  invoke("delete_meeting", { id });

export type MeetingSettings = {
  auto_detect: boolean;
  app_prefs: Record<string, "always" | "ask" | "never">;
  soft_warn_min: number;
  hard_cap_min: number;
  summary_prompt: string;
};

export const getMeetingSettings = (): Promise<MeetingSettings> =>
  invoke("get_meeting_settings");

export const setMeetingSummaryPrompt = (prompt: string): Promise<void> =>
  invoke("set_meeting_summary_prompt", { prompt });

export const setMeetingAutoDetect = (on: boolean): Promise<void> =>
  invoke("set_meeting_auto_detect", { on });

export const setMeetingAppPref = (
  bundle_id: string,
  pref: "always" | "ask" | "never",
): Promise<void> => invoke("set_meeting_app_pref", { bundleId: bundle_id, pref });

export const clearMeetingAppPref = (bundle_id: string): Promise<void> =>
  invoke("meeting_clear_app_pref", { bundleId: bundle_id });

export const retryMeetingSummary = (id: string): Promise<void> =>
  invoke("retry_meeting_summary", { id });
export const retryMeetingChunks = (id: string): Promise<void> =>
  invoke("retry_meeting_chunks", { id });

// ----- Input device selection -----

export interface InputDevice {
  name: string;
  sample_rate: number;
  channels: number;
  is_system_default: boolean;
}

export type InputDeviceSort = "last_used" | "alphabetical";

export const listInputDevices = (): Promise<InputDevice[]> =>
  invoke("list_input_devices");

export const getPreferredInputDevice = (): Promise<string | null> =>
  invoke("get_preferred_input_device");

export const setPreferredInputDevice = (name: string | null): Promise<void> =>
  invoke("set_preferred_input_device", { name });

export const getRecentInputDevices = (): Promise<string[]> =>
  invoke("get_recent_input_devices");

export const getInputDeviceSort = (): Promise<InputDeviceSort> =>
  invoke("get_input_device_sort");

export const setInputDeviceSort = (sort: InputDeviceSort): Promise<void> =>
  invoke("set_input_device_sort", { sort });

// ---------------------------------------------------------------------------
// Daily recap
// ---------------------------------------------------------------------------

export type DailySummaryStatus = "generated" | "skipped_empty" | "failed";

export type DailySummarySectionItem = {
  text: string;
  source_id?: string | null;
};

export type DailySummarySections = {
  meetings?: DailySummarySectionItem[];
  focus_work?: DailySummarySectionItem[];
  notes?: DailySummarySectionItem[];
  things_that_came_up?: DailySummarySectionItem[];
};

export type DailySummary = {
  date: string;
  generated_at: string;
  status: DailySummaryStatus;
  narrative: string;
  sections: DailySummarySections;
  source_meeting_ids: string[];
  source_item_ids: string[];
  model_version: string;
};

export type DailyRecapSettings = {
  enabled: boolean;
  deliver_hour: number;
  include_weekends: boolean;
};

export const getDailySummary = (date: string): Promise<DailySummary | null> =>
  invoke("daily_summary_get", { date });

export const listRecentDailySummaries = (
  limit: number,
): Promise<DailySummary[]> => invoke("daily_summary_list_recent", { limit });

export const regenerateDailySummary = (date: string): Promise<DailySummary> =>
  invoke("daily_summary_regenerate", { date });

export const getDailyRecapSettings = (): Promise<DailyRecapSettings> =>
  invoke("daily_recap_settings_get");

export const setDailyRecapSettings = (
  settings: DailyRecapSettings,
): Promise<void> => invoke("daily_recap_settings_set", { settings });

export const dailyRecapNotificationPermissionStatus = (): Promise<boolean> =>
  invoke("daily_recap_notification_permission_status");

// ===================== Guide templates =====================

export type GuideTemplate = {
  id: string;
  name: string;
  description: string;
  goal: string;
  notes: string;
  created_at: string;
  updated_at: string;
};

export const listGuideTemplates = (): Promise<GuideTemplate[]> =>
  invoke("list_guide_templates");

export const createGuideTemplate = (
  name: string,
  description: string,
  goal: string,
  notes: string,
): Promise<GuideTemplate> =>
  invoke("create_guide_template", { name, description, goal, notes });

export const updateGuideTemplate = (
  id: string,
  name: string,
  description: string,
  goal: string,
  notes: string,
): Promise<void> =>
  invoke("update_guide_template", { id, name, description, goal, notes });

export const deleteGuideTemplate = (id: string): Promise<void> =>
  invoke("delete_guide_template", { id });

export const startGuidedSession = (templateId: string): Promise<string> =>
  invoke("start_guided_session", { templateId });

export const attachGuide = (templateId: string): Promise<string> =>
  invoke("attach_guide", { templateId });

export const detachGuide = (sessionId: string): Promise<void> =>
  invoke("detach_guide", { sessionId });

export const guideSetMode = (
  sessionId: string,
  mode: "auto" | "on_demand",
): Promise<void> => invoke("guide_set_mode", { sessionId, mode });

export const guideTriggerNow = (sessionId: string): Promise<void> =>
  invoke("guide_trigger_now", { sessionId });

export type TranscriptSegment = {
  speaker: "you" | "them";
  start_ms: number;
  end_ms: number;
  text: string;
};

export const getLiveTranscript = (): Promise<TranscriptSegment[]> =>
  invoke("get_live_transcript");

export const getActiveGuides = (): Promise<GuideInit[]> =>
  invoke("get_active_guides");

export const showMeetingHud = (
  focus?: "transcript" | "guides",
): Promise<void> => invoke("show_meeting_hud", { focus: focus ?? null });

export const saveHudFrame = (
  x: number,
  y: number,
  w: number,
  h: number,
): Promise<void> => invoke("save_hud_frame", { x, y, w, h });

export type GuideKeyPoint = {
  id: string;
  label: string;
  status: "covered" | "partial" | "open" | string;
};

export type GuideInit = {
  sessionId: string;
  slot: number;
  templateName: string;
  goal: string;
  mode: "auto" | "on_demand";
};

export type GuideUpdate = {
  sessionId: string;
  slot: number;
  meetingId: string;
  templateName?: string;
  goal?: string;
  mode: "auto" | "on_demand";
  keyPoints: GuideKeyPoint[];
  suggestions: string[];
  updatedAt: string;
};

export type CommonActionTemplate = {
  category: string;
  description: string;
  voice_phrases: string[];
};

export const getAppLauncherEnabled = (): Promise<boolean> =>
  invoke("get_app_launcher_enabled");

export const setAppLauncherEnabled = (enabled: boolean): Promise<void> =>
  invoke("set_app_launcher_enabled", { enabled });

export const getActionCounter = (): Promise<number> =>
  invoke("get_action_counter");

export const resetActionCounter = (): Promise<void> =>
  invoke("reset_action_counter");

export const getCommonActions = (): Promise<CommonActionTemplate[]> =>
  invoke("get_common_actions");

// ============= Voice Format Templates =============

export type FormatTemplate = {
  id: string;
  name: string;
  trigger_phrases: string[];
  system_prompt: string;
};

export const getFormatTemplates = (): Promise<FormatTemplate[]> =>
  invoke("get_format_templates");

export const setFormatTemplates = (
  templates: FormatTemplate[],
): Promise<void> => invoke("set_format_templates", { templates });

// ============= Screen Recordings =============

export type RecordingRow = {
  id: string;
  created_at: number;
  file_path: string;
  duration_ms: number | null;
  width: number | null;
  height: number | null;
  size_bytes: number | null;
  source_label: string | null;
  has_mic: boolean;
  has_sysaudio: boolean;
  thumb_path: string | null;
  drive_file_id: string | null;
  drive_link: string | null;
  upload_status: string;
  upload_error: string | null;
  exports: string;
  title: string | null;
  transcript: string | null;
  denoised_path: string | null;
  events_path: string | null;
  project_json: string | null;
  webcam_path: string | null;
  cursor_hidden: boolean;
  webcam_offset_ms: number | null;
  n_events: number | null;
  n_clicks: number | null;
  project_id: string | null;
  confidence: number | null;
  classified_by: string | null;
};

export const startScreenRecording = (p: {
  display_id?: number | null;
  window_id?: number | null;
  mic_device?: string | null;
  sysaudio: boolean;
  source_label: string;
  hide_cursor?: boolean | null;
  camera_uid?: string | null;
  /** Crop region [x, y, w, h] in global points (top-left origin). Display-path
   *  only; ignored with a window source. Omit/null for full-display capture. */
  rect?: [number, number, number, number] | null;
}): Promise<void> =>
  invoke("start_screen_recording", {
    displayId: p.display_id ?? null,
    windowId: p.window_id ?? null,
    micDevice: p.mic_device ?? null,
    sysaudio: p.sysaudio,
    sourceLabel: p.source_label,
    hideCursor: p.hide_cursor ?? null,
    cameraUid: p.camera_uid ?? null,
    rect: p.rect ?? null,
  });
export const stopScreenRecording = (): Promise<RecordingRow> =>
  invoke("stop_screen_recording");
export const isScreenRecording = (): Promise<boolean> =>
  invoke("is_screen_recording");
/** Pause the in-progress recording (SIGUSR1 to the sidecar). Idempotent. */
export const pauseScreenRecording = (): Promise<void> =>
  invoke("pause_screen_recording");
/** Resume the paused recording (SIGUSR2 to the sidecar). Idempotent. */
export const resumeScreenRecording = (): Promise<void> =>
  invoke("resume_screen_recording");
/** Whether the in-progress recording is currently paused (false when idle). */
export const isScreenRecordingPaused = (): Promise<boolean> =>
  invoke("is_screen_recording_paused");
export const listRecordings = (): Promise<RecordingRow[]> =>
  invoke("list_recordings");
/** Open (or focus) the dedicated editor window for a recording. */
export const openRecordingEditor = (id: string, title?: string): Promise<void> =>
  invoke("open_recording_editor", { id, title: title ?? null });
export const deleteRecording = (id: string): Promise<void> =>
  invoke("delete_recording", { id });
export const renameRecording = (id: string, title: string): Promise<void> =>
  invoke("rename_recording", { id, title });
export const transcribeRecording = (id: string): Promise<string> =>
  invoke("transcribe_recording", { id });

/** A timed caption segment from the local ASR. `startMs`/`endMs` are ms
 *  relative to the recording's t=0 (same base as the recorded-events file). */
export interface CaptionSegment {
  startMs: number;
  endMs: number;
  text: string;
}

/** Generate timed caption segments for a recording. Emits `captions-progress`
 *  events `{ id, ratio }` (ratio 0..1) while running. Rejects with a friendly
 *  message on failure; the caller stores the returned segments in the project. */
export const generateCaptions = (id: string): Promise<CaptionSegment[]> =>
  invoke("generate_captions", { id });
export const denoiseRecording = (id: string): Promise<void> =>
  invoke("denoise_recording", { id });
export const revealRecording = (id: string): Promise<void> =>
  invoke("reveal_recording", { id });

/** Reveal a specific file inside the recordings folder (e.g. the editor's
 *  `<id>.rendered.mp4`). Rejects with a friendly message when the path is
 *  missing or outside the recordings dir. */
export const revealRecordingFile = (path: string): Promise<void> =>
  invoke("reveal_recording_file", { path });

export const exportRecording = (
  id: string,
  quality: "1080" | "720" | "480",
): Promise<RecordingRow> => invoke("export_recording", { id, quality });

/** Raw recorded-input events JSONL for a recording (for auto-zoom). Rejects when
 *  the recording has no events file / it's unreadable — callers treat a
 *  rejection as "render without zoom", not a hard error. */
export const readRecordingEvents = (id: string): Promise<string> =>
  invoke("read_recording_events", { id });

/** Opaque editor-project settings JSON (see `src/lib/editorProject.ts`).
 *  `null` means editor defaults; parse with `parseProject`, never JSON.parse
 *  directly since the stored value may be absent or stale. */
export const getRecordingProject = (id: string): Promise<string | null> =>
  invoke("get_recording_project", { id });

/** Persist a recording's editor-project settings JSON verbatim. */
export const setRecordingProject = (
  id: string,
  projectJson: string,
): Promise<void> =>
  invoke("set_recording_project", { id, projectJson });

/** Copy a user-picked image into the recordings dir as `<id>.bg.<ext>` and
 *  return its absolute path, for use as an editor background. Rejects (with a
 *  friendly message) on a missing file or unsupported extension. */
export const importEditorBackground = (
  id: string,
  srcPath: string,
): Promise<string> =>
  invoke("import_editor_background", { id, srcPath });

/** Finalize an editor export: hand the frontend's video-only render bytes to
 *  Rust, which muxes the recording's (trim-aligned, speed-retimed) audio back
 *  in and writes `<id>.rendered.mp4`. Bytes ride as the raw IPC body (no JSON
 *  number-array copy); the id + optional trim window + speed ranges travel in
 *  headers. Pass `trimStartMs`/`trimEndMs` (SOURCE-time ms) to align the
 *  soundtrack to a trim, or omit both for full-length audio.
 *
 *  `speedRanges` are the POST-TRIM-time speed segments (already shifted via
 *  `shiftRangesForTrim`) that Rust applies to the trimmed WAV; omit/empty for
 *  no retiming. Contract: Rust never re-derives the trim offset — it trusts the
 *  ranges are already in the trimmed audio's time base.
 *
 *  `normalizeLoudness` toggles the loudness-normalization polish pass (gated-RMS
 *  toward −16 dBFS + soft-knee limiter) Rust applies AFTER retime, pre-mux; it's
 *  a best-effort step (any Rust-side failure degrades to un-normalized audio, so
 *  it never fails the export). Only sent when `true`.
 *
 *  `music` (Task 7) adds a background-music track mixed under the voice audio
 *  AFTER normalize, pre-mux — `{path, volume}` rides as the `x-music` JSON
 *  header. Best-effort like normalization: any Rust-side failure (missing
 *  file, decode error) degrades to music-less audio, never fails the export.
 *  Only sent when non-null.
 *
 *  `denoise` toggles an RNNoise noise-removal pass Rust runs on the voice audio
 *  BEFORE normalize (rides as `x-denoise`). Best-effort like normalization: any
 *  Rust-side failure degrades to the un-denoised audio, never fails the export.
 *  Only sent when `true`. Returns the updated row. */
export const finalizeRenderedRecording = (
  id: string,
  bytes: Uint8Array,
  trim?: { startMs: number; endMs: number } | null,
  speedRanges?: { startMs: number; endMs: number; rate: number }[] | null,
  normalizeLoudness?: boolean,
  music?: { path: string; volume: number } | null,
  denoise?: boolean,
): Promise<RecordingRow> => {
  const headers: Record<string, string> = { "x-recording-id": id };
  if (trim) {
    headers["x-trim-start-ms"] = String(Math.round(trim.startMs));
    headers["x-trim-end-ms"] = String(Math.round(trim.endMs));
  }
  if (speedRanges && speedRanges.length > 0) {
    headers["x-speed-ranges"] = JSON.stringify(
      speedRanges.map((r) => ({
        startMs: Math.round(r.startMs),
        endMs: Math.round(r.endMs),
        rate: r.rate,
      })),
    );
  }
  if (denoise) {
    headers["x-denoise"] = "true";
  }
  if (normalizeLoudness) {
    headers["x-normalize-loudness"] = "true";
  }
  if (music) {
    headers["x-music"] = JSON.stringify({ path: music.path, volume: music.volume });
  }
  return invoke("finalize_rendered_recording", bytes, { headers });
};

/** The persisted global editor defaults blob (opaque JSON string), or null when
 *  none have been saved yet. Parse with `parseEditorDefaults`. */
export const getEditorDefaults = (): Promise<string | null> => invoke("get_editor_defaults");

/** Persist the global editor defaults blob (an opaque JSON string produced by
 *  the caller). Used by the editor's auto-remember write. */
export const setEditorDefaults = (json: string): Promise<void> =>
  invoke("set_editor_defaults", { json });

/** Copy a file inside the recordings folder to the system clipboard as a file
 *  reference (macOS NSPasteboard) — a paste in Finder/Mail/Slack pastes the
 *  actual file, not a text path. Rejects with a friendly message when the
 *  path is missing/outside the recordings dir, or on non-macOS platforms. */
export const copyExportToClipboard = (path: string): Promise<void> =>
  invoke("copy_export_to_clipboard", { path });

/** Save an editor GIF export: hand the frontend's fully-rendered animated GIF
 *  bytes to Rust, which writes them verbatim to `<id>.rendered.gif` and records
 *  a `"rendered-gif"` export row (distinct from the MP4 `"rendered"` row).
 *
 *  Unlike `finalizeRenderedRecording`, there is NO audio path — a GIF has no
 *  soundtrack — so the only header is the recording id; the bytes ride as the
 *  raw IPC body (same least-copy transport). Returns the updated row. */
export const saveRenderedGif = (
  id: string,
  bytes: Uint8Array,
): Promise<RecordingRow> =>
  invoke("save_rendered_gif", bytes, { headers: { "x-recording-id": id } });

/** Replace the recording's thumbnail with the poster frame of an edited
 *  export (raw JPEG bytes). Written to `<id>.edited.jpg` + thumb_path update,
 *  so the library shows the edited look. Best-effort from the editor. */
export const setRecordingThumbnail = (
  id: string,
  bytes: Uint8Array,
): Promise<void> =>
  invoke("set_recording_thumbnail", bytes, { headers: { "x-recording-id": id } });

export type DriveStatus = {
  connected: boolean;
  email: string | null;
};

export const driveStatus = (): Promise<DriveStatus> => invoke("drive_status");
export const driveConnect = (): Promise<DriveStatus> => invoke("drive_connect");
export const driveDisconnect = (): Promise<void> => invoke("drive_disconnect");
export const getDriveClientId = (): Promise<string> => invoke("get_drive_client_id");
export const setDriveClientCredentials = (
  clientId: string,
  clientSecret: string,
): Promise<void> => invoke("set_drive_client_credentials", { clientId, clientSecret });
/** Upload quality choices: `"rendered"` = the EDITED export from the editor
 *  (trim/zoom/webcam/background applied); the rest transcode/pass the original. */
export type UploadQuality = "rendered" | "original" | "1080" | "720" | "480";

export const uploadRecording = (
  id: string,
  quality: UploadQuality,
  makePublic?: boolean,
): Promise<RecordingRow> =>
  invoke("upload_recording", { id, quality, makePublic: makePublic ?? null });

export type DrivePrefs = {
  folder_name: string;
  make_public: boolean;
};

export const getDrivePrefs = (): Promise<DrivePrefs> => invoke("get_drive_prefs");

export const setDrivePrefs = (
  folderName: string,
  makePublic: boolean,
): Promise<void> => invoke("set_drive_prefs", { folderName, makePublic });

export type DisplaySource = { id: number; width: number; height: number; label: string };
export type WindowSource = { id: number; app: string; title: string; width: number; height: number; thumb: string };
export type ScreenSources = { displays: DisplaySource[]; windows: WindowSource[] };
export const listScreenSources = (): Promise<ScreenSources> =>
  invoke("list_screen_sources");

export type CameraSource = { uid: string; name: string };
export type Cameras = { cameras: CameraSource[] };
/** Enumerate webcams via the sidecar's `--list-cameras`. Rejects with a
 *  friendly message on failure — treat a rejection as "no cameras available",
 *  not a hard error. */
export const listCameras = (): Promise<Cameras> => invoke("list_cameras");

export type ScreenrecAudioPrefs = {
  sysaudio: boolean;
  mic_enabled: boolean;
  mic_device: string;
  hide_cursor: boolean;
  /** UID of the webcam to record alongside the capture; "" = webcam off. */
  camera_uid: string;
  /** Pre-record 3→2→1 countdown before Start actually begins recording. */
  countdown: boolean;
};
export const getScreenrecAudioPrefs = (): Promise<ScreenrecAudioPrefs> =>
  invoke("get_screenrec_audio_prefs");
export const setScreenrecAudioPrefs = (prefs: ScreenrecAudioPrefs): Promise<void> =>
  invoke("set_screenrec_audio_prefs", { prefs });
export const openScreenrecSetup = (): Promise<void> =>
  invoke("open_screenrec_setup");

/** Bounds `[x, y, width, height]` of a display (GLOBAL points, top-left
 *  origin — the same space `startScreenRecording`'s `rect` param and the
 *  area-picker/countdown windows use), keyed by the SAME id `listScreenSources`
 *  returns as `DisplaySource.id`. Rejects with a friendly message if the
 *  display is no longer attached. */
export const getDisplayBounds = (
  displayId: number,
): Promise<[number, number, number, number]> =>
  invoke("get_display_bounds", { displayId });

// --- Area picker ---

/** Show the full-screen area-picker overlay on `displayId`. The picker page
 *  reports back via the `area-picker-result` event (see `listenAreaPickerResult`
 *  below), NOT this promise's resolution — this only confirms the overlay
 *  was shown. */
export const showAreaPicker = (displayId: number): Promise<void> =>
  invoke("show_area_picker", { displayId });

/** Hide the area-picker overlay unconditionally (no-op if not showing). */
export const closeAreaPicker = (): Promise<void> => invoke("close_area_picker");

export type AreaPickerResultPayload = {
  rect: [number, number, number, number] | null;
};

/** Called by the area-picker page itself on confirm (mouse-up, `rect` =
 *  `[x, y, w, h]` GLOBAL points) or cancel (Esc, `rect` = null). Rust hides
 *  the picker window unconditionally as part of handling this call, then
 *  forwards the result to the setup window's `area-picker-result` event. */
export const submitAreaPickerResult = (
  rect: [number, number, number, number] | null,
): Promise<void> => invoke("submit_area_picker_result", { rect });

// --- Countdown ---

/** Show the pre-record countdown overlay centered on `displayId`, ticking
 *  from `seconds` down to 1 (~1s per tick). */
export const showCountdownOverlay = (
  displayId: number,
  seconds: number,
): Promise<void> => invoke("show_countdown_overlay", { displayId, seconds });

/** Hide the countdown overlay unconditionally (no-op if not showing). Call
 *  on every path that ends the countdown flow (natural finish, Esc-cancel,
 *  and — critically — any `startScreenRecording` failure) so the
 *  always-on-top overlay can never be stranded on screen. */
export const hideCountdownOverlay = (): Promise<void> =>
  invoke("hide_countdown_overlay");

/** Called by the countdown page itself when the user presses Esc. Hides the
 *  countdown and re-shows the setup window — the setup page does not need to
 *  react beyond listening for `countdown-cancelled` if it wants to reset its
 *  "starting…" UI state. */
export const cancelCountdown = (): Promise<void> => invoke("cancel_countdown");

/** Show the floating camera self-view for the named camera. Used by the setup
 *  window to PRE-WARM the camera as soon as it's enabled in the picker (the
 *  self-view page keeps its stream when shown again for the same camera, so
 *  recording start hands over seamlessly). */
export const showCameraPreview = (cameraName: string): Promise<void> =>
  invoke("show_camera_preview", { cameraName });

/** Hide the self-view and release the camera (setup dismissed / camera
 *  disabled without starting a recording). */
export const hideCameraPreview = (): Promise<void> => invoke("hide_camera_preview");

/** Called by the countdown page itself when its own visual tick reaches
 *  zero. The countdown page is the single clock for "when did the countdown
 *  end" — the setup window starts recording ONLY on receiving the resulting
 *  `countdown-finished` event, never from its own timer (that was the
 *  Esc-cancel race this replaces). */
export const finishCountdown = (): Promise<void> => invoke("finish_countdown");

// --- Embedding index (chat memory v2) ---

export const downloadEmbeddingModel = (): Promise<void> =>
  invoke("download_embedding_model");

export interface EmbeddingIndexStatus {
  model_downloaded: boolean;
  embeddings: number;
  indexed_sources: number;
  total_sources: number;
}

export const getEmbeddingIndexStatus = (): Promise<EmbeddingIndexStatus> =>
  invoke("embedding_index_status");

// ===================== Guide review (post-meeting) =====================

export type ScorecardItem = {
  criterion: string;
  verdict: string; // "met" | "partial" | "missed" | "unknown" (kept loose)
  evidence: string;
  why: string;
  tip: string;
};

export type EmergentItem = { observation: string; evidence: string };

export type GuideReview = {
  overall: string; // "strong" | "mixed" | "weak"
  synthesis: string;
  scorecard: ScorecardItem[];
  emergent: EmergentItem[];
};

export type TimelineEntry = {
  at: string;
  key_points: { id: string; label: string; status: string }[];
  suggestions: string[];
};

export type GuideRun = {
  id: string;
  meeting_id: string;
  template_id: string;
  template_name: string;
  template_json: string;
  slot: number;
  started_at: string;
  timeline_json: string | null;
  review_json: string | null;
  status: string; // "pending" | "ready" | "failed"
  error: string | null;
  generated_at: string | null;
  created_at: string;
};

export const listGuideRuns = (meetingId: string): Promise<GuideRun[]> =>
  invoke("list_guide_runs", { meetingId });

export const guideRunsForTemplate = (
  templateId: string,
  limit: number,
): Promise<GuideRun[]> => invoke("guide_runs_for_template", { templateId, limit });

export const regenerateGuideReview = (runId: string): Promise<void> =>
  invoke("regenerate_guide_review", { runId });
