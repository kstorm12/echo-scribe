import { useEffect, useState, type SyntheticEvent } from "react";
import {
  ArrowLeft,
  Mic,
  NotebookPen,
  Phone,
  Zap,
  WandSparkles,
  Settings as SettingsIcon,
  Sparkles,
  Cloud,
  FolderKanban,
  ShieldCheck,
  Trash2,
  Wrench,
  X,
  Info,
  Check,
  type LucideIcon,
} from "lucide-react";
import HotkeyRebinder from "../components/HotkeyRebinder";
import SpeechModelPicker from "../components/SpeechModelPicker";
import LlmModelPicker from "../components/LlmModelPicker";
import ProjectManager from "../components/ProjectManager";
import GuideTemplateManager from "../components/GuideTemplateManager";
import PermissionsSection from "../components/PermissionsSection";
import StartAtLoginToggle from "../components/StartAtLoginToggle";
import TranscriptionSettings from "../components/TranscriptionSettings";
import Dialog from "../components/a11y/Dialog";
import { useCapabilities } from "../lib/capabilitiesContext";
import { uiGates } from "../lib/capabilities";
import {
  diagnosticsLogDir,
  diagnosticsOpenLogFolder,
  diagnosticsRecentLog,
  getAsrUnloadSecs,
  getAudioFeedbackEnabled,
  getAutoFileEnabled,
  getAutoFileThreshold,
  getExportConfidenceThreshold,
  setExportConfidenceThreshold,
  getDailyRecapSettings,
  getInputDeviceSort,
  getLlmUnloadSecs,
  getLogCaptureBinding,
  getMuteWhileRecording,
  getPreferredInputDevice,
  getRecentInputDevices,
  listInputDevices,
  resetOnboardingAndQuit,
  uninstallApplication,
  getAppVersion,
  setAsrUnloadSecs,
  setAudioFeedbackEnabled,
  setAutoFileEnabled,
  setAutoFileThreshold,
  setDailyRecapSettings,
  setInputDeviceSort,
  setLlmUnloadSecs,
  setMuteWhileRecording,
  setPreferredInputDevice,
  testLlmInference,
  updateLogCaptureBinding,
  getAppLauncherEnabled,
  setAppLauncherEnabled,
  getActionCounter,
  resetActionCounter,
  getCommonActions,
  getActionBinding,
  updateActionBinding,
  getEditSelectionBinding,
  updateEditSelectionBinding,
  getTriggerWordRoutingEnabled,
  setTriggerWordRoutingEnabled,
  getActionTriggerWord,
  setActionTriggerWord,
  getFormatTemplates,
  setFormatTemplates,
  getProjectAutoTaggingEnabled,
  setProjectAutoTaggingEnabled,
  projectTaggerStatus,
  projectTaggerBackfill,
  runProjectTaggerDeterministicOnce,
  runProjectTaggerLlmOnce,
  type FormatTemplate,
  type ProjectTaggerStatus,
  driveStatus,
  driveConnect,
  driveDisconnect,
  getDriveClientId,
  setDriveClientCredentials,
  getDrivePrefs,
  setDrivePrefs,
  type DriveStatus,
  type CommonActionTemplate,
  type DailyRecapSettings as DailyRecapSettingsT,
  type InputDevice,
  type InputDeviceSort,
} from "../lib/api";
import { useToasts } from "../components/ToastProvider";
import { useUpdateCheck } from "../lib/useUpdateCheck";
import { ask } from "@tauri-apps/plugin-dialog";

type PageId =
  | "dictation"
  | "logcapture"
  | "meetings"
  | "actions"
  | "templates"
  | "language-model"
  | "general"
  | "drive"
  | "projects"
  | "permissions"
  | "diagnostics"
  | "uninstall";

type NavItem = { id: PageId; label: string; icon: LucideIcon };
type NavGroup = { label: string; items: NavItem[] };

const NAV_GROUPS: NavGroup[] = [
  {
    label: "Capture",
    items: [
      { id: "dictation", label: "Dictation", icon: Mic },
      { id: "logcapture", label: "Log Capture", icon: NotebookPen },
      { id: "meetings", label: "Meetings", icon: Phone },
    ],
  },
  {
    label: "Automation",
    items: [
      { id: "actions", label: "Actions", icon: Zap },
      { id: "templates", label: "Templates", icon: WandSparkles },
    ],
  },
  {
    label: "System",
    items: [
      { id: "language-model", label: "Language Model", icon: Sparkles },
      { id: "general", label: "General", icon: SettingsIcon },
      { id: "drive", label: "Google Drive", icon: Cloud },
      { id: "projects", label: "Projects", icon: FolderKanban },
      { id: "permissions", label: "Permissions", icon: ShieldCheck },
      { id: "diagnostics", label: "Diagnostics", icon: Wrench },
      { id: "uninstall", label: "Uninstall", icon: Trash2 },
    ],
  },
];

const PAGE_DESC: Record<PageId, string> = {
  dictation:
    "Speech-to-text at your cursor — model, microphone, hotkey, and cleanup.",
  logcapture:
    "Capture a thought, idea, or task by voice, then file and export it.",
  meetings: "Detect calls, summarize transcripts, and set recording limits.",
  actions:
    "Run app launches, links, emails, and counters straight from your voice.",
  templates:
    "Rewrite dictation into emails, lists, or any style before it's pasted.",
  "language-model":
    "The local Gemma model that powers classification, summaries, and actions.",
  general: "Sounds, recording behavior, daily recap, and startup.",
  drive: "Upload screen recordings to Google Drive and share them with a link.",
  projects: "Rename, archive, and organize your projects.",
  permissions: "Microphone, accessibility, and screen-recording access.",
  diagnostics: "Inspect logs and reset the app if something breaks.",
  uninstall: "Remove the app while choosing whether to keep your local data.",
};

const PAGES: Record<PageId, () => React.ReactElement> = {
  dictation: DictationPage,
  logcapture: LogCapturePage,
  meetings: MeetingsPage,
  actions: ActionsPage,
  templates: TemplatesPage,
  "language-model": LanguageModelPage,
  general: GeneralPage,
  drive: DrivePage,
  projects: ProjectsPage,
  permissions: PermissionsPage,
  diagnostics: DiagnosticsPage,
  uninstall: UninstallPage,
};

type Props = {
  onBack: () => void;
};

export default function Settings({ onBack }: Props) {
  const [page, setPage] = useState<PageId>("dictation");
  const gates = uiGates(useCapabilities());

  // Drop nav items gated behind macOS-only capabilities, then drop any group
  // that ends up empty. Everything not explicitly gated stays visible.
  const visibleGroups: NavGroup[] = NAV_GROUPS.map((group) => ({
    ...group,
    items: group.items.filter((item) => {
      if (item.id === "meetings") return gates.showMeetingsNav;
      if (item.id === "drive") return gates.showDrive;
      if (item.id === "permissions") return gates.showNativePermissions;
      if (item.id === "uninstall") return gates.showSelfUpdate;
      return true;
    }),
  })).filter((group) => group.items.length > 0);

  const activeItem = visibleGroups.flatMap((g) => g.items).find(
    (i) => i.id === page,
  );

  // Fallback: if the persisted/default page id isn't visible on this platform
  // (e.g. a Windows build inherited "meetings" from a synced macOS profile),
  // redirect to the first visible page instead of rendering nothing.
  useEffect(() => {
    if (!activeItem) {
      const firstVisible = visibleGroups[0]?.items[0]?.id;
      if (firstVisible) setPage(firstVisible);
    }
  }, [activeItem, visibleGroups]);

  const ActivePage = activeItem ? PAGES[page] : null;

  return (
    <div className="min-h-full bg-canvas px-4 py-8 text-fg">
      <div className="mx-auto flex w-full max-w-[900px] flex-col">
        <button
          type="button"
          onClick={onBack}
          className="mb-4 inline-flex cursor-pointer items-center gap-1.5 self-start rounded-md border border-line px-2.5 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-fg"
        >
          <ArrowLeft size={12} strokeWidth={2} />
          Back
        </button>

        <div className="flex items-start gap-5">
          {/* Left sidebar */}
          <nav className="sticky top-4 w-[200px] shrink-0 rounded-xl border border-line bg-surface p-3 shadow-lg shadow-black/30">
            <div className="flex flex-col gap-4">
              {visibleGroups.map((group) => (
                <div key={group.label} className="flex flex-col gap-0.5">
                  <div className="px-2 pb-1 text-[10px] font-semibold uppercase tracking-wider text-faint">
                    {group.label}
                  </div>
                  {group.items.map(({ id, label, icon: Icon }) => {
                    const active = page === id;
                    return (
                      <button
                        key={id}
                        type="button"
                        aria-current={active ? "page" : undefined}
                        onClick={() => setPage(id)}
                        className={[
                          "flex cursor-pointer items-center gap-2.5 rounded-md px-2.5 py-1.5 text-left text-[13px] font-medium transition-colors",
                          active
                            ? "bg-accent-soft text-accent"
                            : "text-muted hover:bg-elevated hover:text-fg",
                        ].join(" ")}
                      >
                        <Icon
                          size={14}
                          strokeWidth={2}
                          className={active ? "text-accent" : "text-faint"}
                        />
                        <span className="truncate">{label}</span>
                      </button>
                    );
                  })}
                </div>
              ))}
            </div>
          </nav>

          {/* Content panel */}
          <div className="min-w-0 flex-1 rounded-xl border border-line bg-surface p-6 shadow-lg shadow-black/30">
            {activeItem && ActivePage ? (
              <>
                <header className="mb-6 border-b border-line pb-4">
                  <h1 className="text-[15px] font-semibold tracking-tight text-fg">
                    {activeItem.label}
                  </h1>
                  <p className="mt-1 text-xs leading-relaxed text-muted">
                    {PAGE_DESC[page]}
                  </p>
                </header>
                <ActivePage />
              </>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}

function DictationPage() {
  return (
    <div className="flex flex-col gap-8">
      <SpeechModelPicker />

      <Section
        title="Microphone"
        subtitle="Pick which input device Echo Scribe records from. Default uses whatever macOS has selected."
      >
        <MicrophonePicker />
      </Section>

      <Section
        title="Voice-at-cursor hotkey"
        subtitle="Hold this key combination anywhere in macOS to dictate at the cursor."
      >
        <HotkeyRebinder />
      </Section>

      <Section
        title="Transcription"
        subtitle="Clean up speech-to-text output before it's pasted or saved."
      >
        <TranscriptionSettings />
      </Section>

      <Section
        title="Keep speech model in memory"
        subtitle="How long the speech-to-text model stays loaded after its last use. Longer = faster next transcription, but uses more RAM."
      >
        <AsrUnloadTimeoutSelect />
      </Section>
    </div>
  );
}

function LogCapturePage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Log capture hotkey"
        subtitle="Hold this key combination to capture a thought, idea, or task — classified locally and saved to your log."
      >
        <HotkeyRebinder
          action="log-capture"
          load={getLogCaptureBinding}
          save={updateLogCaptureBinding}
        />
      </Section>

      <AutoFileSettings />
      <ProjectAutoTaggingSettings />
      <ExportSettings />
    </div>
  );
}

function LanguageModelPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Language model"
        subtitle="Local Gemma model used for log-capture classification, meeting summaries, voice actions, and formatting."
      >
        <LlmModelPicker />
        <div className="mt-4">
          <TestInference />
        </div>
      </Section>

      <Section
        title="Keep model in memory"
        subtitle="How long the AI model stays loaded after its last use. Longer = faster next use, but more RAM."
      >
        <LlmUnloadTimeoutSelect />
      </Section>
    </div>
  );
}

function ActionsPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Voice commands & app launcher"
        subtitle="Detect command actions inside your voice dictations to launch applications, open links, compose emails, and manage counters."
      >
        <AppLauncherSettingsSection />
      </Section>
    </div>
  );
}

function TemplatesPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Voice format templates"
        subtitle='Say "echo, format as email …" (or use the Action hotkey) to rewrite your dictation with a custom system prompt before it gets pasted.'
      >
        <FormatTemplatesSection />
      </Section>
    </div>
  );
}

function ExportSettings() {
  const [threshold, setThresholdLocal] = useState(0.75);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const v = await getExportConfidenceThreshold().catch(() => 0.75);
      if (!cancelled) setThresholdLocal(v);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Persist on mouse, touch AND keyboard interaction — arrow-key changes on
  // the range input never fire mouseup/touchend.
  const commitThreshold = async (e: SyntheticEvent<HTMLInputElement>) => {
    const next = Number((e.target as HTMLInputElement).value);
    try {
      await setExportConfidenceThreshold(next);
    } catch {
      getExportConfidenceThreshold()
        .then(setThresholdLocal)
        .catch(() => {});
    }
  };

  return (
    <Section
      title="Markdown export"
      subtitle="When a project has an export folder configured, items routed to it are auto-saved as markdown files for use with external AI tools. Configure the folder per project in Settings → Projects."
    >
      <div className="flex flex-col gap-3">
        <p className="text-xs text-muted">
          Items export when the classifier's confidence reaches{" "}
          <span className="font-mono">{Math.round(threshold * 100)}%</span> or
          higher. Meetings always export when a project has a folder set.
        </p>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">
            Confidence threshold: {Math.round(threshold * 100)}%
          </span>
          <input
            type="range"
            min={0.5}
            max={0.95}
            step={0.05}
            value={threshold}
            onChange={(e) => setThresholdLocal(Number(e.target.value))}
            onMouseUp={commitThreshold}
            onTouchEnd={commitThreshold}
            onKeyUp={commitThreshold}
            onBlur={commitThreshold}
            className="w-full"
          />
        </label>
      </div>
    </Section>
  );
}

function ProjectAutoTaggingSettings() {
  const toasts = useToasts();
  const [enabled, setEnabled] = useState(true);
  const [status, setStatus] = useState<ProjectTaggerStatus | null>(null);
  const [busy, setBusy] = useState<"backfill" | "router" | "llm" | null>(null);

  const refresh = async () => {
    const [enabledValue, statusValue] = await Promise.all([
      getProjectAutoTaggingEnabled().catch(() => true),
      projectTaggerStatus().catch(() => null),
    ]);
    setEnabled(enabledValue);
    setStatus(statusValue);
  };

  useEffect(() => {
    void refresh();
  }, []);

  const run = async (kind: "backfill" | "router" | "llm") => {
    setBusy(kind);
    try {
      if (kind === "backfill") {
        const n = await projectTaggerBackfill({ source: "voice_at_cursor", limit: 500 });
        toasts.push({ tone: "success", message: `Queued ${n} transcription${n === 1 ? "" : "s"} for tagging.` });
      } else if (kind === "router") {
        const s = await runProjectTaggerDeterministicOnce();
        toasts.push({ tone: "success", message: `Router assigned ${s.assigned} of ${s.scanned} queued item${s.scanned === 1 ? "" : "s"}.` });
      } else {
        const s = await runProjectTaggerLlmOnce();
        toasts.push({ tone: "success", message: `Local AI assigned ${s.assigned} of ${s.scanned} queued item${s.scanned === 1 ? "" : "s"}.` });
      }
      await refresh();
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Project tagging failed: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(null);
    }
  };

  return (
    <Section
      title="Project auto-tagging"
      subtitle="Organize direct dictations later in batches so the local AI model does not load after every paste."
    >
      <div className="flex flex-col gap-3">
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={enabled}
            onChange={async (e) => {
              const next = e.target.checked;
              setEnabled(next);
              try {
                await setProjectAutoTaggingEnabled(next);
                await refresh();
              } catch {
                setEnabled(!next);
              }
            }}
          />
          Enable deferred project tagging
        </label>
        {status && (
          <div className="grid grid-cols-2 gap-2 text-xs text-muted sm:grid-cols-4">
            <span>Pending: <span className="font-mono text-fg">{status.pending}</span></span>
            <span>Deferred: <span className="font-mono text-fg">{status.deferred}</span></span>
            <span>Done: <span className="font-mono text-fg">{status.done}</span></span>
            <span>Failed: <span className="font-mono text-fg">{status.failed}</span></span>
          </div>
        )}
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            disabled={busy !== null}
            onClick={() => void run("backfill")}
            className="rounded-md border border-line px-3 py-1 text-xs hover:bg-elevated disabled:opacity-50"
          >
            {busy === "backfill" ? "Queueing..." : "Queue unassigned dictations"}
          </button>
          <button
            type="button"
            disabled={busy !== null}
            onClick={() => void run("router")}
            className="rounded-md border border-line px-3 py-1 text-xs hover:bg-elevated disabled:opacity-50"
          >
            {busy === "router" ? "Running..." : "Run keyword router"}
          </button>
          <button
            type="button"
            disabled={busy !== null || status?.llm_ready === false}
            onClick={() => void run("llm")}
            className="rounded-md border border-line px-3 py-1 text-xs hover:bg-elevated disabled:opacity-50"
          >
            {busy === "llm" ? "Running..." : "Run local AI batch"}
          </button>
        </div>
      </div>
    </Section>
  );
}

function AutoFileSettings() {
  const [autoFileEnabled, setAutoFileEnabledLocal] = useState(true);
  const [autoFileThreshold, setAutoFileThresholdLocal] = useState(0.75);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const [enabled, threshold] = await Promise.all([
        getAutoFileEnabled().catch(() => true),
        getAutoFileThreshold().catch(() => 0.75),
      ]);
      if (cancelled) return;
      setAutoFileEnabledLocal(enabled);
      setAutoFileThresholdLocal(threshold);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Persist on mouse, touch AND keyboard interaction — arrow-key changes on
  // the range input never fire mouseup/touchend.
  const commitAutoFileThreshold = async (
    e: SyntheticEvent<HTMLInputElement>,
  ) => {
    const next = Number((e.target as HTMLInputElement).value);
    try {
      await setAutoFileThreshold(next);
    } catch {
      // Reload from backend on error.
      getAutoFileThreshold().then(setAutoFileThresholdLocal).catch(() => {});
    }
  };

  return (
    <Section
      title="Auto-file confident captures"
      subtitle="File high-confidence captures silently. New-project proposals always show the review overlay."
    >
      <div className="flex flex-col gap-3">
        <p className="text-xs text-muted">
          When the local AI is at least <span className="font-mono">
          {Math.round(autoFileThreshold * 100)}%</span> sure about the project and
          kind, file the capture silently with a toast (or system notification when
          the window is closed).
        </p>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={autoFileEnabled}
            onChange={async (e) => {
              const next = e.target.checked;
              setAutoFileEnabledLocal(next);
              try {
                await setAutoFileEnabled(next);
              } catch {
                setAutoFileEnabledLocal(!next);
              }
            }}
          />
          Enable auto-file
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">
            Threshold: {Math.round(autoFileThreshold * 100)}%
          </span>
          <input
            type="range"
            min={0.5}
            max={0.95}
            step={0.05}
            disabled={!autoFileEnabled}
            value={autoFileThreshold}
            onChange={(e) => setAutoFileThresholdLocal(Number(e.target.value))}
            onMouseUp={commitAutoFileThreshold}
            onTouchEnd={commitAutoFileThreshold}
            onKeyUp={commitAutoFileThreshold}
            onBlur={commitAutoFileThreshold}
            className="w-full"
          />
        </label>
      </div>
    </Section>
  );
}

function MicrophonePicker() {
  const [devices, setDevices] = useState<InputDevice[]>([]);
  const [preferred, setPreferred] = useState<string | null>(null);
  const [recent, setRecent] = useState<string[]>([]);
  const [sort, setSort] = useState<InputDeviceSort>("last_used");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const reload = async () => {
    try {
      const [d, p, r, s] = await Promise.all([
        listInputDevices(),
        getPreferredInputDevice(),
        getRecentInputDevices(),
        getInputDeviceSort(),
      ]);
      setDevices(d);
      setPreferred(p);
      setRecent(r);
      setSort(s);
      setError(null);
    } catch (e) {
      setError(`Couldn't load microphones: ${String(e)}`);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    reload();
  }, []);

  // Order: system default first, then preferred (if not the same as default
  // and present), then everything else by chosen sort. The system-default
  // entry is always shown — even if it changed since last selection — so the
  // user can revert to "follow macOS" with one click.
  const ordered = orderDevices(devices, recent, sort);
  const systemDefault = devices.find((d) => d.is_system_default) ?? null;
  const preferredMissing =
    preferred !== null && !devices.some((d) => d.name === preferred);

  if (loading) {
    return <p className="text-xs text-muted">Loading microphones…</p>;
  }
  if (error) {
    return (
      <div className="flex flex-col gap-2">
        <p className="text-xs text-warning">{error}</p>
        <button
          type="button"
          onClick={reload}
          className="self-start rounded border border-line px-2 py-1 text-xs"
        >
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      {preferredMissing && (
        <p className="rounded border border-warning/40 bg-warning/10 px-2 py-1.5 text-xs text-warning">
          Saved mic <span className="font-mono">{preferred}</span> isn't
          connected. Pick another below or revert to the system default.
        </p>
      )}

      <label className="flex flex-col gap-1 text-sm">
        <span className="text-muted">Input device</span>
        <select
          className="rounded border border-line bg-canvas px-2 py-1 text-sm"
          value={preferred ?? ""}
          onChange={async (e) => {
            const next = e.target.value === "" ? null : e.target.value;
            const prev = preferred;
            setPreferred(next);
            try {
              await setPreferredInputDevice(next);
              if (next !== null) {
                setRecent((curr) => {
                  const without = curr.filter((n) => n !== next);
                  return [next, ...without].slice(0, 10);
                });
              }
            } catch {
              setPreferred(prev);
            }
          }}
        >
          <option value="">
            System default
            {systemDefault ? ` — ${systemDefault.name}` : ""}
          </option>
          {ordered.map((d) => (
            <option key={d.name} value={d.name}>
              {d.name}
              {d.sample_rate ? ` · ${d.sample_rate / 1000}kHz` : ""}
              {d.channels ? ` · ${d.channels}ch` : ""}
            </option>
          ))}
        </select>
      </label>

      <label className="flex items-center gap-2 text-sm">
        <span className="text-muted">Sort by</span>
        <select
          className="rounded border border-line bg-canvas px-2 py-1 text-sm"
          value={sort}
          onChange={async (e) => {
            const next = e.target.value as InputDeviceSort;
            const prev = sort;
            setSort(next);
            try {
              await setInputDeviceSort(next);
            } catch {
              setSort(prev);
            }
          }}
        >
          <option value="last_used">Last used</option>
          <option value="alphabetical">Alphabetical</option>
        </select>
      </label>

      <p className="text-[11px] text-muted">
        When a saved mic is unplugged, Echo Scribe will refuse to record and
        notify you — it won't silently fall back to another device.
      </p>
    </div>
  );
}

function orderDevices(
  devices: InputDevice[],
  recent: string[],
  sort: InputDeviceSort,
): InputDevice[] {
  if (sort === "alphabetical") {
    return [...devices].sort((a, b) => a.name.localeCompare(b.name));
  }
  // last_used: recent-MRU first (in order), then unseen devices alphabetical.
  const recentSet = new Set(recent);
  const recentDevices: InputDevice[] = [];
  for (const name of recent) {
    const d = devices.find((x) => x.name === name);
    if (d) recentDevices.push(d);
  }
  const rest = devices
    .filter((d) => !recentSet.has(d.name))
    .sort((a, b) => a.name.localeCompare(b.name));
  return [...recentDevices, ...rest];
}

const SUMMARY_TEMPLATES = [
  {
    id: "standard",
    name: "Standard Note-Taker",
    description: "General summary and actionable next steps",
    prompt: `You are an expert meeting note-taker. You receive a transcript of a {duration_minutes}-minute conversation captured from {app}. The transcript labels each segment as 'You:' (the user) or 'Them:' (the other side).

Generate a comprehensive meeting summary that captures the essence of the discussion. Focus on:
1. The main objectives and topics of the meeting.
2. The key discussion points, arguments, and context.
3. Crucial decisions made or consensus reached.
4. Specific action items with clear ownership.`,
  },
  {
    id: "action-item",
    name: "Action-Item Focused",
    description: "Prioritizes tasks, owners, and deadlines",
    prompt: `You are a highly efficient project manager. You receive a transcript of a {duration_minutes}-minute conversation captured from {app}. The transcript labels each segment as 'You:' (the user) or 'Them:' (the other side).

Focus deeply on extracting all action items, ownership, deadlines, and deliverables. Ensure:
1. Every task is explicitly captured with its owner (either 'you', 'them', or 'unspecified').
2. Mention any deadlines, timelines, or dependencies mentioned.
3. Keep the general summary extremely concise, highlighting only what led to the tasks.`,
  },
  {
    id: "executive",
    name: "Executive Summary",
    description: "Strategic roadmaps, outcomes, and business impact",
    prompt: `You are a high-level strategic advisor. You receive a transcript of a {duration_minutes}-minute conversation captured from {app}. The transcript labels each segment as 'You:' (the user) or 'Them:' (the other side).

Create a premium, high-level executive summary for leadership. Focus on:
1. Key takeaways, strategic decisions, and alignment.
2. Core business/product outcomes and why they matter.
3. High-level roadmaps, major milestones, and strategic action items.
4. Keep technical minutiae to an absolute minimum, focusing on high-level impact.`,
  },
  {
    id: "technical",
    name: "Technical Sync",
    description: "Deep dev syncs, APIs, and blockers",
    prompt: `You are a lead systems architect and technical writer. You receive a transcript of a {duration_minutes}-minute conversation captured from {app}. The transcript labels each segment as 'You:' (the user) or 'Them:' (the other side).

Generate a highly detailed technical sync note. Focus on:
1. Architectural decisions, system designs, and code changes discussed.
2. Specific APIs, libraries, endpoints, database schemas, or protocols mentioned.
3. Blockers, bugs, performance issues, and debugging steps.
4. Precise technical tasks, dev ownership, and next steps for the engineering team.`,
  },
];

interface SummaryPromptModalProps {
  isOpen: boolean;
  onClose: () => void;
  currentPrompt: string;
  onSave: (prompt: string) => Promise<void>;
}

function SummaryPromptModal({
  isOpen,
  onClose,
  currentPrompt,
  onSave,
}: SummaryPromptModalProps) {
  const [prompt, setPrompt] = useState(currentPrompt);
  const [selectedTemplate, setSelectedTemplate] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (isOpen) {
      setPrompt(currentPrompt);
      const matched = SUMMARY_TEMPLATES.find(
        (t) => t.prompt.trim() === currentPrompt.trim()
      );
      setSelectedTemplate(matched ? matched.id : null);
    }
  }, [isOpen, currentPrompt]);

  if (!isOpen) return null;

  const handleSave = async () => {
    setBusy(true);
    try {
      await onSave(prompt);
      onClose();
    } catch {
      // Parent's handler processes reporting errors via Toast
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      onClose={onClose}
      labelledBy="summary-prompt-modal-title"
      dismissible={!busy}
      className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/60 backdrop-blur-sm animate-backdrop"
      panelClassName="w-full max-w-[680px] rounded-xl border border-line bg-surface p-6 text-fg shadow-2xl flex flex-col max-h-[90vh] animate-card"
    >
      <style>{`
        @keyframes modal-backdrop-fade {
          from { opacity: 0; background-color: rgba(0, 0, 0, 0); backdrop-filter: blur(0px); }
          to { opacity: 1; background-color: rgba(0, 0, 0, 0.6); backdrop-filter: blur(4px); }
        }
        @keyframes modal-card-appear {
          from { opacity: 0; transform: scale(0.96) translateY(12px); }
          to { opacity: 1; transform: scale(1) translateY(0); }
        }
        .animate-backdrop {
          animation: modal-backdrop-fade 0.2s cubic-bezier(0.16, 1, 0.3, 1) forwards;
        }
        .animate-card {
          animation: modal-card-appear 0.25s cubic-bezier(0.34, 1.56, 0.64, 1) forwards;
        }
      `}</style>
        {/* Header */}
        <div className="flex items-start justify-between border-b border-line pb-4">
          <div>
            <h2
              id="summary-prompt-modal-title"
              className="text-base font-semibold tracking-tight flex items-center gap-1.5"
            >
              <Sparkles size={14} className="text-accent animate-pulse" />
              Meeting Summary Guidelines
            </h2>
            <p className="mt-1 text-xs text-muted">
              Customize the instructions the local AI uses to summarize and extract action items from transcripts.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded-md p-1.5 text-muted hover:bg-elevated hover:text-fg transition-colors"
          >
            <X size={14} />
          </button>
        </div>

        {/* Content */}
        <div className="mt-4 flex-1 overflow-y-auto pr-1 flex flex-col gap-4">
          {/* Templates Section */}
          <div className="flex flex-col gap-2">
            <span className="text-xs font-semibold text-fg">Load from pre-defined templates</span>
            <div className="grid grid-cols-2 gap-2">
              {SUMMARY_TEMPLATES.map((t) => (
                <button
                  key={t.id}
                  type="button"
                  onClick={() => {
                    setPrompt(t.prompt);
                    setSelectedTemplate(t.id);
                  }}
                  className={[
                    "flex flex-col items-start gap-1 p-3 rounded-lg border text-left transition-all",
                    selectedTemplate === t.id
                      ? "border-accent bg-accent-soft/30 text-fg ring-1 ring-accent"
                      : "border-line bg-canvas hover:bg-elevated text-muted hover:text-fg",
                  ].join(" ")}
                >
                  <span className="text-xs font-bold">{t.name}</span>
                  <span className="text-[10px] text-muted leading-tight">{t.description}</span>
                </button>
              ))}
            </div>
          </div>

          {/* Textarea Section */}
          <div className="flex flex-col gap-1.5">
            <div className="flex items-center justify-between">
              <span className="text-xs font-semibold text-fg">Custom guidelines</span>
              {selectedTemplate && (
                <span className="text-[10px] text-accent font-medium">
                  Template loaded
                </span>
              )}
            </div>
            <textarea
              rows={11}
              value={prompt}
              onChange={(e) => {
                setPrompt(e.target.value);
                setSelectedTemplate(null);
              }}
              placeholder="Describe how the AI should summarize the meeting transcript..."
              className="w-full rounded-lg border border-line bg-canvas p-3 text-xs leading-relaxed font-mono focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent transition-all resize-none"
            />
          </div>

          {/* Guidelines info card */}
          <div className="rounded-lg border border-line bg-canvas p-3 flex flex-col gap-1.5 text-xs text-muted">
            <div className="flex items-center gap-1.5 font-semibold text-fg">
              <Info size={12} className="text-accent" />
              Dynamic Placeholders
            </div>
            <p className="text-[11px] leading-relaxed">
              You can insert <code className="font-mono text-accent bg-accent-soft px-1 rounded">{"{duration_minutes}"}</code> and <code className="font-mono text-accent bg-accent-soft px-1 rounded">{"{app}"}</code> in your guidelines. They will be replaced at runtime with the meeting's duration and app name (e.g., Zoom, Teams).
            </p>
          </div>

          {/* Safety Notice */}
          <div className="rounded-lg border border-success/20 bg-success/5 p-3 flex flex-col gap-1.5 text-xs text-success">
            <div className="flex items-center gap-1.5 font-semibold">
              <Check size={12} />
              Format Safety Handled Automatically
            </div>
            <p className="text-[11px] leading-relaxed text-muted">
              Do not worry about instructing the AI to output JSON or specific lists format. The backend automatically appends formatting schemas to ensure the meeting summary is properly parsed and displayed in the application layout.
            </p>
          </div>
        </div>

        {/* Footer Actions */}
        <div className="mt-6 flex items-center justify-end gap-2.5 border-t border-line pt-4 shrink-0">
          <button
            type="button"
            disabled={busy}
            onClick={onClose}
            className="rounded-md border border-line px-3 py-1.5 text-xs font-semibold text-muted hover:bg-elevated hover:text-fg transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={busy || !prompt.trim()}
            onClick={handleSave}
            className="rounded-md bg-accent px-4 py-1.5 text-xs font-semibold text-canvas hover:bg-accent-hover transition-colors flex items-center gap-1 disabled:opacity-50"
          >
            {busy ? "Saving..." : "Save Guidelines"}
          </button>
        </div>
    </Dialog>
  );
}

function MeetingsPage() {
  const [settings, setSettings] = useState<{
    auto_detect: boolean;
    app_prefs: Record<string, "always" | "ask" | "never">;
    soft_warn_min: number;
    hard_cap_min: number;
    summary_prompt: string;
  } | null>(null);
  const [isModalOpen, setIsModalOpen] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    void import("../lib/api").then(({ getMeetingSettings }) =>
      getMeetingSettings().then(setSettings),
    );
  }, []);

  const handleSavePrompt = async (nextPrompt: string) => {
    try {
      const { setMeetingSummaryPrompt } = await import("../lib/api");
      await setMeetingSummaryPrompt(nextPrompt);
      setSettings((prev) => prev ? { ...prev, summary_prompt: nextPrompt } : null);
      toasts.push({
        tone: "success",
        message: "Meeting summary guidelines updated successfully.",
      });
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Failed to save guidelines: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  };

  if (!settings) {
    return <div className="text-sm text-muted">Loading…</div>;
  }

  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Auto-detect meetings"
        subtitle="Echo Scribe watches for Zoom, Teams, FaceTime, Discord, Slack, and browser tabs and offers to record."
      >
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={settings.auto_detect}
            onChange={async (e) => {
              const on = e.target.checked;
              const { setMeetingAutoDetect } = await import("../lib/api");
              await setMeetingAutoDetect(on);
              setSettings({ ...settings, auto_detect: on });
            }}
          />
          <span>Detect supported meeting apps automatically</span>
        </label>
      </Section>

      {Object.keys(settings.app_prefs).length > 0 && (
        <Section
          title="Per-app preferences"
          subtitle="Override the per-app prompt: always record, always ask, or never record."
        >
          <table className="w-full text-sm">
            <tbody>
              {Object.entries(settings.app_prefs).map(([bundle, pref]) => (
                <tr key={bundle} className="border-t border-line">
                  <td className="py-2">{bundle}</td>
                  <td className="py-2 text-right">
                    <div className="flex justify-end gap-2">
                      <select
                        className="rounded-md bg-canvas px-2 py-1 text-xs"
                        value={pref}
                        onChange={async (e) => {
                          const next = e.target.value as
                            | "always"
                            | "ask"
                            | "never";
                          const { setMeetingAppPref } = await import(
                            "../lib/api"
                          );
                          await setMeetingAppPref(bundle, next);
                          setSettings({
                            ...settings,
                            app_prefs: {
                              ...settings.app_prefs,
                              [bundle]: next,
                            },
                          });
                        }}
                      >
                        <option value="always">Always</option>
                        <option value="ask">Ask</option>
                        <option value="never">Never</option>
                      </select>
                      <button
                        className="rounded-md bg-surface-2 px-2 py-1 text-xs text-muted hover:text-fg"
                        onClick={async () => {
                          const { clearMeetingAppPref } = await import(
                            "../lib/api"
                          );
                          await clearMeetingAppPref(bundle);
                          const next = { ...settings.app_prefs };
                          delete next[bundle];
                          setSettings({ ...settings, app_prefs: next });
                        }}
                        title="Remove this app's preference (revert to Ask on next detection)"
                      >
                        Clear
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      )}

      <Section
        title="Meeting summary guidelines"
        subtitle="Configure custom prompt guidelines used by the local AI to summarize your meetings."
      >
        <div className="flex flex-col gap-3 rounded-lg border border-line bg-canvas p-4">
          <div className="flex items-start justify-between gap-4">
            <div className="flex flex-col gap-1 min-w-0 flex-1">
              <span className="text-xs font-semibold text-muted">Active guidelines</span>
              <p className="mt-1.5 text-xs text-muted/80 line-clamp-3 italic whitespace-pre-wrap leading-relaxed font-mono">
                {settings.summary_prompt || "No custom guidelines set."}
              </p>
            </div>
            <button
              type="button"
              onClick={() => setIsModalOpen(true)}
              className="shrink-0 inline-flex items-center gap-1.5 rounded-md border border-line bg-surface px-3 py-1.5 text-xs font-semibold text-fg transition-all hover:bg-elevated hover:text-accent active:scale-[0.98]"
            >
              <Sparkles size={12} className="text-accent" />
              Edit guidelines...
            </button>
          </div>
        </div>
      </Section>

      <Section
        title="Time limits"
        subtitle="Echo Scribe shows a soft warning at the soft-warn time and auto-stops at the hard cap."
      >
        <div className="flex flex-col gap-3 text-sm">
          <label className="flex items-center justify-between gap-3">
            <span>Soft warn at (minutes)</span>
            <input
              type="number"
              min={1}
              max={1440}
              value={settings.soft_warn_min}
              onChange={(e) =>
                setSettings({
                  ...settings,
                  soft_warn_min: Number(e.target.value),
                })
              }
              className="w-20 rounded-md bg-canvas px-2 py-1 text-right text-xs"
            />
          </label>
          <label className="flex items-center justify-between gap-3">
            <span>Hard cap at (minutes)</span>
            <input
              type="number"
              min={1}
              max={1440}
              value={settings.hard_cap_min}
              onChange={(e) =>
                setSettings({
                  ...settings,
                  hard_cap_min: Number(e.target.value),
                })
              }
              className="w-20 rounded-md bg-canvas px-2 py-1 text-right text-xs"
            />
          </label>
        </div>
      </Section>

      <Section
        title="Guide templates"
        subtitle="Reusable goals + notes you can attach to a guided meeting session."
      >
        <GuideTemplateManager />
      </Section>

      <SummaryPromptModal
        isOpen={isModalOpen}
        onClose={() => setIsModalOpen(false)}
        currentPrompt={settings.summary_prompt}
        onSave={handleSavePrompt}
      />
    </div>
  );
}

function GeneralPage() {
  const gates = uiGates(useCapabilities());

  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Audio feedback"
        subtitle="Subtle blips when recording starts, stops, and a log capture is ready for review."
      >
        <AudioFeedbackToggle />
      </Section>

      {/* Mute-while-recording is implemented via osascript volume control
       *  (audio/mute.rs), which is macOS-only — hide on other platforms. */}
      {gates.showSystemAudio && (
        <Section
          title="Mute while recording"
          subtitle="Pause music and system audio while the hotkey is held, then restore it when you release."
        >
          <MuteWhileRecordingToggle />
        </Section>
      )}

      <Section
        title="Daily recap"
        subtitle="A morning notification that summarizes yesterday's meetings, notes, and dictations."
      >
        <DailyRecapSection />
      </Section>

      <Section
        title="Startup"
        subtitle="Launch Echo Scribe automatically when you log in."
      >
        <StartAtLoginToggle />
      </Section>

      {/* Self-update swaps the macOS .app bundle — gate on the same capability
       *  as the update banner and uninstall page. */}
      {gates.showSelfUpdate && (
        <Section
          title="Updates"
          subtitle="Echo Scribe checks for updates automatically each day. You can also check now."
        >
          <UpdateSettingsSection />
        </Section>
      )}
    </div>
  );
}

function UpdateSettingsSection() {
  const { check, checking } = useUpdateCheck();
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getAppVersion()
      .then((v) => {
        if (!cancelled) setVersion(v);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="flex items-center justify-between gap-4">
      <p className="text-[13px] text-muted">
        {version ? `Current version ${version}` : "Current version"}
      </p>
      <button
        type="button"
        onClick={() => void check()}
        disabled={checking}
        className="shrink-0 cursor-pointer rounded-md border border-line px-3 py-1.5 text-[13px] font-medium text-fg transition-colors hover:bg-elevated disabled:cursor-not-allowed disabled:opacity-60"
      >
        {checking ? "Checking…" : "Check for Updates"}
      </button>
    </div>
  );
}

function DrivePage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Google Drive"
        subtitle="Upload screen recordings to Drive and get an anyone-with-the-link share URL. The app only sees files it creates (scope drive.file)."
      >
        <DriveSettings />
      </Section>
    </div>
  );
}

function ProjectsPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Projects"
        subtitle="Rename, archive, or unarchive projects."
      >
        <ProjectManager />
      </Section>
    </div>
  );
}

function PermissionsPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Permissions"
        subtitle="Re-grant microphone or accessibility access if something feels broken. Reset & quit clears macOS's TCC grants for both services so the next launch re-prompts cleanly."
      >
        <PermissionsSection />
      </Section>
    </div>
  );
}

function DiagnosticsPage() {
  return (
    <div className="flex flex-col gap-8">
      <Section
        title="Diagnostics"
        subtitle="Inspect the rolling crash log Echo Scribe writes to your home folder."
      >
        <DiagnosticsPane />
      </Section>

      <ResetSection />
    </div>
  );
}

function UninstallPage() {
  const [busy, setBusy] = useState<"app" | "all" | null>(null);
  const toasts = useToasts();

  const uninstall = async (deleteData: boolean) => {
    const confirmed = await ask(
      deleteData
        ? "Uninstall Echo Scribe and remove all local data? This includes captured items, recordings, downloaded models, settings, logs, the local event archive, and the Google Drive connection. Files will be moved to the Trash, but the Drive credential and macOS permission grants will be removed."
        : "Uninstall Echo Scribe but keep all local data? The app will quit and move to the Trash. Reinstalling later will restore your items, recordings, models, settings, and preferences.",
      {
        title: deleteData
          ? "Uninstall Echo Scribe and data"
          : "Uninstall Echo Scribe",
        kind: "warning",
      },
    );
    if (!confirmed) return;

    setBusy(deleteData ? "all" : "app");
    try {
      await uninstallApplication(deleteData);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Uninstall failed: ${e instanceof Error ? e.message : String(e)}`,
      });
      setBusy(null);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-line bg-canvas p-4">
        <div className="flex items-start justify-between gap-5">
          <div>
            <h2 className="text-sm font-semibold text-fg">
              Uninstall application
            </h2>
            <p className="mt-1 text-xs leading-relaxed text-muted">
              Removes only Echo Scribe. Your captured items, recordings,
              downloaded models, settings, and preferences stay on this Mac
              and are available after reinstalling.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void uninstall(false)}
            disabled={busy !== null}
            className="shrink-0 rounded-md border border-line px-3 py-1.5 text-xs font-medium text-fg transition-colors hover:bg-elevated disabled:opacity-50"
          >
            {busy === "app" ? "Uninstalling…" : "Uninstall app"}
          </button>
        </div>
      </div>

      <div className="rounded-lg border border-danger/40 bg-danger/10 p-4">
        <div className="flex items-start justify-between gap-5">
          <div>
            <h2 className="text-sm font-semibold text-danger">
              Uninstall application and data
            </h2>
            <p className="mt-1 text-xs leading-relaxed text-muted">
              Removes Echo Scribe and all of its local data, including items,
              recordings, models, settings, logs, and the local event archive.
              Use this only when you want the next install to start fresh.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void uninstall(true)}
            disabled={busy !== null}
            className="shrink-0 rounded-md border border-danger/50 bg-danger/15 px-3 py-1.5 text-xs font-medium text-danger transition-colors hover:bg-danger/25 disabled:opacity-50"
          >
            {busy === "all" ? "Uninstalling…" : "Uninstall app & data"}
          </button>
        </div>
      </div>

      <p className="text-[11px] leading-relaxed text-faint">
        The application and filesystem data are moved to the macOS Trash.
      </p>
    </div>
  );
}

function Section({
  title,
  subtitle,
  children,
}: {
  title: string;
  subtitle: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h2 className="text-[13px] font-semibold tracking-tight text-fg">
        {title}
      </h2>
      <p className="mt-1 text-[12px] leading-relaxed text-muted">{subtitle}</p>
      <div className="mt-4">{children}</div>
    </section>
  );
}

function AudioFeedbackToggle() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await getAudioFeedbackEnabled();
        if (!cancelled) setEnabled(v);
      } catch {
        if (!cancelled) setEnabled(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onToggle = async (next: boolean) => {
    setBusy(true);
    try {
      await setAudioFeedbackEnabled(next);
      setEnabled(next);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't update audio feedback: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
      <div>
        <div className="text-sm font-semibold text-fg">
          Play recording sounds
        </div>
        <p className="text-xs text-muted">
          Three short tones tied to start, stop, and classification ready.
        </p>
      </div>
      <input
        type="checkbox"
        disabled={busy || enabled === null}
        checked={enabled ?? true}
        onChange={(e) => void onToggle(e.target.checked)}
        className="h-4 w-4 cursor-pointer accent-accent"
      />
    </label>
  );
}

function MuteWhileRecordingToggle() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await getMuteWhileRecording();
        if (!cancelled) setEnabled(v);
      } catch {
        if (!cancelled) setEnabled(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onToggle = async (next: boolean) => {
    setBusy(true);
    try {
      await setMuteWhileRecording(next);
      setEnabled(next);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't update mute setting: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
      <div>
        <div className="text-sm font-semibold text-fg">
          Mute system audio while recording
        </div>
        <p className="text-xs text-muted">
          Silences music and video playback for the duration of the recording.
        </p>
      </div>
      <input
        type="checkbox"
        disabled={busy || enabled === null}
        checked={enabled ?? false}
        onChange={(e) => void onToggle(e.target.checked)}
        className="h-4 w-4 cursor-pointer accent-accent"
      />
    </label>
  );
}

function AppLauncherSettingsSection() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [counter, setCounter] = useState<number>(0);
  const [templates, setTemplates] = useState<CommonActionTemplate[]>([]);
  const [routingEnabled, setRoutingEnabled] = useState<boolean | null>(null);
  const [triggerWord, setTriggerWord] = useState<string>("echo");
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [en, cnt, tmpl, routEnabled, trgWord] = await Promise.all([
          getAppLauncherEnabled(),
          getActionCounter(),
          getCommonActions(),
          getTriggerWordRoutingEnabled().catch(() => false),
          getActionTriggerWord().catch(() => "echo"),
        ]);
        if (!cancelled) {
          setEnabled(en);
          setCounter(cnt);
          setTemplates(tmpl);
          setRoutingEnabled(routEnabled);
          setTriggerWord(trgWord);
        }
      } catch (e) {
        console.error("Failed to load launcher settings:", e);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onToggle = async (next: boolean) => {
    setBusy(true);
    try {
      await setAppLauncherEnabled(next);
      setEnabled(next);
      toasts.push({
        tone: "success",
        message: next ? "Voice Command App Launcher enabled" : "Voice Command App Launcher disabled",
      });
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't update launcher setting: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  const onReset = async () => {
    setBusy(true);
    try {
      await resetActionCounter();
      setCounter(0);
      toasts.push({
        tone: "success",
        message: "Action counter reset to 0",
      });
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't reset counter: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3 transition-all duration-200 hover:border-accent/40">
        <div>
          <div className="text-sm font-semibold text-fg flex items-center gap-1.5">
            <span>Voice command app launcher</span>
            <span className="rounded bg-accent/15 px-1.5 py-0.5 text-[10px] font-medium text-accent">Sonoma+</span>
          </div>
          <p className="text-xs text-muted mt-0.5">
            Intercept voice dictation to launch apps, open links, compose emails, and manage counters.
          </p>
        </div>
        <input
          type="checkbox"
          disabled={busy || enabled === null}
          checked={enabled ?? false}
          onChange={(e) => void onToggle(e.target.checked)}
          className="h-4 w-4 cursor-pointer accent-accent"
        />
      </label>

      {enabled && (
        <div className="rounded-lg border border-line bg-canvas p-4 flex flex-col gap-4 transition-all duration-300">
          {/* Option 2: Prefix-Based Routing */}
          <div className="border border-line rounded-lg p-4 bg-surface/30 flex flex-col gap-3">
            <div className="flex items-center justify-between">
              <div>
                <span className="text-xs font-semibold text-fg block">Prefix-Based Trigger Routing</span>
                <span className="text-[11px] text-muted block mt-0.5">
                  Route normal voice dictations to AI only if they start with a trigger word (e.g. "echo"). Prevents dictation latency!
                </span>
              </div>
              <input
                type="checkbox"
                disabled={busy || routingEnabled === null}
                checked={routingEnabled ?? false}
                onChange={async (e) => {
                  const val = e.target.checked;
                  setRoutingEnabled(val);
                  try {
                    await setTriggerWordRoutingEnabled(val);
                    toasts.push({ tone: "success", message: `Prefix trigger routing ${val ? "enabled" : "disabled"}` });
                  } catch (err) {
                    setRoutingEnabled(!val);
                    toasts.push({ tone: "error", message: "Failed to update prefix routing settings" });
                  }
                }}
                className="h-4 w-4 cursor-pointer accent-accent"
              />
            </div>

            {routingEnabled && (
              <div className="flex flex-col gap-1.5 mt-1 border-t border-line/30 pt-3">
                <label className="text-[11px] font-medium text-muted">Trigger Prefix Word</label>
                <div className="flex gap-2">
                  <input
                    type="text"
                    value={triggerWord}
                    onChange={(e) => setTriggerWord(e.target.value)}
                    onBlur={async () => {
                      const word = triggerWord.trim();
                      if (!word) {
                        setTriggerWord("echo");
                        await setActionTriggerWord("echo");
                        return;
                      }
                      try {
                        await setActionTriggerWord(word);
                        toasts.push({ tone: "success", message: `Trigger word updated to "${word}"` });
                      } catch (err) {
                        toasts.push({ tone: "error", message: "Failed to save trigger word" });
                      }
                    }}
                    className="flex-1 bg-surface border border-line rounded-md px-2.5 py-1 text-xs text-fg focus:outline-none focus:border-accent"
                    placeholder="echo"
                  />
                  <button
                    type="button"
                    onClick={async () => {
                      setTriggerWord("echo");
                      await setActionTriggerWord("echo");
                      toasts.push({ tone: "success", message: 'Trigger word reset to "echo"' });
                    }}
                    className="rounded-md border border-line bg-surface px-2.5 py-1 text-xs font-medium text-fg hover:bg-elevated hover:text-accent transition-colors"
                  >
                    Reset
                  </button>
                </div>
                <p className="text-[10px] text-faint italic leading-snug">
                  * Note: "echo" triggers loose phonetic matches (echo, eco, ekko, hecho) automatically!
                </p>
              </div>
            )}
          </div>

          {/* Option 3: Dedicated Action Hotkey */}
          <div className="border border-line rounded-lg p-4 bg-surface/30 flex flex-col gap-3">
            <div>
              <span className="text-xs font-semibold text-fg block">Dedicated Action Hotkey</span>
              <span className="text-[11px] text-muted block mt-0.5">
                Press a custom key combination to trigger Action Command mode directly. Always runs AI and bypasses prefix rules.
              </span>
            </div>
            <div className="mt-1">
              <HotkeyRebinder
                action="action"
                load={getActionBinding}
                save={updateActionBinding}
              />
            </div>
          </div>

          {/* Edit selection: voice-rewrite highlighted text in place */}
          <div className="border border-line rounded-lg p-4 bg-surface/30 flex flex-col gap-3">
            <div>
              <span className="text-xs font-semibold text-fg block">Edit Selection Hotkey</span>
              <span className="text-[11px] text-muted block mt-0.5">
                Highlight text in any app, hold this hotkey, and speak an instruction (e.g. "make this more concise", "translate to French"). The local model rewrites the selection in place.
              </span>
            </div>
            <div className="mt-1">
              <HotkeyRebinder
                action="edit-selection"
                load={getEditSelectionBinding}
                save={updateEditSelectionBinding}
              />
            </div>
          </div>
          <div className="flex items-center justify-between bg-surface-2/40 border border-line/50 rounded-lg p-3">
            <div className="flex flex-col">
              <span className="text-xs font-semibold text-fg">Automated Actions Count</span>
              <span className="text-[11px] text-muted">Number of voice actions run successfully</span>
            </div>
            <div className="flex items-center gap-3">
              <span className="font-mono text-lg font-bold text-accent px-2.5 py-0.5 rounded-md bg-accent/10 border border-accent/20">
                {counter}
              </span>
              <button
                type="button"
                disabled={busy}
                onClick={() => void onReset()}
                className="rounded-md border border-line bg-surface px-2.5 py-1 text-xs font-medium text-fg hover:bg-elevated hover:text-accent transition-colors disabled:opacity-50"
              >
                Reset
              </button>
            </div>
          </div>

          <div className="flex flex-col gap-2">
            <div className="text-xs font-bold tracking-wide uppercase text-[10px] text-muted mb-1">
              Voice Action Cheatsheet
            </div>
            <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
              {templates.map((t) => (
                <div key={t.category} className="border border-line rounded-lg p-3 bg-surface/50 hover:bg-surface transition-colors flex flex-col gap-2">
                  <div className="flex flex-col">
                    <span className="text-xs font-bold text-accent">{t.category}</span>
                    <span className="text-[10px] text-muted leading-snug mt-0.5">{t.description}</span>
                  </div>
                  <div className="flex flex-wrap gap-1 mt-1">
                    {t.voice_phrases.map((phrase) => (
                      <span
                        key={phrase}
                        className="rounded font-mono bg-surface-2 px-1.5 py-0.5 border border-line text-[10px] text-fg whitespace-nowrap"
                      >
                        "{phrase}"
                      </span>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function FormatTemplatesSection() {
  const [templates, setTemplates] = useState<FormatTemplate[] | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const list = await getFormatTemplates();
        if (!cancelled) setTemplates(list);
      } catch (e) {
        console.error("Failed to load format templates:", e);
        if (!cancelled) setTemplates([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const persist = async (next: FormatTemplate[]) => {
    setBusy(true);
    try {
      await setFormatTemplates(next);
      setTemplates(next);
      toasts.push({ tone: "success", message: "Format templates saved" });
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't save templates: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  const updateOne = (id: string, patch: Partial<FormatTemplate>) => {
    if (!templates) return;
    setTemplates(templates.map((t) => (t.id === id ? { ...t, ...patch } : t)));
  };

  const slugify = (s: string) =>
    s
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "_")
      .replace(/^_+|_+$/g, "")
      .slice(0, 32) || `tpl_${Date.now().toString(36)}`;

  const addNew = () => {
    if (!templates) return;
    let base = "new_template";
    let id = base;
    let n = 1;
    const taken = new Set(templates.map((t) => t.id));
    while (taken.has(id)) {
      id = `${base}_${n++}`;
    }
    const next: FormatTemplate[] = [
      ...templates,
      {
        id,
        name: "New template",
        trigger_phrases: ["format as new"],
        system_prompt:
          "Rewrite the user's raw dictation in the desired style. Output ONLY the rewritten text.",
      },
    ];
    setTemplates(next);
  };

  const removeOne = async (id: string) => {
    if (!templates) return;
    const ok = await ask("Delete this format template? This cannot be undone.", {
      title: "Delete template",
      kind: "warning",
    });
    if (!ok) return;
    const next = templates.filter((t) => t.id !== id);
    await persist(next);
  };

  const renameId = (oldId: string, newName: string) => {
    if (!templates) return;
    const newId = slugify(newName);
    if (newId === oldId || templates.some((t) => t.id === newId)) {
      // keep id stable if collision or unchanged; just update name
      updateOne(oldId, { name: newName });
      return;
    }
    setTemplates(
      templates.map((t) =>
        t.id === oldId ? { ...t, id: newId, name: newName } : t,
      ),
    );
  };

  if (templates === null) {
    return <div className="text-xs text-muted">Loading templates…</div>;
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-line/50 bg-surface-2/40 p-3">
        <p className="text-[11px] text-muted leading-snug">
          When your dictation starts with one of a template's trigger phrases
          (e.g. <span className="font-mono text-fg">"echo, format as email …"</span>),
          the rest of the sentence is rewritten through the template's system
          prompt before it's pasted at your cursor. Works with both the "echo"
          prefix and the dedicated Action hotkey.
        </p>
      </div>

      {templates.length === 0 && (
        <div className="text-xs text-muted italic">
          No templates yet. Add one to enable voice formatting.
        </div>
      )}

      <div className="flex flex-col gap-3">
        {templates.map((t) => (
          <div
            key={t.id}
            className="rounded-lg border border-line bg-canvas p-3 flex flex-col gap-2.5"
          >
            <div className="flex items-center gap-2">
              <input
                type="text"
                value={t.name}
                onChange={(e) => renameId(t.id, e.target.value)}
                className="flex-1 bg-surface border border-line rounded-md px-2.5 py-1 text-sm font-semibold text-fg focus:outline-none focus:border-accent"
                placeholder="Template name"
              />
              <span className="font-mono text-[10px] text-muted px-1.5 py-0.5 rounded bg-surface-2 border border-line">
                id: {t.id}
              </span>
              <button
                type="button"
                disabled={busy}
                onClick={() => void removeOne(t.id)}
                className="rounded-md border border-line bg-surface px-2.5 py-1 text-xs font-medium text-fg hover:bg-elevated hover:text-red-400 transition-colors disabled:opacity-50"
              >
                Delete
              </button>
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-[11px] font-medium text-muted">
                Trigger phrases (comma-separated)
              </label>
              <input
                type="text"
                value={t.trigger_phrases.join(", ")}
                onChange={(e) =>
                  updateOne(t.id, {
                    trigger_phrases: e.target.value
                      .split(",")
                      .map((p) => p.trim())
                      .filter((p) => p.length > 0),
                  })
                }
                className="bg-surface border border-line rounded-md px-2.5 py-1 text-xs text-fg focus:outline-none focus:border-accent"
                placeholder="format as email, make this an email"
              />
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-[11px] font-medium text-muted">
                System prompt
              </label>
              <textarea
                value={t.system_prompt}
                onChange={(e) =>
                  updateOne(t.id, { system_prompt: e.target.value })
                }
                rows={6}
                className="bg-surface border border-line rounded-md px-2.5 py-1.5 text-xs text-fg leading-relaxed focus:outline-none focus:border-accent font-mono resize-y"
                placeholder="Describe how the model should rewrite the dictation. Be specific about tone, length, and what to omit."
              />
            </div>
          </div>
        ))}
      </div>

      <div className="flex items-center gap-2">
        <button
          type="button"
          disabled={busy}
          onClick={addNew}
          className="rounded-md border border-line bg-surface px-3 py-1.5 text-xs font-medium text-fg hover:bg-elevated hover:text-accent transition-colors disabled:opacity-50"
        >
          + Add template
        </button>
        <button
          type="button"
          disabled={busy || templates === null}
          onClick={() => void persist(templates)}
          className="rounded-md border border-accent/40 bg-accent/15 px-3 py-1.5 text-xs font-semibold text-accent hover:bg-accent/25 transition-colors disabled:opacity-50"
        >
          Save changes
        </button>
      </div>
    </div>
  );
}

function DiagnosticsPane() {
  const [logDir, setLogDir] = useState<string>("");
  const [recent, setRecent] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const toasts = useToasts();

  const loadRecent = async () => {
    setBusy(true);
    setError(null);
    try {
      const txt = await diagnosticsRecentLog(200);
      setRecent(txt);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const dir = await diagnosticsLogDir();
        if (!cancelled) setLogDir(dir);
      } catch {
        /* ignore */
      }
    })();
    void loadRecent();
    return () => {
      cancelled = true;
    };
  }, []);

  const onOpenFolder = async () => {
    try {
      await diagnosticsOpenLogFolder();
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't open folder: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  };

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3 rounded-lg border border-line bg-canvas p-3">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold text-fg">
            Log folder
          </div>
          <p className="truncate text-xs text-muted" title={logDir}>
            {logDir || "—"}
          </p>
        </div>
        <button
          type="button"
          onClick={() => void onOpenFolder()}
          disabled={!logDir}
          className="rounded border border-line px-3 py-1 text-xs hover:bg-elevated disabled:opacity-50"
        >
          Open
        </button>
      </div>

      <div className="rounded-lg border border-line bg-canvas p-3">
        <div className="flex items-center justify-between">
          <div className="text-sm font-semibold text-fg">
            Recent log (last 200 lines)
          </div>
          <button
            type="button"
            onClick={() => void loadRecent()}
            disabled={busy}
            className="rounded border border-line px-2 py-0.5 text-xs hover:bg-elevated disabled:opacity-50"
          >
            {busy ? "…" : "Refresh"}
          </button>
        </div>
        {error ? (
          <p className="mt-2 text-xs text-warninging">{error}</p>
        ) : null}
        <pre className="mt-2 max-h-64 overflow-auto rounded-md border border-line bg-canvas p-2 font-mono text-[11px] leading-snug text-muted">
          {recent || "(no log content yet)"}
        </pre>
      </div>
    </div>
  );
}

function TestInference() {
  const [prompt, setPrompt] = useState("Say hello in five words.");
  const [response, setResponse] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onRun = async () => {
    setBusy(true);
    setError(null);
    setResponse(null);
    try {
      const r = await testLlmInference(prompt);
      setResponse(r);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rounded-lg border border-line bg-canvas p-3">
      <p className="text-xs font-semibold tracking-tight text-muted">
        Test inference
      </p>
      <div className="mt-2 flex gap-2">
        <input
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          className="flex-1 rounded-md border border-line bg-surface px-3 py-1 text-sm focus:border-accent focus:outline-none"
        />
        <button
          type="button"
          onClick={() => void onRun()}
          disabled={busy || !prompt.trim()}
          className="rounded-md bg-accent px-3 py-1 text-xs font-semibold text-canvas hover:bg-accent-hover disabled:opacity-50"
        >
          {busy ? "Running…" : "Run"}
        </button>
      </div>
      {error ? <p className="mt-2 text-xs text-danger">{error}</p> : null}
      {response ? (
        <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap rounded-md border border-line bg-canvas p-2 text-xs text-fg">
          {response}
        </pre>
      ) : null}
    </div>
  );
}

const LLM_UNLOAD_OPTIONS: { label: string; secs: number }[] = [
  { label: "1 minute", secs: 60 },
  { label: "2 minutes", secs: 120 },
  { label: "5 minutes", secs: 300 },
  { label: "15 minutes", secs: 900 },
  { label: "Keep loaded", secs: 0 },
];

function LlmUnloadTimeoutSelect() {
  const [secs, setSecs] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await getLlmUnloadSecs();
        if (!cancelled) setSecs(v);
      } catch {
        if (!cancelled) setSecs(120);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onChange = async (next: number) => {
    setBusy(true);
    try {
      await setLlmUnloadSecs(next);
      setSecs(next);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't update AI memory setting: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
      <div>
        <div className="text-sm font-semibold text-fg">
          Unload after idle
        </div>
        <p className="text-xs text-muted">
          Frees RAM when you haven't used log-capture for a while.
        </p>
      </div>
      <select
        disabled={busy || secs === null}
        value={secs ?? 120}
        onChange={(e) => void onChange(Number(e.target.value))}
        className="rounded border border-line bg-surface px-2 py-1 text-xs text-fg focus:border-accent focus:outline-none disabled:opacity-50"
      >
        {LLM_UNLOAD_OPTIONS.map(({ label, secs: s }) => (
          <option key={s} value={s}>
            {label}
          </option>
        ))}
      </select>
    </div>
  );
}

const ASR_UNLOAD_OPTIONS: { label: string; secs: number }[] = [
  { label: "30 seconds", secs: 30 },
  { label: "1 minute", secs: 60 },
  { label: "2 minutes", secs: 120 },
  { label: "5 minutes", secs: 300 },
  { label: "15 minutes", secs: 900 },
  { label: "Keep loaded", secs: 0 },
];

function AsrUnloadTimeoutSelect() {
  const [secs, setSecs] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await getAsrUnloadSecs();
        if (!cancelled) setSecs(v);
      } catch {
        if (!cancelled) setSecs(120);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onChange = async (next: number) => {
    setBusy(true);
    try {
      await setAsrUnloadSecs(next);
      setSecs(next);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't update speech model memory setting: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
      <div>
        <div className="text-sm font-semibold text-fg">
          Unload after idle
        </div>
        <p className="text-xs text-muted">
          Frees RAM when you haven't dictated for a while. The model reloads automatically on next use.
        </p>
      </div>
      <select
        disabled={busy || secs === null}
        value={secs ?? 120}
        onChange={(e) => void onChange(Number(e.target.value))}
        className="rounded border border-line bg-surface px-2 py-1 text-xs text-fg focus:border-accent focus:outline-none disabled:opacity-50"
      >
        {ASR_UNLOAD_OPTIONS.map(({ label, secs: s }) => (
          <option key={s} value={s}>
            {label}
          </option>
        ))}
      </select>
    </div>
  );
}

function ResetSection() {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  const onReset = async () => {
    const confirmed = await ask(
      "Reset onboarding? This clears the settings store and quits the app. You'll need to relaunch.",
      { title: "Reset onboarding", kind: "warning" },
    );
    if (!confirmed) {
      return;
    }
    setBusy(true);
    try {
      await resetOnboardingAndQuit();
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Reset failed: ${e instanceof Error ? e.message : String(e)}`,
      });
      setBusy(false);
    }
  };

  return (
    <section>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="text-xs text-faint underline-offset-2 hover:text-muted hover:underline"
      >
        {open ? "Hide reset options" : "Show reset options"}
      </button>
      {open ? (
        <div className="mt-3 rounded-lg border border-danger/40 bg-danger/15 p-3">
          <p className="text-xs text-danger">
            Wipes the settings store (hotkeys, active models, persisted
            preferences) and quits the app. Your captured items are
            preserved — they live in the SQLite database, not the settings store.
          </p>
          <button
            type="button"
            onClick={() => void onReset()}
            disabled={busy}
            className="mt-3 rounded-md border border-danger/40 bg-danger/15 px-3 py-1 text-xs text-danger hover:bg-danger/15 disabled:opacity-50"
          >
            {busy ? "Resetting…" : "Reset onboarding"}
          </button>
        </div>
      ) : null}
    </section>
  );
}

function DriveSettings() {
  const [status, setStatus] = useState<DriveStatus>({ connected: false, email: null });
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [showByo, setShowByo] = useState(false);
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [showSetup, setShowSetup] = useState(false);
  const [saving, setSaving] = useState(false);
  const [folderName, setFolderName] = useState("Echo Scribe");
  const [makePublic, setMakePublic] = useState(true);

  useEffect(() => {
    void driveStatus().then(setStatus);
    void getDriveClientId().then((id) => {
      setClientId(id);
      setShowByo(id.trim().length > 0);
    });
    void getDrivePrefs().then((p) => {
      setFolderName(p.folder_name);
      setMakePublic(p.make_public);
    });
  }, []);

  const savePrefs = async (name: string, isPublic: boolean) => {
    try {
      await setDrivePrefs(name.trim() || "Echo Scribe", isPublic);
    } catch (e) {
      setErr(String(e));
    }
  };

  const onConnect = async () => {
    setBusy(true);
    setErr(null);
    try {
      if (showByo && clientId.trim() && clientSecret.trim()) {
        await setDriveClientCredentials(clientId, clientSecret);
      }
      setStatus(await driveConnect());
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const onDisconnect = async () => {
    setBusy(true);
    setErr(null);
    try {
      await driveDisconnect();
      setStatus({ connected: false, email: null });
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const onSaveClient = async () => {
    setSaving(true);
    setErr(null);
    try {
      if (clientId.trim() && clientSecret.trim()) {
        await setDriveClientCredentials(clientId, clientSecret);
      }
      setShowSetup(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="flex flex-col gap-3">
      {status.connected ? (
        <div className="flex items-center gap-3">
          <span className="text-[13px]">
            Connected{status.email ? ` as ${status.email}` : ""}.
          </span>
          <button
            onClick={() => void onDisconnect()}
            disabled={busy}
            className="rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-surface disabled:opacity-50"
          >
            Disconnect
          </button>
        </div>
      ) : (
        <button
          onClick={() => void onConnect()}
          disabled={busy}
          className="self-start rounded-md bg-accent px-3 py-1.5 text-[13px] font-medium text-canvas disabled:opacity-50"
        >
          {busy ? "Connecting…" : "Connect Drive"}
        </button>
      )}
      <div className="flex flex-col gap-1">
        <label className="text-[12px] text-muted">Drive folder for uploads</label>
        <input
          value={folderName}
          onChange={(e) => setFolderName(e.target.value)}
          onBlur={() => void savePrefs(folderName, makePublic)}
          placeholder="Echo Scribe"
          className="w-full max-w-xs rounded-md border border-line bg-canvas px-2 py-1.5 text-[13px]"
        />
      </div>
      <div className="flex flex-col gap-1">
        <label className="flex items-center gap-2 text-[12px] text-muted">
          <input
            type="checkbox"
            checked={makePublic}
            onChange={(e) => {
              setMakePublic(e.target.checked);
              void savePrefs(folderName, e.target.checked);
            }}
          />
          Default sharing: anyone with the link can view
        </label>
        <span className="pl-6 text-[11px] text-muted/70">
          Applied to each file (not the folder). Override per video when you upload.
        </span>
      </div>

      <label className="flex items-center gap-2 text-[12px] text-muted">
        <input
          type="checkbox"
          checked={showByo}
          onChange={(e) => setShowByo(e.target.checked)}
        />
        Use my own Google OAuth client (removes the unverified-app warning)
      </label>
      {showByo ? (
        <div className="flex items-center gap-2">
          <span className="text-[12px] text-muted">
            {clientId.trim()
              ? `Client configured (…${clientId.trim().slice(-14)})`
              : "No client configured."}
          </span>
          <button
            onClick={() => setShowSetup(true)}
            className="rounded-md border border-line px-2.5 py-1 text-[12px] hover:bg-surface"
          >
            {clientId.trim() ? "Edit" : "Set up"}
          </button>
          <button
            onClick={() => setShowSetup(true)}
            aria-label="How to create a client ID"
            title="How to create a client ID"
            className="grid h-5 w-5 place-items-center rounded-full border border-line text-[11px] text-muted hover:bg-surface"
          >
            ?
          </button>
        </div>
      ) : null}
      {err ? <div className="text-[12px] text-red-400">{err}</div> : null}

      {showSetup ? (
        <DriveClientSetupModal
          clientId={clientId}
          clientSecret={clientSecret}
          saving={saving}
          onClientId={setClientId}
          onClientSecret={setClientSecret}
          onSave={() => void onSaveClient()}
          onCancel={() => setShowSetup(false)}
        />
      ) : null}
    </div>
  );
}

function DriveClientSetupModal(props: {
  clientId: string;
  clientSecret: string;
  saving: boolean;
  onClientId: (v: string) => void;
  onClientSecret: (v: string) => void;
  onSave: () => void;
  onCancel: () => void;
}) {
  return (
    <Dialog
      onClose={props.onCancel}
      labelledBy="drive-client-setup-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      panelClassName="max-h-[85vh] w-full max-w-lg overflow-y-auto rounded-lg border border-line bg-canvas p-5 text-fg shadow-xl"
    >
        <h3 id="drive-client-setup-title" className="mb-3 text-[15px] font-semibold">
          Set up your own Google OAuth client
        </h3>
        <ol className="mb-4 list-decimal space-y-2 pl-5 text-[12px] leading-relaxed text-muted">
          <li>
            Open the{" "}
            <a
              className="text-accent underline"
              href="https://console.cloud.google.com/"
              target="_blank"
              rel="noreferrer"
            >
              Google Cloud Console
            </a>{" "}
            and create a project (or pick an existing one).
          </li>
          <li>
            APIs &amp; Services → Library → search <b>Google Drive API</b> →{" "}
            <b>Enable</b>.
          </li>
          <li>
            APIs &amp; Services → OAuth consent screen → <b>External</b> → fill in
            the app name and your email → add the scope{" "}
            <code>.../auth/drive.file</code> → add your Google account under{" "}
            <b>Test users</b> → Save. Leave it in <b>Testing</b>.
          </li>
          <li>
            APIs &amp; Services → Credentials → Create credentials →{" "}
            <b>OAuth client ID</b> → Application type: <b>Desktop app</b> → Create.
          </li>
          <li>
            Copy the <b>Client ID</b> and <b>Client secret</b> into the fields
            below. No redirect URI is needed — Desktop apps allow loopback
            automatically.
          </li>
        </ol>
        <div className="flex flex-col gap-2">
          <input
            value={props.clientId}
            onChange={(e) => props.onClientId(e.target.value)}
            placeholder="Client ID (…apps.googleusercontent.com)"
            className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-[13px]"
          />
          <input
            value={props.clientSecret}
            onChange={(e) => props.onClientSecret(e.target.value)}
            placeholder="Client secret"
            className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-[13px]"
          />
          <p className="text-[11px] text-muted">
            Stored securely on this Mac. Leave the secret blank when editing to
            keep the saved one.
          </p>
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <button
            onClick={props.onCancel}
            className="rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-surface"
          >
            Cancel
          </button>
          <button
            onClick={props.onSave}
            disabled={props.saving}
            className="rounded-md bg-accent px-3 py-1.5 text-[13px] font-medium text-canvas disabled:opacity-50"
          >
            {props.saving ? "Saving…" : "Save"}
          </button>
        </div>
    </Dialog>
  );
}

function DailyRecapSection() {
  const [settings, setSettings] = useState<DailyRecapSettingsT | null>(null);
  const [busy, setBusy] = useState(false);
  const toasts = useToasts();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await getDailyRecapSettings();
        if (!cancelled) setSettings(v);
      } catch (e) {
        if (!cancelled) {
          toasts.push({
            tone: "error",
            message: `Couldn't load daily recap settings: ${
              e instanceof Error ? e.message : String(e)
            }`,
          });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
    // toasts is stable from context; intentionally excluded to avoid re-fetch
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const save = async (patch: Partial<DailyRecapSettingsT>) => {
    if (!settings) return;
    const next = { ...settings, ...patch };
    setSettings(next);
    setBusy(true);
    try {
      await setDailyRecapSettings(next);
    } catch (e) {
      toasts.push({
        tone: "error",
        message: `Couldn't save daily recap settings: ${
          e instanceof Error ? e.message : String(e)
        }`,
      });
    } finally {
      setBusy(false);
    }
  };

  if (!settings) {
    return (
      <div className="rounded-lg border border-line bg-canvas p-3 text-xs text-muted">
        Loading…
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
        <div>
          <div className="text-sm font-semibold text-fg">
            Generate a daily recap each morning
          </div>
          <p className="text-xs text-muted">
            One macOS notification at your chosen hour, summarizing yesterday.
          </p>
        </div>
        <input
          type="checkbox"
          disabled={busy}
          checked={settings.enabled}
          onChange={(e) => void save({ enabled: e.target.checked })}
          className="h-4 w-4 cursor-pointer accent-accent"
        />
      </label>

      <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
        <div>
          <div className="text-sm font-semibold text-fg">Deliver at</div>
          <p className="text-xs text-muted">Local time. Default 08:00.</p>
        </div>
        <select
          disabled={busy || !settings.enabled}
          value={settings.deliver_hour}
          onChange={(e) =>
            void save({ deliver_hour: Number(e.target.value) })
          }
          className="rounded-md border border-line bg-canvas px-2 py-1 text-sm text-fg disabled:opacity-50"
        >
          {Array.from({ length: 24 }, (_, h) => (
            <option key={h} value={h}>
              {`${String(h).padStart(2, "0")}:00`}
            </option>
          ))}
        </select>
      </label>

      <label className="flex items-center justify-between rounded-lg border border-line bg-canvas p-3">
        <div>
          <div className="text-sm font-semibold text-fg">Include weekends</div>
          <p className="text-xs text-muted">
            Off by default — no Sunday morning summary unless you want one.
          </p>
        </div>
        <input
          type="checkbox"
          disabled={busy || !settings.enabled}
          checked={settings.include_weekends}
          onChange={(e) => void save({ include_weekends: e.target.checked })}
          className="h-4 w-4 cursor-pointer accent-accent"
        />
      </label>
    </div>
  );
}
