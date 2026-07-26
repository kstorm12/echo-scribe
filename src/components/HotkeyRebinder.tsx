import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  getHotkeyProblems,
  getVoiceAtCursorBinding,
  updateVoiceAtCursorBinding,
  type JsBinding,
  type ModKind,
  type ModSide,
} from "../lib/api";
import {
  formatBinding,
  isModifierCode,
  modifierKindFromCode,
  modifierSideFromCode,
} from "../lib/binding";

type CaptureState =
  | { kind: "idle" }
  | { kind: "capturing" }
  | { kind: "captured"; binding: JsBinding };

type Props = {
  onChange?: (binding: JsBinding) => void;
  /// Override which command pair this rebinder talks to. Defaults to the
  /// voice-at-cursor binding for backwards compatibility.
  load?: () => Promise<JsBinding>;
  save?: (b: JsBinding) => Promise<void>;
  /// Action slug used to match this rebinder against `getHotkeyProblems()`
  /// entries. Defaults to voice-at-cursor, matching `load`/`save`.
  action?: string;
};

function buildBinding(
  primaryCode: string,
  pressedCodes: Set<string>,
): JsBinding {
  const sides: Partial<Record<ModKind, Set<ModSide>>> = {};
  for (const code of pressedCodes) {
    if (code === primaryCode) continue;
    const kind = modifierKindFromCode(code);
    if (!kind) continue;
    const side = modifierSideFromCode(code);
    sides[kind] ??= new Set<ModSide>();
    sides[kind]!.add(side);
  }

  const modifiers: { kind: ModKind; side: ModSide }[] = [];
  (["Control", "Shift", "Alt", "Meta"] as ModKind[]).forEach((kind) => {
    const set = sides[kind];
    if (!set) return;
    const hasLeft = set.has("Left");
    const hasRight = set.has("Right");
    let side: ModSide;
    if (hasLeft && hasRight) side = "Either";
    else if (hasLeft) side = "Left";
    else if (hasRight) side = "Right";
    else side = "Either";
    modifiers.push({ kind, side });
  });

  return { primary: primaryCode, modifiers };
}

export default function HotkeyRebinder({
  onChange,
  load,
  save,
  action = "voice-at-cursor",
}: Props) {
  const [current, setCurrent] = useState<JsBinding | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [capture, setCapture] = useState<CaptureState>({ kind: "idle" });
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  const loader = load ?? getVoiceAtCursorBinding;
  const saver = save ?? updateVoiceAtCursorBinding;

  // Whether the OS accepted this shortcut. On Windows a combination another
  // app already owns fails to register, and without this the app looks healthy
  // while the hotkey does nothing at all.
  const refreshProblem = async () => {
    try {
      const problems = await getHotkeyProblems();
      setProblem(problems.find((p) => p.action === action)?.message ?? null);
    } catch {
      // Diagnostics only — never block the rebinder on this.
    }
  };

  // Load current binding on mount
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const b = await loader();
        if (!cancelled) setCurrent(b);
      } catch (e) {
        if (!cancelled)
          setLoadError(e instanceof Error ? e.message : String(e));
      }
      if (!cancelled) await refreshProblem();
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Capture-mode key listeners
  const pressedRef = useRef<Set<string>>(new Set());
  const lastKeyRef = useRef<string | null>(null);

  useEffect(() => {
    if (capture.kind !== "capturing") return;

    pressedRef.current = new Set();
    lastKeyRef.current = null;

    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      if (e.code === "Escape") {
        void invoke("set_rebinding", { active: false });
        setCapture({ kind: "idle" });
        return;
      }
      if (e.repeat) return;
      pressedRef.current.add(e.code);
      if (!isModifierCode(e.code)) {
        lastKeyRef.current = e.code;
      } else if (lastKeyRef.current === null) {
        lastKeyRef.current = e.code;
      }
    };

    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      if (e.code === "Escape") {
        void invoke("set_rebinding", { active: false });
        setCapture({ kind: "idle" });
        return;
      }
      let primary = lastKeyRef.current;
      if (!primary) {
        primary = e.code;
      }
      if (
        isModifierCode(primary) &&
        !Array.from(pressedRef.current).some((c) => !isModifierCode(c))
      ) {
        primary = e.code;
      }

      void invoke("set_rebinding", { active: false });
      const binding = buildBinding(primary, pressedRef.current);
      setCapture({ kind: "captured", binding });
    };

    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
    };
  }, [capture.kind]);

  const handleSave = async () => {
    if (capture.kind !== "captured") return;
    setSaving(true);
    setSaveError(null);
    try {
      await saver(capture.binding);
      setCurrent(capture.binding);
      onChange?.(capture.binding);
      setCapture({ kind: "idle" });
      // The backend re-registers on save, so ask whether the new combination
      // was actually accepted by the OS.
      await refreshProblem();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setSaveError(`Unsupported key — try a different one. (${msg})`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div>
      <div className="rounded-md border border-line bg-canvas p-4">
        {capture.kind === "idle" ? (
          <div className="flex items-center justify-between gap-4">
            <div className="text-sm">
              Current:{" "}
              <span className="font-semibold">
                {current
                  ? formatBinding(current)
                  : loadError
                    ? "—"
                    : "Loading…"}
              </span>
            </div>
            <button
              type="button"
              onClick={() => {
                setSaveError(null);
                void invoke("set_rebinding", { active: true });
                setCapture({ kind: "capturing" });
              }}
              className="rounded border border-line px-3 py-1 text-xs hover:bg-elevated"
            >
              Record new shortcut…
            </button>
          </div>
        ) : capture.kind === "capturing" ? (
          <div className="text-sm">
            <div className="font-semibold">Listening…</div>
            <p className="mt-1 text-muted">
              Press a key combination, then release. Press Esc to cancel.
            </p>
          </div>
        ) : (
          <div className="flex flex-col gap-3">
            <div className="text-sm">
              Captured:{" "}
              <span className="font-semibold">
                {formatBinding(capture.binding)}
              </span>
            </div>
            <div className="flex gap-2">
              <button
                type="button"
                disabled={saving}
                onClick={() => {
                  void handleSave();
                }}
                className="rounded-md bg-accent px-3 py-1 text-xs font-semibold text-canvas hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-50"
              >
                {saving ? "Saving…" : "Save"}
              </button>
              <button
                type="button"
                onClick={() => {
                  void invoke("set_rebinding", { active: false });
                  setCapture({ kind: "idle" });
                  setSaveError(null);
                }}
                className="rounded border border-line px-3 py-1 text-xs hover:bg-elevated"
              >
                Cancel
              </button>
            </div>
          </div>
        )}
      </div>

      {problem && capture.kind === "idle" ? (
        <p className="mt-2 text-xs text-warning">{problem}</p>
      ) : null}
      {saveError ? (
        <p className="mt-2 text-xs text-warning">{saveError}</p>
      ) : null}
      {loadError && capture.kind === "idle" ? (
        <p className="mt-2 text-xs text-warning">
          Couldn’t load current shortcut: {loadError}
        </p>
      ) : null}
    </div>
  );
}
