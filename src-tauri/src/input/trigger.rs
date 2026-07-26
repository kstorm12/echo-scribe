//! Cross-platform dictation trigger helpers.
//!
//! macOS uses the CGEventTap listener in `hotkeys.rs`. Windows/Linux have no
//! event tap, so we drive the coordinator from the global-shortcut plugin.
//! Both funnel into `CoordinatorMsg::Hotkey`.
//!
//! On Windows/Linux the shortcuts are derived from the *user's configured
//! bindings* (the same ones the Settings UI edits), not a hardcoded default.
//! [`sync_shortcuts`] is idempotent: it clears every registration and rebuilds
//! it from the current bindings, so it can be called at pipeline start and
//! again after any rebind.

use crate::input::binding::{code_from_key, Binding, ModifierKind};
use crate::input::hotkeys::{is_modifier, HotkeyEvent};

/// Map a "is the trigger currently active?" boolean to a coordinator hotkey
/// transition. `true` => `Pressed` (start capture), `false` => `Released`
/// (stop + transcribe + paste). Shared by the global shortcut and the button.
pub fn shortcut_state_to_hotkey(pressed: bool) -> HotkeyEvent {
    if pressed {
        HotkeyEvent::Pressed
    } else {
        HotkeyEvent::Released
    }
}

/// A hotkey that could not be registered, tagged with the action it belongs to
/// so the settings UI can show the message next to the shortcut that caused it
/// instead of dumping every problem under all four rebinders.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HotkeyProblem {
    /// Stable action slug: "voice-at-cursor" | "log-capture" | "action" |
    /// "edit-selection". Matches the `action` prop of `HotkeyRebinder`.
    pub action: String,
    /// Friendly, already-formatted explanation for the user.
    pub message: String,
}

/// Why a configured binding cannot become a Windows/Linux global shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedBinding {
    /// The primary key is a bare modifier (the macOS default is Right Control
    /// on its own). `RegisterHotKey` requires a non-modifier key, so this
    /// shape is unrepresentable on Windows.
    ModifierOnly,
    /// Another Echo Scribe action already claimed this exact combination.
    /// The macOS event tap happily lets two actions share a binding;
    /// `RegisterHotKey` rejects the duplicate, so we have to say which of our
    /// own shortcuts is in the way rather than blaming another app.
    DuplicateOf(&'static str),
    /// The key has no DOM-code mapping, so we can't translate it.
    UnmappableKey,
}

impl UnsupportedBinding {
    /// A short, human-readable explanation for the settings UI. `label` names
    /// the action, e.g. "voice-at-cursor".
    pub fn message(self, label: &str) -> String {
        match self {
            Self::ModifierOnly => format!(
                "The {label} shortcut is a single modifier key. Windows can only \
                 register a shortcut that includes a regular key — add a letter, \
                 digit or function key to the combination."
            ),
            Self::UnmappableKey => {
                format!("The {label} shortcut uses a key Windows can't register. Pick another key.")
            }
            Self::DuplicateOf(other) => format!(
                "The {label} shortcut is the same combination as the {other} shortcut. Windows \
                 needs a different combination for each action."
            ),
        }
    }
}

/// Split a binding into the DOM `KeyboardEvent.code` of its primary key plus
/// the distinct modifier kinds it requires.
///
/// Pure and platform-independent so it unit-tests on every host, including the
/// macOS CI runner. Note that `ModifierSide` is deliberately discarded:
/// Windows' `RegisterHotKey` has no notion of left-vs-right modifiers, so a
/// binding of "Right Alt + Slash" registers as "Alt + Slash" there.
pub fn shortcut_parts(
    binding: &Binding,
) -> Result<(&'static str, Vec<ModifierKind>), UnsupportedBinding> {
    if is_modifier(binding.primary.0) {
        return Err(UnsupportedBinding::ModifierOnly);
    }
    let code = code_from_key(binding.primary.0).ok_or(UnsupportedBinding::UnmappableKey)?;

    let mut kinds: Vec<ModifierKind> = Vec::new();
    for (kind, _side) in &binding.modifiers {
        if !kinds.contains(kind) {
            kinds.push(*kind);
        }
    }
    // Canonical order, so two bindings that require the same modifiers compare
    // equal regardless of the order the user pressed them in. Duplicate
    // detection in `sync_shortcuts` relies on this.
    kinds.sort_by_key(|k| modifier_rank(*k));
    Ok((code, kinds))
}

fn modifier_rank(kind: ModifierKind) -> u8 {
    match kind {
        ModifierKind::Control => 0,
        ModifierKind::Shift => 1,
        ModifierKind::Alt => 2,
        ModifierKind::Meta => 3,
    }
}

#[cfg(not(target_os = "macos"))]
pub use platform::sync_shortcuts;

#[cfg(not(target_os = "macos"))]
mod platform {
    use tauri::AppHandle;
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    use super::{shortcut_parts, shortcut_state_to_hotkey, HotkeyProblem};
    use crate::commands::AppState;
    use crate::coordinator::{Action, CoordinatorMsg};
    use crate::input::binding::{Binding, ModifierKind};

    /// The bindings we register, paired with the action they drive and the
    /// label used in user-facing problem messages.
    fn configured(state: &AppState) -> Vec<(Action, &'static str, Binding)> {
        let read = |lock: &std::sync::RwLock<Binding>| lock.read().ok().map(|g| g.clone());
        let mut out = Vec::new();
        if let Some(b) = read(&state.binding) {
            out.push((Action::VoiceAtCursor, "voice-at-cursor", b));
        }
        if let Some(b) = read(&state.log_capture_binding) {
            out.push((Action::LogCapture, "log-capture", b));
        }
        if let Some(b) = read(&state.action_binding) {
            out.push((Action::ActionCommand, "action", b));
        }
        if let Some(b) = read(&state.edit_selection_binding) {
            out.push((Action::EditSelection, "edit-selection", b));
        }
        out
    }

    fn modifiers_for(kinds: &[ModifierKind]) -> Modifiers {
        let mut mods = Modifiers::empty();
        for kind in kinds {
            mods |= match kind {
                ModifierKind::Control => Modifiers::CONTROL,
                ModifierKind::Shift => Modifiers::SHIFT,
                ModifierKind::Alt => Modifiers::ALT,
                ModifierKind::Meta => Modifiers::META,
            };
        }
        mods
    }

    /// DOM `KeyboardEvent.code` string -> `Code`.
    ///
    /// Explicit rather than `Code::from_str` so an unmappable key is a
    /// `None` we report, never a panic or a silently-wrong key. The input
    /// strings come from `binding::code_from_key`, so this table mirrors it.
    fn code_for(code: &str) -> Option<Code> {
        Some(match code {
            "Space" => Code::Space,
            "Tab" => Code::Tab,
            "Escape" => Code::Escape,
            "Enter" => Code::Enter,
            "Backspace" => Code::Backspace,
            "KeyA" => Code::KeyA,
            "KeyB" => Code::KeyB,
            "KeyC" => Code::KeyC,
            "KeyD" => Code::KeyD,
            "KeyE" => Code::KeyE,
            "KeyF" => Code::KeyF,
            "KeyG" => Code::KeyG,
            "KeyH" => Code::KeyH,
            "KeyI" => Code::KeyI,
            "KeyJ" => Code::KeyJ,
            "KeyK" => Code::KeyK,
            "KeyL" => Code::KeyL,
            "KeyM" => Code::KeyM,
            "KeyN" => Code::KeyN,
            "KeyO" => Code::KeyO,
            "KeyP" => Code::KeyP,
            "KeyQ" => Code::KeyQ,
            "KeyR" => Code::KeyR,
            "KeyS" => Code::KeyS,
            "KeyT" => Code::KeyT,
            "KeyU" => Code::KeyU,
            "KeyV" => Code::KeyV,
            "KeyW" => Code::KeyW,
            "KeyX" => Code::KeyX,
            "KeyY" => Code::KeyY,
            "KeyZ" => Code::KeyZ,
            "Digit0" => Code::Digit0,
            "Digit1" => Code::Digit1,
            "Digit2" => Code::Digit2,
            "Digit3" => Code::Digit3,
            "Digit4" => Code::Digit4,
            "Digit5" => Code::Digit5,
            "Digit6" => Code::Digit6,
            "Digit7" => Code::Digit7,
            "Digit8" => Code::Digit8,
            "Digit9" => Code::Digit9,
            "F1" => Code::F1,
            "F2" => Code::F2,
            "F3" => Code::F3,
            "F4" => Code::F4,
            "F5" => Code::F5,
            "F6" => Code::F6,
            "F7" => Code::F7,
            "F8" => Code::F8,
            "F9" => Code::F9,
            "F10" => Code::F10,
            "F11" => Code::F11,
            "F12" => Code::F12,
            "Period" => Code::Period,
            "Comma" => Code::Comma,
            "Semicolon" => Code::Semicolon,
            "Quote" => Code::Quote,
            "BracketLeft" => Code::BracketLeft,
            "BracketRight" => Code::BracketRight,
            "Backslash" => Code::Backslash,
            "Slash" => Code::Slash,
            "Minus" => Code::Minus,
            "Equal" => Code::Equal,
            "Backquote" => Code::Backquote,
            _ => return None,
        })
    }

    /// Re-register every global shortcut from the user's current bindings.
    ///
    /// Returns one friendly message per binding that could NOT be registered —
    /// either because Windows can't express it, or because another running
    /// application already owns the combination. The caller stores these so the
    /// settings UI can show them; the technical detail goes to the log.
    ///
    /// Safe to call repeatedly: it unregisters everything first.
    pub fn sync_shortcuts(app: &AppHandle, state: &AppState) -> Vec<HotkeyProblem> {
        let mut problems: Vec<HotkeyProblem> = Vec::new();
        let problem = |action: &str, message: String| HotkeyProblem {
            action: action.to_string(),
            message,
        };

        if let Err(e) = app.global_shortcut().unregister_all() {
            tracing::warn!(target: "trigger", ?e, "failed to clear existing global shortcuts");
        }

        let coord_tx = match state.coord_tx.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => {
                tracing::warn!(target: "trigger", "coordinator channel lock poisoned");
                None
            }
        };
        let Some(coord_tx) = coord_tx else {
            // The pipeline hasn't started yet; ensure_pipeline_started will
            // call us again once the channel exists.
            tracing::info!(target: "trigger", "no coordinator channel yet; skipping shortcut sync");
            return problems;
        };

        // (code, modifiers) pairs already taken, with the action that took
        // them — so a self-collision reports the real culprit.
        let mut claimed: Vec<(&'static str, Vec<ModifierKind>, &'static str)> = Vec::new();

        for (action, label, binding) in configured(state) {
            let (code, kinds) = match shortcut_parts(&binding) {
                Ok(parts) => parts,
                Err(e) => {
                    tracing::warn!(target: "trigger", %label, ?e, "binding unsupported on this platform");
                    problems.push(problem(label, e.message(label)));
                    continue;
                }
            };
            // Reject a duplicate before asking the OS, so the message names our
            // own conflicting shortcut instead of implying a foreign app.
            if let Some((_, _, other)) = claimed
                .iter()
                .find(|(c, k, _)| *c == code && *k == kinds)
            {
                let e = super::UnsupportedBinding::DuplicateOf(other);
                tracing::warn!(target: "trigger", %label, %other, "two actions share one binding");
                problems.push(problem(label, e.message(label)));
                continue;
            }
            claimed.push((code, kinds.clone(), label));

            let Some(code) = code_for(code) else {
                let e = super::UnsupportedBinding::UnmappableKey;
                tracing::warn!(target: "trigger", %label, %code, "no Code mapping for key");
                problems.push(problem(label, e.message(label)));
                continue;
            };

            let shortcut = Shortcut::new(Some(modifiers_for(&kinds)), code);
            // Rendered before `shortcut` is handed to the plugin so the log
            // lines below don't depend on it still being ours to borrow.
            let desc = format!("{shortcut:?}");
            let tx = coord_tx.clone();
            let result = app
                .global_shortcut()
                .on_shortcut(shortcut, move |_app, _shortcut, event| {
                    let ev = shortcut_state_to_hotkey(matches!(event.state(), ShortcutState::Pressed));
                    if let Err(e) = tx.send(CoordinatorMsg::Hotkey(action, ev)) {
                        tracing::warn!(target: "trigger", ?e, "failed to forward global shortcut to coordinator");
                    }
                });

            match result {
                Ok(()) => {
                    tracing::info!(target: "trigger", %label, %desc, "registered global shortcut");
                }
                Err(e) => {
                    // The overwhelmingly common cause on Windows is another
                    // running app already owning the combination —
                    // RegisterHotKey is first-come-first-served process-wide.
                    tracing::error!(target: "trigger", %label, %desc, %e, "failed to register global shortcut");
                    problems.push(problem(
                        label,
                        format!(
                            "Couldn't register the {label} shortcut. Another application is \
                             probably already using that key combination — pick a different one."
                        ),
                    ));
                }
            }
        }

        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::binding::{ModifierSide, SerKey};
    use rdev::Key;

    #[test]
    fn maps_pressed_and_released() {
        assert_eq!(shortcut_state_to_hotkey(true), HotkeyEvent::Pressed);
        assert_eq!(shortcut_state_to_hotkey(false), HotkeyEvent::Released);
    }

    #[test]
    fn combo_splits_into_code_and_modifiers() {
        let b = Binding {
            primary: SerKey(Key::Slash),
            modifiers: vec![(ModifierKind::Alt, ModifierSide::Right)],
        };
        let (code, kinds) = shortcut_parts(&b).expect("combo should be supported");
        assert_eq!(code, "Slash");
        assert_eq!(kinds, vec![ModifierKind::Alt]);
    }

    /// Left/right is meaningless to RegisterHotKey, so the same modifier bound
    /// on both sides must collapse to one entry rather than being applied twice.
    #[test]
    fn duplicate_modifier_kinds_collapse() {
        let b = Binding {
            primary: SerKey(Key::KeyK),
            modifiers: vec![
                (ModifierKind::Control, ModifierSide::Left),
                (ModifierKind::Control, ModifierSide::Right),
            ],
        };
        let (_, kinds) = shortcut_parts(&b).expect("supported");
        assert_eq!(kinds, vec![ModifierKind::Control]);
    }

    /// THE Windows gap: the shipped macOS default (bare Right Control) can't
    /// be a global shortcut, so it must be reported rather than silently
    /// dropped — that silence is what made the mic look dead on Windows.
    #[test]
    fn bare_modifier_primary_is_unsupported() {
        assert_eq!(
            shortcut_parts(&Binding::single(Key::ControlRight)),
            Err(UnsupportedBinding::ModifierOnly)
        );
        assert_eq!(
            shortcut_parts(&Binding::single(Key::AltGr)),
            Err(UnsupportedBinding::ModifierOnly)
        );
    }

    #[test]
    fn single_non_modifier_key_is_supported() {
        let (code, kinds) = shortcut_parts(&Binding::single(Key::F8)).expect("supported");
        assert_eq!(code, "F8");
        assert!(kinds.is_empty());
    }

    /// Modifier order must not affect equality: Echo Scribe's own default
    /// bindings collided on Windows (`RegisterHotKey` rejects duplicates where
    /// the macOS event tap allowed them), and duplicate detection compares
    /// these vectors.
    #[test]
    fn modifier_order_is_canonical() {
        let ctrl_shift = Binding {
            primary: SerKey(Key::KeyD),
            modifiers: vec![
                (ModifierKind::Shift, ModifierSide::Either),
                (ModifierKind::Control, ModifierSide::Either),
            ],
        };
        let shift_ctrl = Binding {
            primary: SerKey(Key::KeyD),
            modifiers: vec![
                (ModifierKind::Control, ModifierSide::Either),
                (ModifierKind::Shift, ModifierSide::Either),
            ],
        };
        assert_eq!(
            shortcut_parts(&ctrl_shift).unwrap(),
            shortcut_parts(&shift_ctrl).unwrap()
        );
    }

    #[test]
    fn duplicate_message_names_the_conflicting_action() {
        let msg = UnsupportedBinding::DuplicateOf("log-capture").message("voice-at-cursor");
        assert!(msg.contains("log-capture"), "got {msg}");
        assert!(msg.contains("voice-at-cursor"), "got {msg}");
    }

    #[test]
    fn messages_name_the_action() {
        let msg = UnsupportedBinding::ModifierOnly.message("voice-at-cursor");
        assert!(msg.contains("voice-at-cursor"), "got {msg}");
    }
}
