use std::thread;
use std::time::Duration;
use thiserror::Error;
use tracing::{info, warn};

/// Base milliseconds to wait after sending Cmd+V before restoring the clipboard.
/// The actual delay is scaled up for longer texts (see `restore_delay_ms`).
const RESTORE_DELAY_BASE_MS: u64 = 300;
/// Maximum restore delay cap.
const RESTORE_DELAY_MAX_MS: u64 = 1500;

/// Compute how long to wait after Cmd+V before restoring the original clipboard.
///
/// Short transcriptions: 300ms is plenty. Long ones (from extended recordings)
/// mean the target app was in App Nap for longer, so its run loop has more
/// activation backlog to drain before it can process the Cmd+V and read the
/// clipboard. We scale by text length as a proxy for recording duration.
fn restore_delay_ms(text_len: usize) -> u64 {
    // +1ms per 3 chars beyond the first 100, capped at 1500ms total.
    let extra = (text_len.saturating_sub(100) as u64) / 3;
    (RESTORE_DELAY_BASE_MS + extra).min(RESTORE_DELAY_MAX_MS)
}

#[derive(Debug, Error)]
pub enum PasteError {
    #[error("failed to set clipboard: {0}")]
    Clipboard(String),
    #[error("failed to synthesize keystroke: {0}")]
    Keystroke(String),
    #[error("failed to initialize enigo: {0}")]
    Init(String),
}

/// Copies `text` to the clipboard and synthesizes Cmd+V (macOS) /
/// Ctrl+V (other platforms) to paste at the focused application's cursor.
///
/// Preserves the user's existing clipboard content: saves it before
/// overwriting, then restores it after the paste keystroke lands.
pub fn paste_at_cursor(text: &str) -> Result<(), PasteError> {
    use arboard::Clipboard;

    let mut clipboard = Clipboard::new().map_err(|e| PasteError::Clipboard(e.to_string()))?;

    // ── Save original clipboard ──────────────────────────────────
    let original = clipboard.get_text().ok(); // None if clipboard is empty or non-text
    if original.is_some() {
        info!("saved existing clipboard content for restoration");
    }

    // ── Write transcription to clipboard ─────────────────────────
    clipboard
        .set_text(text)
        .map_err(|e| PasteError::Clipboard(e.to_string()))?;
    info!(len = text.len(), "set clipboard text");

    // ── Synthesize paste keystroke ────────────────────────────────
    synthesize_cmd_v()?;

    // ── Restore original clipboard ───────────────────────────────
    // Wait for the target app to process the paste event, then put
    // the user's original content back. The delay scales with text
    // length because longer dictations mean the target app was in
    // background (App Nap) longer and needs more time after activation
    // to drain its event backlog before it reads the clipboard.
    if let Some(original_text) = original {
        let delay = restore_delay_ms(text.len());
        info!(delay_ms = delay, text_len = text.len(), "waiting before clipboard restore");
        thread::sleep(Duration::from_millis(delay));
        // Best-effort restore — don't fail the transcription if this errors.
        match clipboard.set_text(&original_text) {
            Ok(()) => info!("restored original clipboard content"),
            Err(e) => warn!(?e, "failed to restore original clipboard content"),
        }
    }

    Ok(())
}

/// Synthesizes Cmd+V (macOS) or Ctrl+V (other platforms).
///
/// On macOS we use CoreGraphics directly and set CGEventFlagCommand on the V
/// keydown event itself rather than relying on a separate modifier press.
/// The two-step enigo approach (press Meta, click V, release Meta) is
/// racy: CGEventPost is asynchronous, so the V keydown can be dispatched
/// before the global CombinedSessionState has registered the Command flag,
/// causing the target app to receive plain "v" instead of a paste.
/// Setting the flag directly on the event is deterministic.
///
/// We post at CGEventTapLocation::Session so the events bypass our own
/// HID-level CGEventTap and go straight to the focused application.
#[cfg(target_os = "macos")]
fn synthesize_cmd_v() -> Result<(), PasteError> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| PasteError::Keystroke("failed to create CGEventSource".into()))?;

    // kVK_ANSI_V = 9
    let v_down = CGEvent::new_keyboard_event(source.clone(), 9, true)
        .map_err(|_| PasteError::Keystroke("failed to create V keydown event".into()))?;
    v_down.set_flags(CGEventFlags::CGEventFlagCommand);
    v_down.post(CGEventTapLocation::Session);

    thread::sleep(Duration::from_millis(20));

    let v_up = CGEvent::new_keyboard_event(source, 9, false)
        .map_err(|_| PasteError::Keystroke("failed to create V keyup event".into()))?;
    v_up.post(CGEventTapLocation::Session);

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn synthesize_cmd_v() -> Result<(), PasteError> {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut enigo =
        Enigo::new(&Settings::default()).map_err(|e| PasteError::Init(e.to_string()))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    enigo
        .key(Key::Control, Direction::Release)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    Ok(())
}

/// Pure decision helper: given the clipboard text before and after a synthetic
/// Cmd+C, return the selected text — or `None` if the clipboard did not change
/// (which we treat as "nothing was selected"). Kept pure so it is unit-testable
/// without a live app.
///
/// ⚠️ Unsound when `before` is `None`: a failed pre-read makes *any* stale
/// clipboard content compare as "changed". Prefer
/// [`selection_from_sentinel`], which does not depend on reading the
/// user's clipboard at all. Retained because it is the right check when the
/// pre-read is known to have succeeded.
pub fn selection_from_clipboard_delta(before: Option<&str>, after: Option<&str>) -> Option<String> {
    match after {
        Some(a) if !a.is_empty() && Some(a) != before => Some(a.to_string()),
        _ => None,
    }
}

/// Marker written to the clipboard immediately before the synthetic copy.
///
/// Deliberately not empty: `set_text("")` is unreliable across platforms
/// (some clipboards drop the entry entirely rather than storing an empty
/// string), which would put us right back to guessing.
const COPY_SENTINEL: &str = "\u{200B}echo-scribe-copy-probe\u{200B}";

/// Pure decision helper: did the synthetic copy actually replace our sentinel?
///
/// This is the sound version of [`selection_from_clipboard_delta`]. Because we
/// *wrote* the pre-state ourselves, "unchanged" is provable rather than
/// inferred, so a clipboard whose pre-read failed can no longer be mistaken for
/// a fresh selection. Fixes real Windows behaviour: `capture_selection` grabbed
/// a URL copied minutes earlier and handed it to the editor as if the user had
/// selected it.
pub fn selection_from_sentinel(after: Option<&str>) -> Option<String> {
    match after {
        Some(a) if !a.is_empty() && a != COPY_SENTINEL => Some(a.to_string()),
        _ => None,
    }
}

/// Fallback selection capture: synthesize the platform copy chord, read the
/// clipboard, then restore the user's original clipboard. Returns the selected
/// text, or `None` if nothing changed / the copy failed.
///
/// Platform-neutral: only [`synthesize_cmd_c`] differs per OS (CoreGraphics on
/// macOS, enigo elsewhere). On Windows this is the *only* selection path, since
/// the AX-based `focus::capture_selection` has no Win32 equivalent yet.
pub fn capture_selection_via_copy() -> Option<String> {
    use arboard::Clipboard;
    let mut clipboard = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "edit", ?e, "capture_selection_via_copy: clipboard unavailable");
            return None;
        }
    };
    let before = clipboard.get_text().ok();

    // Overwrite with a known marker so "nothing was copied" is provable. Without
    // this, a failed `before` read (routine on Windows — the clipboard is one
    // global resource that browsers and clipboard managers lock constantly) makes
    // leftover content look like a brand-new selection.
    if let Err(e) = clipboard.set_text(COPY_SENTINEL) {
        warn!(target: "edit", ?e, "capture_selection_via_copy: failed to write probe sentinel");
        return None;
    }

    if let Err(e) = synthesize_cmd_c() {
        warn!(target: "edit", ?e, "capture_selection_via_copy: copy synthesis failed");
        restore_clipboard(&mut clipboard, before.as_deref());
        return None;
    }
    // Give the frontmost app time to service the copy and write the pasteboard.
    thread::sleep(Duration::from_millis(120));
    let after = clipboard.get_text().ok();
    let result = selection_from_sentinel(after.as_deref());
    if result.is_none() {
        info!(
            target: "edit",
            sentinel_intact = after.as_deref() == Some(COPY_SENTINEL),
            after_readable = after.is_some(),
            "capture_selection_via_copy: nothing was copied (no active selection?)"
        );
    }
    restore_clipboard(&mut clipboard, before.as_deref());
    result
}

/// Put the user's clipboard back. Always attempted, including on the failure
/// paths — we overwrote it with a sentinel, so bailing out early without this
/// would leave the probe marker sitting in their clipboard.
fn restore_clipboard(clipboard: &mut arboard::Clipboard, before: Option<&str>) {
    match before {
        Some(orig) => {
            if let Err(e) = clipboard.set_text(orig) {
                warn!(target: "edit", ?e, "capture_selection_via_copy: failed to restore clipboard");
            }
        }
        None => {
            // We never managed to read it, so we cannot put it back. Clearing is
            // still better than leaving our sentinel behind.
            if let Err(e) = clipboard.set_text("") {
                warn!(target: "edit", ?e, "capture_selection_via_copy: failed to clear sentinel");
            }
        }
    }
}


/// Synthesize Cmd+C via CoreGraphics (same deterministic approach as
/// [`synthesize_cmd_v`], with the Command flag set directly on the keydown).
#[cfg(target_os = "macos")]
fn synthesize_cmd_c() -> Result<(), PasteError> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| PasteError::Keystroke("failed to create CGEventSource".into()))?;

    // kVK_ANSI_C = 8
    let c_down = CGEvent::new_keyboard_event(source.clone(), 8, true)
        .map_err(|_| PasteError::Keystroke("failed to create C keydown event".into()))?;
    c_down.set_flags(CGEventFlags::CGEventFlagCommand);
    c_down.post(CGEventTapLocation::Session);

    thread::sleep(Duration::from_millis(20));

    let c_up = CGEvent::new_keyboard_event(source, 8, false)
        .map_err(|_| PasteError::Keystroke("failed to create C keyup event".into()))?;
    c_up.post(CGEventTapLocation::Session);

    Ok(())
}

/// Synthesize Ctrl+C. Mirrors [`synthesize_cmd_v`]'s non-macOS enigo path so
/// the two chords behave identically on Windows/Linux.
#[cfg(not(target_os = "macos"))]
fn synthesize_cmd_c() -> Result<(), PasteError> {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut enigo =
        Enigo::new(&Settings::default()).map_err(|e| PasteError::Init(e.to_string()))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    enigo
        .key(Key::Unicode('c'), Direction::Click)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    enigo
        .key(Key::Control, Direction::Release)
        .map_err(|e| PasteError::Keystroke(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this replaces: a failed `before` read made stale clipboard
    /// content look like a fresh selection. Observed on Windows as a URL copied
    /// minutes earlier being handed to the selection editor.
    #[test]
    fn sentinel_intact_means_nothing_was_copied() {
        assert_eq!(selection_from_sentinel(Some(COPY_SENTINEL)), None);
    }

    #[test]
    fn sentinel_replaced_means_real_selection() {
        assert_eq!(
            selection_from_sentinel(Some("the highlighted sentence")),
            Some("the highlighted sentence".to_string())
        );
    }

    #[test]
    fn sentinel_unreadable_or_empty_is_not_a_selection() {
        assert_eq!(selection_from_sentinel(None), None);
        assert_eq!(selection_from_sentinel(Some("")), None);
    }

    /// Regression guard on the old helper: it cannot distinguish stale content
    /// from a real selection once `before` is unknown. Documents *why*
    /// `capture_selection_via_copy` no longer uses it.
    #[test]
    fn clipboard_delta_is_unsound_without_a_known_before() {
        assert_eq!(
            selection_from_clipboard_delta(None, Some("https://github.com/")),
            Some("https://github.com/".to_string()),
            "documents the unsoundness that motivated the sentinel probe"
        );
    }

    #[test]
    fn paste_error_display_messages() {
        let e = PasteError::Clipboard("test".into());
        assert!(e.to_string().contains("clipboard"));

        let e = PasteError::Keystroke("test".into());
        assert!(e.to_string().contains("keystroke"));

        let e = PasteError::Init("test".into());
        assert!(e.to_string().contains("enigo"));
    }

    #[test]
    fn paste_error_init_display() {
        let e = PasteError::Init("bad driver".into());
        assert_eq!(e.to_string(), "failed to initialize enigo: bad driver");
    }

    #[test]
    fn restore_delay_is_reasonable() {
        // Short text: base delay, well above the minimum for app responsiveness.
        let short = restore_delay_ms(50);
        assert!(short >= 200, "base delay too low: {short}");
        assert!(short <= 500, "base delay too high: {short}");

        // Long text: delay grows but stays capped.
        let long = restore_delay_ms(5000);
        assert!(long > short, "long text should have longer delay");
        assert!(long <= RESTORE_DELAY_MAX_MS, "delay must not exceed cap: {long}");

        // Cap is always enforced.
        assert_eq!(restore_delay_ms(usize::MAX), RESTORE_DELAY_MAX_MS);
    }

    #[test]
    fn clipboard_delta_detects_new_selection() {
        // Selection changed the clipboard → that's the selection.
        assert_eq!(
            selection_from_clipboard_delta(Some("old"), Some("selected text")),
            Some("selected text".to_string())
        );
        // Nothing selected → Cmd+C leaves clipboard unchanged → None.
        assert_eq!(selection_from_clipboard_delta(Some("old"), Some("old")), None);
        // Empty after → None.
        assert_eq!(selection_from_clipboard_delta(Some("old"), Some("")), None);
        // Previously-empty clipboard, now populated → Some.
        assert_eq!(selection_from_clipboard_delta(None, Some("x")), Some("x".to_string()));
    }
}
