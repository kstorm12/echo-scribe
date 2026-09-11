//! Windows UI Automation selection reader — the Win32 counterpart to macOS
//! `AXSelectedText`.
//!
//! Reads the focused element's live text selection through
//! `IUIAutomationTextPattern::GetSelection`, synthesizing **no input at all**.
//! That property is the whole point of this module.
//!
//! The edit-selection hotkey captures at PRESS time, while the user is still
//! physically holding the trigger. The obvious shortcut — synthesize Ctrl+C and
//! read the clipboard — is unusable there: `enigo` ends its chord by releasing
//! Ctrl, so Windows sees Ctrl go up, the still-held primary key stops matching
//! the `RegisterHotKey` combination, and its auto-repeat is delivered to the
//! focused app as a stream of characters typed into the user's document. That
//! path was tried, shipped, and reverted (commit 9856d20). Reading the
//! selection out of band is the fix.
//!
//! Caveat worth knowing when reading logs: `TextPattern` is a per-control
//! opt-in. Standard Win32 edits, RichEdit, UWP/WinUI text, Office, and
//! Chromium-based apps (Chrome, Edge, Electron, VS Code) expose it; a few
//! custom-drawn editors (some terminals, some Java/Qt apps) do not. When it is
//! missing we return `None` and the coordinator shows its "select text first"
//! hint, which is why every failure step below logs a distinct reason.

use std::sync::mpsc;
use std::time::Duration;

use tracing::{info, warn};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
};

/// Hard deadline for the whole UIA round-trip. A hung or busy provider (an app
/// pumping a modal loop, a slow Electron a11y server) must never stall the
/// hotkey — the press handler is on the coordinator's critical path.
const UIA_DEADLINE: Duration = Duration::from_millis(600);

/// Read the current selection from the focused UI element.
///
/// Runs the COM work on a throwaway thread so a wedged provider costs us
/// `UIA_DEADLINE` and nothing more. Only the resulting `String` crosses the
/// thread boundary; every COM pointer is created, used, and dropped inside the
/// worker, which is what keeps this sound despite COM interfaces being `!Send`.
pub fn selected_text() -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_selection());
    });

    match rx.recv_timeout(UIA_DEADLINE) {
        Ok(result) => result,
        Err(_) => {
            warn!(
                target: "edit",
                deadline_ms = UIA_DEADLINE.as_millis() as u64,
                "uia: selection read timed out; treating as no selection"
            );
            None
        }
    }
}

/// COM lifecycle wrapper. Initializes an STA on this worker thread (UIA
/// clients are documented to work from either apartment; STA matches what the
/// providers we talk to expect) and always uninitializes, including on the
/// error paths.
fn read_selection() -> Option<String> {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            warn!(target: "edit", error = %e, "uia: CoInitializeEx failed");
            return None;
        }
        let result = read_selection_com();
        CoUninitialize();
        result
    }
}

unsafe fn read_selection_com() -> Option<String> {
    let automation: IUIAutomation =
        match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(a) => a,
            Err(e) => {
                warn!(target: "edit", error = %e, "uia: CoCreateInstance(CUIAutomation) failed");
                return None;
            }
        };

    let element = match automation.GetFocusedElement() {
        Ok(el) => el,
        Err(e) => {
            info!(target: "edit", error = %e, "uia: GetFocusedElement failed");
            return None;
        }
    };

    // `GetCurrentPatternAs` does the QueryInterface for us, so an element that
    // simply doesn't implement TextPattern surfaces as an Err here rather than
    // as a silently-null IUnknown from the plain `GetCurrentPattern`.
    let text_pattern: IUIAutomationTextPattern =
        match element.GetCurrentPatternAs(UIA_TextPatternId) {
            Ok(p) => p,
            Err(e) => {
                info!(
                    target: "edit",
                    error = %e,
                    "uia: focused element does not support TextPattern"
                );
                return None;
            }
        };

    let ranges = match text_pattern.GetSelection() {
        Ok(r) => r,
        Err(e) => {
            info!(target: "edit", error = %e, "uia: GetSelection failed");
            return None;
        }
    };
    let count = ranges.Length().unwrap_or(0);
    if count <= 0 {
        info!(target: "edit", "uia: no selection ranges on focused element");
        return None;
    }

    // Disjoint selections (a multi-select grid, a column selection) come back
    // as several ranges. Join them so we edit everything the user highlighted
    // rather than silently dropping all but the first block.
    let mut out = String::new();
    for i in 0..count {
        let Ok(range) = ranges.GetElement(i) else {
            continue;
        };
        // -1 = no length cap. The coordinator enforces the real limit via
        // `llm::edit::within_length_limit`, which reports it to the user.
        let Ok(bstr) = range.GetText(-1) else {
            continue;
        };
        let chunk = bstr.to_string();
        if chunk.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&chunk);
    }

    if out.trim().is_empty() {
        info!(
            target: "edit",
            ranges = count,
            "uia: selection ranges are empty (caret only, no highlighted text)"
        );
        return None;
    }

    info!(
        target: "edit",
        ranges = count,
        chars = out.len(),
        "uia: read selection via TextPattern"
    );
    Some(out)
}
