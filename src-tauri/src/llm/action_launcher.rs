use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use tracing::{debug, info, warn};


use crate::commands::AppState;
use crate::llm::{GenerateRequest, LlmError, LlmGenerator};
use crate::settings::FormatTemplate;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ActionCommand {
    pub is_action: bool,
    pub action_type: Option<String>, // "launch_app" | "draft_email" | "open_url" | "increment_counter" | "reset_counter" | "show_counter" | "format_text"
    pub app_name: Option<String>,
    pub email_to: Option<String>,
    pub email_subject: Option<String>,
    pub email_body: Option<String>,
    pub url: Option<String>,
    /// Matched format template id (e.g. "email", "slack"). Only set when
    /// `action_type == "format_text"`.
    #[serde(default)]
    pub format_id: Option<String>,
    /// The substring of the user's dictation that should be reformatted —
    /// i.e. everything after the "format as X" trigger phrase.
    #[serde(default)]
    pub format_body: Option<String>,
    pub confidence: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("llm error: {0}")]
    Llm(#[from] LlmError),
    #[error("json parsing failed: {0}")]
    Parse(String),
    #[error("execution failed: {0}")]
    Execute(String),
    #[error("settings error: {0}")]
    Settings(String),
}

const ACTION_SYSTEM_PROMPT_BASE: &str = "\
You are Echo Scribe's system action classifier.
Analyze the user's voice dictation and classify if it represents a system action/command.
Respond ONLY with a single JSON object matching this schema:
{
  \"is_action\": true | false,
  \"action_type\": \"launch_app\" | \"draft_email\" | \"open_url\" | \"increment_counter\" | \"reset_counter\" | \"show_counter\" | \"format_text\" | null,
  \"app_name\": \"<name of app to launch or null>\",
  \"email_to\": \"<recipient name or email address or null>\",
  \"email_subject\": \"<subject line or null>\",
  \"email_body\": \"<body content or null>\",
  \"url\": \"<url to open or null>\",
  \"format_id\": \"<id of matched format template or null>\",
  \"format_body\": \"<the portion of dictation that should be reformatted, or null>\",
  \"confidence\": <float between 0.0 and 1.0>
}

Common templates:
- Launch app: 'open Slack', 'launch Safari', 'open Growthinator', 'launch LiveCase'. action_type: 'launch_app'
- Draft email: 'email denis about Growthinator saying tests passed', 'email John about meeting saying I will be there'. action_type: 'draft_email'
- Open URL: 'open google', 'go to github.com', 'open website echo-scribe.com'. action_type: 'open_url'
- Increment counter: 'increment counter', 'add one to counter', 'add to count'. action_type: 'increment_counter'
- Reset counter: 'reset counter', 'clear count', 'reset action count'. action_type: 'reset_counter'
- Show counter: 'show counter', 'how many actions', 'what is the count'. action_type: 'show_counter'
- Format text: the user dictates a 'format as X' phrase followed by the body to reformat. action_type: 'format_text'. Set format_id to the matching template id, and format_body to the dictation text AFTER the trigger phrase (the content to be reformatted). Only use format_text if the user's dictation clearly starts with or contains a format-trigger phrase from the list below.";

const ACTION_SYSTEM_PROMPT_TAIL: &str = "\n\
Rules:
- If the user's transcript matches any of these command intents, set is_action to true, appropriate action_type, extract details, and set confidence high (e.g. >= 0.85).
- If it's just regular dictation (like a note, task, diary thoughts, or description), set is_action to false and all other fields to null.
- For format_text: confidence must be >= 0.85 only when both (a) a recognised format trigger phrase appears AND (b) format_body is non-empty.
- Respond ONLY with the raw JSON object. No surrounding markdown, no backticks, no prose.";

fn build_action_system_prompt(templates: &[FormatTemplate]) -> String {
    let mut out = String::from(ACTION_SYSTEM_PROMPT_BASE);
    if !templates.is_empty() {
        out.push_str("\n\nAvailable format templates:");
        for t in templates {
            out.push_str(&format!(
                "\n- id='{}' name='{}' trigger_phrases={:?}",
                t.id, t.name, t.trigger_phrases
            ));
        }
    } else {
        out.push_str("\n\nAvailable format templates: (none configured — do not classify as format_text)");
    }
    out.push_str(ACTION_SYSTEM_PROMPT_TAIL);
    out
}

/// Detect if the spoken transcript represents a system command action.
/// `templates` is the user's configured voice format templates; their phrases
/// are injected into the classifier system prompt so the LLM can pick one.
pub async fn detect_action<L: LlmGenerator + ?Sized>(
    llm: &L,
    transcript: &str,
    templates: &[FormatTemplate],
) -> Result<ActionCommand, ActionError> {
    let system_prompt = build_action_system_prompt(templates);
    let req = GenerateRequest {
        system: Some(system_prompt),
        user: transcript.to_string(),
        history: Vec::new(),
        max_tokens: 256,
        temperature: 0.1,
        stop_strings: Vec::new(),
        grammar_gbnf: None,
        n_ctx: Some(2048), // Dynamic context optimization: lean 2048 context window for speed
    };

    let raw = llm.generate(req.clone()).await?;
    debug!(raw = %raw, "action classifier raw output");

    let parsed = match parse_raw_action(&raw) {
        Ok(cmd) => cmd,
        Err(e) => {
            warn!(?e, "action first-pass parse failed; retrying with stricter prompt");
            let mut req_retry = req;
            req_retry.user = format!(
                "{transcript}\n\n(Your previous response failed to parse: {e}. \
                 Respond ONLY with a single JSON object matching the action schema. \
                 No prose, no code fences.)"
            );
            let raw2 = llm.generate(req_retry).await?;
            parse_raw_action(&raw2).map_err(|e2| {
                ActionError::Parse(format!("primary: {e}; retry: {e2}"))
            })?
        }
    };

    Ok(parsed)
}

fn parse_raw_action(raw: &str) -> Result<ActionCommand, String> {
    let start = raw.find('{').ok_or_else(|| "no '{' in output".to_string())?;
    let end = raw.rfind('}').ok_or_else(|| "no '}' in output".to_string())?;
    if end <= start {
        return Err(format!("malformed braces: start={start} end={end}"));
    }
    let slice = &raw[start..=end];
    serde_json::from_str::<ActionCommand>(slice).map_err(|e| e.to_string())
}

/// Launch a desktop application by user-facing name.
///
/// This is the one action with no cross-platform primitive: `open -a` is a macOS
/// idiom, and the opener plugin opens *paths and URLs*, not app names. So it
/// stays cfg-split.
#[cfg(target_os = "macos")]
fn launch_app_by_name(app_name: &str) -> Result<(), ActionError> {
    let status = std::process::Command::new("open")
        .arg("-a")
        .arg(app_name)
        .status()
        .map_err(|e| ActionError::Execute(e.to_string()))?;
    if !status.success() {
        return Err(ActionError::Execute(format!(
            "Failed to launch app '{app_name}'. Check if it is installed."
        )));
    }
    Ok(())
}

/// Windows equivalent of `open -a`: `start` resolves names registered under
/// App Paths (e.g. "chrome", "notepad", "excel") as well as anything on PATH.
///
/// The empty `""` argument is the window *title* that `start` otherwise steals
/// from a quoted program name — omit it and `start "Chrome"` opens a console
/// titled "Chrome" instead of launching anything.
#[cfg(target_os = "windows")]
fn launch_app_by_name(app_name: &str) -> Result<(), ActionError> {
    let status = std::process::Command::new("cmd")
        .args(["/C", "start", "", app_name])
        .status()
        .map_err(|e| ActionError::Execute(e.to_string()))?;
    if !status.success() {
        return Err(ActionError::Execute(format!(
            "Failed to launch app '{app_name}'. Check if it is installed."
        )));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launch_app_by_name(app_name: &str) -> Result<(), ActionError> {
    Err(ActionError::Execute(format!(
        "Launching applications by name isn't supported on this platform (wanted '{app_name}')."
    )))
}

/// Execute a classified action on behalf of the user.
///
/// URLs and `mailto:` drafts go through the opener plugin, which is
/// cross-platform and — unlike shelling out to `cmd /C start` on Windows —
/// does not treat the `&` between `?subject=` and `&body=` as a command
/// separator and silently truncate the email.
pub fn execute_action(app: &AppHandle, cmd: &ActionCommand) -> Result<String, ActionError> {
    let action_type = cmd.action_type.as_deref().unwrap_or("");
    info!(action_type, "executing voice action command");

    match action_type {
        "launch_app" => {
            let app_name = cmd.app_name.as_deref().ok_or_else(|| {
                ActionError::Execute("launch_app missing app_name".to_string())
            })?;
            
            launch_app_by_name(app_name)?;

            // Increment action stats
            increment_stats(app)?;

            Ok(format!("Launched application '{}'", app_name))
        }
        "draft_email" => {
            let to = cmd.email_to.as_deref().unwrap_or("");
            let subject = cmd.email_subject.as_deref().unwrap_or("");
            let body = cmd.email_body.as_deref().unwrap_or("");

            // Normalize spoken email recipient (removes spacing and spelling hyphens)
            let normalized_to = normalize_spoken_email(to);

            // URL encode headers using percent encoding (fixes spaces turning into plus signs)
            let encoded_subject = percent_encode(subject);
            let encoded_body = percent_encode(body);
            
            let mailto_url = format!(
                "mailto:{}?subject={}&body={}",
                normalized_to, encoded_subject, encoded_body
            );

            app.opener()
                .open_url(&mailto_url, None::<&str>)
                .map_err(|e| {
                    ActionError::Execute(format!("Failed to open mail client draft: {e}"))
                })?;

            increment_stats(app)?;

            Ok(format!(
                "Drafted email to '{}' with subject '{}'",
                if normalized_to.is_empty() { "default recipient".to_string() } else { normalized_to },
                if subject.is_empty() { "no subject" } else { subject }
            ))
        }
        "open_url" => {
            let raw_url = cmd.url.as_deref().ok_or_else(|| {
                ActionError::Execute("open_url missing url parameter".to_string())
            })?;

            // Simple sanitization: prepends https:// if schema is missing
            let mut sanitized_url = raw_url.to_string();
            if !sanitized_url.starts_with("http://") && !sanitized_url.starts_with("https://") && !sanitized_url.starts_with("mailto:") {
                sanitized_url = format!("https://{}", sanitized_url);
            }

            app.opener()
                .open_url(&sanitized_url, None::<&str>)
                .map_err(|e| {
                    ActionError::Execute(format!("Failed to open URL '{sanitized_url}': {e}"))
                })?;

            increment_stats(app)?;

            Ok(format!("Opened URL '{}'", sanitized_url))
        }
        "increment_counter" => {
            let app_state = app.try_state::<AppState>().ok_or_else(|| {
                ActionError::Settings("app state unavailable".to_string())
            })?;
            let next_val = app_state
                .settings
                .increment_action_counter()
                .map_err(|e| ActionError::Settings(e.to_string()))?;

            Ok(format!("Incremented action counter. Current count: {}", next_val))
        }
        "reset_counter" => {
            let app_state = app.try_state::<AppState>().ok_or_else(|| {
                ActionError::Settings("app state unavailable".to_string())
            })?;
            app_state
                .settings
                .set_action_counter(0)
                .map_err(|e| ActionError::Settings(e.to_string()))?;

            Ok("Action counter has been reset to 0".to_string())
        }
        "show_counter" => {
            let app_state = app.try_state::<AppState>().ok_or_else(|| {
                ActionError::Settings("app state unavailable".to_string())
            })?;
            let count = app_state.settings.action_counter();

            Ok(format!("The current action counter value is {}", count))
        }
        _ => Err(ActionError::Execute(format!(
            "Unsupported or unrecognized action type: '{}'",
            action_type
        ))),
    }
}

fn increment_stats(app: &AppHandle) -> Result<(), ActionError> {
    if let Some(app_state) = app.try_state::<AppState>() {
        app_state
            .settings
            .increment_action_counter()
            .map_err(|e| ActionError::Settings(e.to_string()))?;
    }
    Ok(())
}

/// Normalize spoken email addresses that contain spelling hyphens and spaces
pub fn normalize_spoken_email(raw: &str) -> String {
    let mut email = raw.to_lowercase();
    
    // Replace spoken "@" representations
    email = email.replace(" at ", "@");
    email = email.replace(" [at] ", "@");
    email = email.replace(" (at) ", "@");
    
    // Remove all whitespace
    email.retain(|c| !c.is_whitespace());
    
    if let Some(pos) = email.find('@') {
        let local = &email[..pos];
        let domain = &email[pos + 1..];
        
        // Count hyphens in local part
        let hyphen_count = local.chars().filter(|&c| c == '-').count();
        
        // If there are multiple hyphens (e.g. 2 or more), strip all hyphens from the local part.
        let clean_local = if hyphen_count >= 2 {
            local.replace('-', "")
        } else {
            local.to_string()
        };
        
        // Clean domain part (e.g. remove spelling hyphens, but usually domain is simple)
        let clean_domain = domain.replace('-', "");
        
        format!("{}@{}", clean_local, clean_domain)
    } else {
        // Fallback if no @ was found: remove hyphens if it looks spelled out
        let hyphen_count = email.chars().filter(|&c| c == '-').count();
        if hyphen_count >= 2 {
            email.replace('-', "")
        } else {
            email
        }
    }
}

/// Standard percent-encoding (RFC 3986) to encode spaces as %20 instead of +
fn percent_encode(s: &str) -> String {
    let mut encoded = String::new();
    for b in s.as_bytes() {
        match *b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*b as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", b));
            }
        }
    }
    encoded
}

/// Run the second-stage reformat: feed the matched template's system prompt
/// + the user's dictation body to the LLM and return the rewritten text. On
/// LLM failure the caller should fall back to pasting the raw dictation.
pub async fn format_text<L: LlmGenerator + ?Sized>(
    llm: &L,
    template: &FormatTemplate,
    body: &str,
) -> Result<String, ActionError> {
    let req = GenerateRequest {
        system: Some(template.system_prompt.clone()),
        user: body.to_string(),
        history: Vec::new(),
        max_tokens: 1024,
        temperature: 0.3,
        stop_strings: Vec::new(),
        grammar_gbnf: None,
        n_ctx: Some(4096),
    };
    let raw = llm.generate(req).await?;
    let trimmed = raw.trim().to_string();
    Ok(trimmed)
}

/// Check if the transcript starts with a command trigger word (e.g., "echo", "eco", "hecho", "ekko").
/// Returns Some(stripped_command) if a trigger is detected, or None if it's regular dictation.
pub fn strip_trigger_prefix(text: &str) -> Option<String> {
    let text_trimmed = text.trim();
    let text_lower = text_trimmed.to_lowercase();
    for trigger in &["echo", "eco", "hecho", "ekko"] {
        if text_lower.starts_with(trigger) {
            let trigger_len = trigger.len();
            if text_lower.len() == trigger_len {
                return Some(String::new());
            }
            if let Some(c) = text_lower.chars().nth(trigger_len) {
                if c.is_whitespace() || c.is_ascii_punctuation() {
                    let remaining = &text_trimmed[trigger_len..];
                    let remaining_trimmed = remaining.trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
                    return Some(remaining_trimmed.to_string());
                }
            }
        }
    }
    None
}

