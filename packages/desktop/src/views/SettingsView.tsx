import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import clsx from "clsx";
import {
  Bot,
  Check,
  Cloud,
  Copy,
  Eye,
  EyeOff,
  Folder,
  GitCompareArrows,
  GitPullRequest,
  Import,
  Info,
  Loader2,
  type LucideIcon,
  Monitor,
  Play,
  RotateCcw,
  Settings as SettingsIcon,
  Sparkles,
  Terminal,
  Trash2,
  User,
  X,
} from "lucide-react";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
} from "react";
import { CacheSettings } from "../components/CacheSettings";
import { ScopeChip } from "../components/ScopeChip";
import { useFileTransfers } from "../hooks/useFileTransfers";
import { useWorkspaceSettings } from "../hooks/useWorkspaceSettings";
import { call, dialogs, mainProcess, rpc } from "../ipc";
import { DARK_THEMES, LIGHT_THEMES } from "../lib/editor-themes";
import { usePrefetchProfiles } from "../lib/prefetch-profiles";
import { repositoryWorktree } from "../lib/repository-worktree";
import { useSession } from "../lib/session-context";
import { telemetry } from "../lib/telemetry";
import { importVsCodeTheme } from "../lib/vscode-theme-import";
import { isWorkspaceSetting } from "../lib/workspace-settings-store";
import { DEFAULT_SETTINGS, Settings, useUserSettings } from "../settings";
import { useActiveRepo } from "../state";
import { AgentManagerView } from "./AgentManagerView";
import { DiagnosticsView } from "./DiagnosticsView";
import { HealthView } from "./HealthView";
import { StorageConfigSection } from "./StorageConfigSection";

const DARK_THEME_OPTIONS = DARK_THEMES.map((t) => ({
  value: t.id,
  label: t.label,
}));
const LIGHT_THEME_OPTIONS = LIGHT_THEMES.map((t) => ({
  value: t.id,
  label: t.label,
}));

type SectionId =
  | "appearance"
  | "workspace"
  | "git"
  | "storage"
  | "integrations"
  | "agents"
  | "ssh"
  | "diagnostics"
  | "general";

interface SectionDef {
  id: SectionId;
  label: string;
  icon: LucideIcon;
}

const SECTIONS: SectionDef[] = [
  { id: "appearance", label: "Appearance & Editor", icon: Sparkles },
  { id: "workspace", label: "Workspace", icon: Folder },
  { id: "git", label: "Git & Diff", icon: GitCompareArrows },
  { id: "storage", label: "Storage & Cache", icon: Cloud },
  { id: "integrations", label: "Integrations", icon: GitPullRequest },
  { id: "agents", label: "Agents & AI", icon: Bot },
  { id: "ssh", label: "SSH Remote", icon: Monitor },
  { id: "diagnostics", label: "Diagnostics", icon: Terminal },
  { id: "general", label: "General", icon: Info },
];

export type SettingsScope = "user" | "workspace";

/**
 * Full-pane Settings page. Opened when the user clicks the Settings
 * icon in the activity bar (or hits `Cmd+,`). Left: section nav.
 * Right: the selected section's form. Everything lives in a single
 * component because the forms are small and there's no benefit to
 * code-splitting them.
 */
export function SettingsView() {
  const [section, setSection] = useState<SectionId>("appearance");
  const [scope, setScope] = useState<SettingsScope>("user");
  const activeRepo = useActiveRepo();
  const workspace = useWorkspaceSettings();
  const hasRepo = workspace.hasRepo;

  // Reset to "user" scope when no repo is open
  useEffect(() => {
    if (
      (!activeRepo || !repositoryWorktree(activeRepo)) &&
      scope === "workspace"
    ) {
      setScope("user");
    }
  }, [activeRepo, scope]);

  // Listen for programmatic navigation to a specific section (e.g. from
  // the "Add Agent" button in the chat input).
  useEffect(() => {
    // Map legacy section IDs to new consolidated IDs for backward compat
    const LEGACY_MAP: Record<string, SectionId> = {
      appearance: "appearance",
      editor: "appearance",
      previews: "workspace",
      workspace: "workspace",
      diff: "git",
      commit: "git",
      notifications: "diagnostics",
      prefetch: "storage",
      experiments: "agents",
      forge: "integrations",
      integrations: "integrations",
      storage: "storage",
      cache: "storage",
      memory: "storage",
      agents: "agents",
      ssh: "ssh",
      logs: "diagnostics",
      "pty-daemon": "diagnostics",
      "session-persistence": "workspace",
      telemetry: "diagnostics",
      shortcuts: "general",
      reset: "general",
      about: "general",
      diagnostics: "diagnostics",
      general: "general",
      git: "git",
    };
    function handleNavigate(e: Event) {
      const detail = (e as CustomEvent).detail as
        | { section?: string }
        | undefined;
      if (detail?.section) {
        const mapped =
          LEGACY_MAP[detail.section] ??
          (SECTIONS.some((s) => s.id === detail.section)
            ? (detail.section as SectionId)
            : null);
        if (mapped) setSection(mapped);
      }
    }
    window.addEventListener("crab:settings-navigate", handleNavigate);
    return () =>
      window.removeEventListener("crab:settings-navigate", handleNavigate);
  }, []);

  return (
    <SettingsEditorContext.Provider value={scope}>
      <div className="flex h-full min-h-0 w-full bg-background">
        <nav className="w-56 shrink-0 overflow-auto border-r border-border bg-card py-2">
          <div className="flex items-center gap-2 px-4 py-2 text-sm font-semibold text-foreground">
            <SettingsIcon size={14} />
            Settings
          </div>
          <ul className="mt-2">
            {SECTIONS.map((s) => (
              <li key={s.id}>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => setSection(s.id)}
                  className={clsx(
                    "flex w-full items-center justify-start gap-2 px-4 py-1.5 text-left text-[13px] transition",
                    section === s.id
                      ? "bg-primary/15 text-foreground"
                      : "text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                >
                  <s.icon size={13} />
                  {s.label}
                </Button>
              </li>
            ))}
          </ul>
        </nav>
        <div className="min-h-0 flex-1 overflow-auto">
          <div className="mx-auto max-w-2xl p-8">
            <SettingsScopeToggle
              scope={scope}
              onScopeChange={setScope}
              hasRepo={hasRepo}
              repoName={activeRepo?.label ?? null}
            />
            {workspace.error && (
              <div
                role="alert"
                className="mb-4 rounded border border-destructive/40 p-3 text-xs text-destructive"
              >
                Workspace settings: {workspace.error}{" "}
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="underline"
                  onClick={workspace.reload}
                >
                  Retry loading workspace settings
                </Button>
              </div>
            )}
            {scope === "workspace" &&
              !["appearance", "git"].includes(section) && (
                <p className="mb-4 text-xs text-muted-foreground">
                  These controls manage this device. Choose User to change them.
                </p>
              )}
            <fieldset
              disabled={
                scope === "workspace" &&
                (!hasRepo ||
                  workspace.loading ||
                  !["appearance", "git"].includes(section))
              }
            >
              {section === "appearance" && <AppearanceAndEditor />}
              {section === "workspace" && <WorkspaceSection />}
              {section === "git" && <GitAndDiff />}
              {section === "storage" && <StorageAndCache />}
              {section === "integrations" && <ForgeAuthentication />}
              {section === "agents" && <AgentsAndAI />}
              {section === "ssh" && <SshSettings />}
              {section === "diagnostics" && <DiagnosticsSection />}
              {section === "general" && <GeneralSection />}
            </fieldset>
          </div>
        </div>
      </div>
    </SettingsEditorContext.Provider>
  );
}

const SettingsEditorContext = createContext<SettingsScope>("user");
function useSettingsEditor() {
  const user = useUserSettings(),
    workspace = useWorkspaceSettings();
  const scope = useContext(SettingsEditorContext);
  const update = useCallback(
    <K extends keyof Settings>(key: K, value: Settings[K]) => {
      if (scope === "workspace") void workspace.setWorkspaceSetting(key, value);
      else user.update(key, value);
    },
    [scope, workspace.setWorkspaceSetting, user.update],
  );
  return {
    ...user,
    settings:
      scope === "workspace"
        ? { ...user.settings, ...workspace.workspaceSettings }
        : user.settings,
    update,
  };
}

// ---- Settings Scope Toggle -------------------------------------------

function SettingsScopeToggle({
  scope,
  onScopeChange,
  hasRepo,
  repoName,
}: {
  scope: SettingsScope;
  onScopeChange: (scope: SettingsScope) => void;
  hasRepo: boolean;
  repoName: string | null;
}) {
  return (
    <div className="mb-6">
      <ToggleGroup
        type="single"
        value={scope}
        onValueChange={(value) => {
          if (value) onScopeChange(value as SettingsScope);
        }}
        aria-label="Settings scope"
        className="inline-flex items-center rounded-md border border-border bg-card p-0.5"
      >
        <ToggleGroupItem
          value={"user"}
          className={clsx(
            "flex items-center gap-1.5 rounded px-3 py-1.5 text-xs font-medium transition",
            scope === "user"
              ? "bg-primary/15 text-foreground shadow-sm"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          <User size={12} />
          User
        </ToggleGroupItem>
        <ToggleGroupItem
          value={"workspace"}
          disabled={!hasRepo}
          title={
            hasRepo
              ? "Edit workspace settings"
              : "Open a repository to edit workspace settings"
          }
          className={clsx(
            "flex items-center gap-1.5 rounded px-3 py-1.5 text-xs font-medium transition",
            !hasRepo && "cursor-not-allowed opacity-50",
            scope === "workspace"
              ? "bg-primary/15 text-foreground shadow-sm"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          <Folder size={12} />
          Workspace
        </ToggleGroupItem>
      </ToggleGroup>
      {scope === "workspace" && repoName && (
        <div className="mt-2 flex items-center gap-2 rounded border border-primary/30 bg-primary/5 px-3 py-2 text-xs text-primary">
          <Folder size={12} className="shrink-0" />
          <span>
            Editing workspace settings for{" "}
            <span className="font-semibold">{repoName}</span>
          </span>
        </div>
      )}
    </div>
  );
}

// ---- Section: Appearance & Editor ------------------------------------

function AppearanceAndEditor() {
  const { settings, update } = useSettingsEditor();
  const [importStatus, setImportStatus] = useState<{
    type: "success" | "error";
    message: string;
  } | null>(null);

  async function handleImportVsCodeTheme() {
    setImportStatus(null);
    const result = await dialogs.pickJsonFile("Import VS Code Theme (.json)");
    if (!result) return;

    let parsed: unknown;
    try {
      parsed = JSON.parse(result.content);
    } catch {
      setImportStatus({
        type: "error",
        message: "Failed to parse the selected file as JSON.",
      });
      return;
    }

    const theme = importVsCodeTheme(parsed);
    if (!theme) {
      setImportStatus({
        type: "error",
        message:
          'Not a valid VS Code theme, or too few colors could be mapped. Ensure the file has a "colors" object with workbench color entries.',
      });
      return;
    }

    try {
      await rpc.saveImportedTheme(theme);
      setImportStatus({
        type: "success",
        message: `Imported "${theme.name}" (${theme.colorScheme}).`,
      });
    } catch (e) {
      setImportStatus({
        type: "error",
        message:
          e instanceof Error ? e.message : "Failed to save the imported theme.",
      });
    }
  }

  return (
    <Section
      title="Appearance & Editor"
      subtitle="Theme, layout, and code editor preferences."
    >
      {/* Theme */}
      <SubSection title="Theme">
        <Row
          label="Theme"
          hint="Light, Dark, or follow the OS setting."
          setting="theme"
        >
          <SettingsSelect
            value={settings.theme}
            onChange={(v) => update("theme", v as Settings["theme"])}
            options={[
              { value: "dark", label: "Dark" },
              { value: "light", label: "Light" },
              { value: "hc-dark", label: "High Contrast Dark" },
              { value: "hc-light", label: "High Contrast Light" },
              { value: "system", label: "System" },
            ]}
          />
        </Row>
        <Row
          label="Show activity bar"
          hint="The 48 px icon column with Explorer / Search / Changes."
          setting="showActivityBar"
        >
          <Toggle
            checked={settings.showActivityBar}
            onChange={(v) => update("showActivityBar", v)}
          />
        </Row>
        <Row
          label="Import VS Code theme"
          hint="Import a VS Code theme JSON file. Maps workbench colors to Crab Desktop's palette."
        >
          <Button
            variant="outline"
            type="button"
            className="text-xs"
            onClick={handleImportVsCodeTheme}
            aria-label="Import VS Code theme"
          >
            <Import size={12} /> Import .json
          </Button>
        </Row>
        {importStatus && (
          <div
            className={clsx(
              "rounded border p-2 text-xs",
              importStatus.type === "success"
                ? "border-success/40 bg-success/10 text-success"
                : "border-destructive/40 bg-destructive/10 text-destructive",
            )}
          >
            {importStatus.message}
          </div>
        )}
      </SubSection>

      {/* Editor */}
      <SubSection title="Code Editor">
        <Row
          label="Syntax theme (dark)"
          hint="Color scheme for code highlighting in dark mode."
          setting="editorThemeDark"
        >
          <SettingsSelect
            value={settings.editorThemeDark}
            onChange={(v) => update("editorThemeDark", v)}
            options={DARK_THEME_OPTIONS}
          />
        </Row>
        <Row
          label="Syntax theme (light)"
          hint="Color scheme for code highlighting in light mode."
          setting="editorThemeLight"
        >
          <SettingsSelect
            value={settings.editorThemeLight}
            onChange={(v) => update("editorThemeLight", v)}
            options={LIGHT_THEME_OPTIONS}
          />
        </Row>
        <Row
          label="Font size"
          hint="Monaco editor font size."
          setting="editorFontSize"
        >
          <NumberInput
            value={settings.editorFontSize}
            onChange={(v) => update("editorFontSize", clamp(v, 10, 20))}
            min={10}
            max={20}
            step={1}
            suffix="px"
          />
        </Row>
        <Row label="Word wrap" setting="editorWordWrap">
          <Toggle
            checked={settings.editorWordWrap}
            onChange={(v) => update("editorWordWrap", v)}
          />
        </Row>
        <Row
          label="Show minimap"
          hint="The scrollable code thumbnail on the right edge."
          setting="editorMinimap"
        >
          <Toggle
            checked={settings.editorMinimap}
            onChange={(v) => update("editorMinimap", v)}
          />
        </Row>
      </SubSection>

      {/* Previews */}
      <SubSection title="File Previews">
        <Row
          label="Max preview size"
          hint="Files larger than this won't auto-preview. Higher values use more memory."
          setting="maxPreviewKib"
        >
          <NumberInput
            value={settings.maxPreviewKib}
            onChange={(v) => update("maxPreviewKib", clamp(v, 64, 16384))}
            min={64}
            max={16384}
            step={64}
            suffix="KiB"
          />
        </Row>
        <Row
          label="Parquet preview rows"
          hint="Rows fetched for the data grid."
          setting="parquetPreviewRows"
        >
          <SettingsSelect
            value={String(settings.parquetPreviewRows)}
            onChange={(v) =>
              update(
                "parquetPreviewRows",
                Number(v) as Settings["parquetPreviewRows"],
              )
            }
            options={[
              { value: "50", label: "50" },
              { value: "100", label: "100" },
              { value: "200", label: "200 (default)" },
              { value: "500", label: "500" },
            ]}
          />
        </Row>
      </SubSection>
    </Section>
  );
}

// ---- Section: Workspace (merged: Workspace + Session Persistence) ----

function WorkspaceSection() {
  const { settings, update } = useSettingsEditor();
  const sessionManager = useSession();
  const sp = settings.sessionPersistence;

  function updateSp<K extends keyof typeof sp>(key: K, value: (typeof sp)[K]) {
    update("sessionPersistence", { ...sp, [key]: value });
  }

  async function pickClone() {
    const dir = await dialogs.pickCloneDestination();
    if (dir) update("defaultCloneDir", dir);
  }

  async function clearAllSessions() {
    const ok = await mainProcess.dialogConfirm(
      "Clear all saved sessions? This removes persisted view state, scroll positions, and navigation history for every repo. Terminals are not affected.",
    );
    if (ok) {
      sessionManager.clear();
    }
  }

  return (
    <Section
      title="Workspace"
      subtitle="Repository behavior, startup preferences, and session persistence."
    >
      {/* Repos & Startup */}
      <SubSection title="Repositories">
        <Row label="Reopen repositories on startup">
          <Toggle
            checked={settings.reopenReposOnStartup}
            onChange={(v) => update("reopenReposOnStartup", v)}
          />
        </Row>
        <Row
          label="Default clone destination"
          hint="Pre-filled in the Clone dialog."
        >
          <div className="flex w-full items-center gap-2">
            <Input
              className="flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-xs"
              placeholder="(prompt every time)"
              value={settings.defaultCloneDir ?? ""}
              readOnly
            />
            <Button
              variant="outline"
              type="button"
              className="text-xs"
              onClick={pickClone}
            >
              Browse
            </Button>
            {settings.defaultCloneDir && (
              <Button
                variant="outline"
                type="button"
                className="text-xs"
                onClick={() => update("defaultCloneDir", null)}
              >
                Clear
              </Button>
            )}
          </div>
        </Row>
      </SubSection>

      {/* Session Persistence */}
      <SubSection title="Session Persistence">
        <Row
          label="Restore session on repo switch"
          hint="When enabled, switching back to a repo restores the view, file, and navigation state you left it in."
        >
          <Toggle
            checked={sp.enabled}
            onChange={(v) => updateSp("enabled", v)}
          />
        </Row>
        <Row
          label="Keep terminals alive across switches"
          hint="Hide terminal instances instead of destroying them when switching repos. Preserves scrollback and running processes."
        >
          <Toggle
            checked={sp.terminalKeepAlive}
            onChange={(v) => updateSp("terminalKeepAlive", v)}
          />
        </Row>
        <Row
          label="Restore scroll positions"
          hint="Remember and restore scroll positions in Timeline, Branches, Changes, and Explorer views."
        >
          <Toggle
            checked={sp.scrollRestore}
            onChange={(v) => updateSp("scrollRestore", v)}
          />
        </Row>
        <Row
          label="Terminal keep-alive limit"
          hint="Maximum number of repos whose terminals stay alive simultaneously. Oldest are evicted when the cap is exceeded."
        >
          <NumberInput
            value={sp.terminalKeepAliveLimit}
            onChange={(v) =>
              updateSp("terminalKeepAliveLimit", clamp(v, 1, 10))
            }
            min={1}
            max={10}
            step={1}
            suffix="repos"
          />
        </Row>
        <Row
          label="Clear all saved sessions"
          hint="Purge all persisted session data from localStorage. Does not affect terminal processes or open repos."
        >
          <Button
            variant="outline"
            type="button"
            className="text-xs"
            onClick={clearAllSessions}
          >
            <Trash2 size={12} /> Clear All Saved Sessions
          </Button>
        </Row>
      </SubSection>
    </Section>
  );
}

// ---- Section: Git & Diff (merged: Diff + Commit) --------------------

function GitAndDiff() {
  const { settings, update } = useSettingsEditor();
  return (
    <Section
      title="Git & Diff"
      subtitle="Diff display, commit helpers, and file staging behavior."
    >
      {/* Diff */}
      <SubSection title="Diff">
        <Row
          label="Prefer crab diff"
          hint="Use crab's chunk-aware diff for pointer files when available. Falls back to git diff for regular files."
          setting="preferCrabDiff"
        >
          <Toggle
            checked={settings.preferCrabDiff}
            onChange={(v) => update("preferCrabDiff", v)}
          />
        </Row>
        <Row
          label="Confirm before large discard"
          hint="Show a modal before discarding a hunk or file when the change touches more than this many lines. Set to 0 to skip the dialog entirely."
          setting="discardConfirmThreshold"
        >
          <NumberInput
            value={settings.discardConfirmThreshold}
            onChange={(v) =>
              update("discardConfirmThreshold", clamp(v, 0, 10000))
            }
            min={0}
            max={10000}
            step={1}
            suffix="lines"
          />
        </Row>
      </SubSection>

      {/* Commit */}
      <SubSection title="Commit">
        <Row
          label="Conventional commits"
          hint="Replace the plain subject input with a structured helper: type / scope / breaking-change dropdowns that compose the subject line automatically."
          setting="conventionalCommits"
        >
          <Toggle
            checked={settings.conventionalCommits}
            onChange={(v) => update("conventionalCommits", v)}
          />
        </Row>
        <Row
          label="Strip notebook outputs before commit"
          hint="When staged files include .ipynb notebooks, automatically strip cell outputs and execution counts from the staged version. The working tree file is untouched."
          setting="stripNotebookOutputs"
        >
          <Toggle
            checked={settings.stripNotebookOutputs}
            onChange={(v) => update("stripNotebookOutputs", v)}
          />
        </Row>
        <Row
          label="Large file warning threshold"
          hint="Warn when staging non-pointer files larger than this threshold. Set to 0 to disable."
          setting="largeFileThresholdMb"
        >
          <SettingsSelect
            value={String(settings.largeFileThresholdMb)}
            onChange={(v) => update("largeFileThresholdMb", Number(v))}
            options={[
              { value: "0", label: "Disabled" },
              { value: "10", label: "10 MB" },
              { value: "25", label: "25 MB" },
              { value: "50", label: "50 MB (default)" },
              { value: "100", label: "100 MB" },
              { value: "250", label: "250 MB" },
              { value: "500", label: "500 MB" },
            ]}
          />
        </Row>
      </SubSection>
    </Section>
  );
}

// ---- Section: Notifications (moved to DiagnosticsSection) -----------

const DND_DURATIONS = [
  { value: "15", label: "15 minutes" },
  { value: "30", label: "30 minutes" },
  { value: "60", label: "1 hour" },
  { value: "120", label: "2 hours" },
  { value: "indefinite", label: "Until I turn it off" },
] as const;

// ---- Section: Agents & AI (merged: Agents + Experiment Tracking) -----

type ProviderId = "none" | "mlflow" | "wandb";

interface ProviderConfig {
  label: string;
  defaultUrl: string;
  urlPlaceholder: string;
  tokenLabel: string;
  helpText: string;
}

const PROVIDER_CONFIGS: Record<Exclude<ProviderId, "none">, ProviderConfig> = {
  mlflow: {
    label: "MLflow",
    defaultUrl: "http://localhost:5000",
    urlPlaceholder: "http://localhost:5000",
    tokenLabel: "API Token",
    helpText: "Enter your MLflow tracking server URL",
  },
  wandb: {
    label: "Weights & Biases",
    defaultUrl: "https://api.wandb.ai",
    urlPlaceholder: "https://api.wandb.ai",
    tokenLabel: "API Key",
    helpText: "Enter your W&B API key from wandb.ai/authorize",
  },
};

function ExperimentTracking() {
  const activeRepo = useActiveRepo();
  const [provider, setProvider] = useState<ProviderId>("none");
  const [baseUrl, setBaseUrl] = useState("");
  const [token, setToken] = useState("");
  const [showToken, setShowToken] = useState(false);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState<{
    type: "success" | "error";
    message: string;
  } | null>(null);

  function handleProviderChange(id: string) {
    const newProvider = id as ProviderId;
    setProvider(newProvider);
    setFeedback(null);
    if (newProvider !== "none") {
      setBaseUrl(PROVIDER_CONFIGS[newProvider].defaultUrl);
      setToken("");
      setShowToken(false);
    } else {
      setBaseUrl("");
      setToken("");
      setShowToken(false);
    }
  }

  async function testConnection() {
    if (!activeRepo || provider === "none") return;
    setTesting(true);
    setFeedback(null);
    try {
      const res = await rpc.experimentsConfigure(
        activeRepo.repo_id,
        provider,
        baseUrl,
        token,
      );
      setFeedback({
        type: "success",
        message: res.message ?? "Connection successful",
      });
    } catch (e) {
      setFeedback({
        type: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setTesting(false);
    }
  }

  async function saveCredentials() {
    if (!activeRepo || provider === "none") return;
    setSaving(true);
    setFeedback(null);
    try {
      const res = await rpc.experimentsConfigure(
        activeRepo.repo_id,
        provider,
        baseUrl,
        token,
      );
      setFeedback({
        type: "success",
        message: res.message ?? "Credentials saved",
      });
    } catch (e) {
      setFeedback({
        type: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  }

  const config = provider !== "none" ? PROVIDER_CONFIGS[provider] : null;

  return (
    <SubSection title="Experiment Tracking">
      <div className="mb-2 text-[11px] text-muted-foreground">
        Connect to an experiment tracking provider to view runs alongside
        commits.
      </div>
      <Row label="Provider" hint="Select your experiment tracking service.">
        <SettingsSelect
          value={provider}
          onChange={handleProviderChange}
          options={[
            { value: "none", label: "None" },
            { value: "mlflow", label: "MLflow" },
            { value: "wandb", label: "Weights & Biases" },
          ]}
        />
      </Row>

      {config && (
        <>
          <Row label="Base URL" hint={config.helpText}>
            <Input
              type="text"
              className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary"
              placeholder={config.urlPlaceholder}
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
            />
          </Row>

          <Row
            label={config.tokenLabel}
            hint={
              provider === "wandb"
                ? "Find your key at wandb.ai/authorize"
                : "Token for authenticating with the tracking server."
            }
          >
            <div className="flex w-full items-center gap-2">
              <Input
                type={showToken ? "text" : "password"}
                className="flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary"
                placeholder={provider === "wandb" ? "W&B API key" : "API token"}
                value={token}
                onChange={(e) => setToken(e.target.value)}
              />
              <Button
                variant="outline"
                type="button"
                className="text-xs"
                onClick={() => setShowToken((v) => !v)}
                title={showToken ? "Hide token" : "Show token"}
                aria-label={showToken ? "Hide token" : "Show token"}
              >
                {showToken ? <EyeOff size={13} /> : <Eye size={13} />}
              </Button>
            </div>
          </Row>

          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              type="button"
              className="text-xs"
              onClick={testConnection}
              disabled={testing || !activeRepo || !baseUrl.trim()}
            >
              {testing ? (
                <>
                  <Loader2 size={12} className="animate-spin" /> Testing…
                </>
              ) : (
                "Test Connection"
              )}
            </Button>
            <Button
              variant="default"
              type="button"
              className="text-xs"
              onClick={saveCredentials}
              disabled={
                saving || !activeRepo || !baseUrl.trim() || !token.trim()
              }
            >
              {saving ? (
                <>
                  <Loader2 size={12} className="animate-spin" /> Saving…
                </>
              ) : (
                "Save"
              )}
            </Button>
          </div>

          {!activeRepo && (
            <div className="text-[11px] text-muted-foreground">
              Open a repository to configure experiment tracking.
            </div>
          )}

          {feedback && (
            <div
              className={clsx(
                "rounded border p-2 text-xs",
                feedback.type === "success"
                  ? "border-success/40 bg-success/10 text-success"
                  : "border-destructive/40 bg-destructive/10 text-destructive",
              )}
            >
              {feedback.message}
            </div>
          )}
        </>
      )}
    </SubSection>
  );
}

function AgentsAndAI() {
  return (
    <Section
      title="Agents & AI"
      subtitle="Manage AI agents and experiment tracking integrations."
    >
      <SubSection title="Agent Configuration">
        <AgentManagerView />
      </SubSection>
      <ExperimentTracking />
    </Section>
  );
}

// ---- Section: Forge Authentication -----------------------------------

type ForgeProvider = "github" | "gitlab" | "bitbucket" | "gitea";

interface ForgeProviderInfo {
  id: ForgeProvider;
  label: string;
  hint: string;
  supportsDeviceFlow: boolean;
  defaultClientId: string;
  defaultScopes: string;
  tokenHelpUrl: string;
  tokenHelpText: string;
}

const FORGE_PROVIDERS: ForgeProviderInfo[] = [
  {
    id: "github",
    label: "GitHub",
    hint: "github.com or GitHub Enterprise",
    supportsDeviceFlow: true,
    defaultClientId: import.meta.env.VITE_GITHUB_CLIENT_ID ?? "",
    defaultScopes: "repo read:org",
    tokenHelpUrl: "https://github.com/settings/tokens/new",
    tokenHelpText:
      'Go to GitHub → Settings → Developer settings → Personal access tokens → Generate new token. Select the "repo" scope.',
  },
  {
    id: "gitlab",
    label: "GitLab",
    hint: "gitlab.com or self-hosted",
    supportsDeviceFlow: true,
    defaultClientId: import.meta.env.VITE_GITLAB_CLIENT_ID ?? "",
    defaultScopes: "read_api read_repository",
    tokenHelpUrl: "https://gitlab.com/-/user_settings/personal_access_tokens",
    tokenHelpText:
      'Go to GitLab → Preferences → Access Tokens → Add new token. Select the "read_api" and "read_repository" scopes.',
  },
  {
    id: "bitbucket",
    label: "Bitbucket",
    hint: "bitbucket.org — supports OAuth device flow",
    supportsDeviceFlow: false,
    defaultClientId: "",
    defaultScopes: "",
    tokenHelpUrl: "https://bitbucket.org/account/settings/app-passwords/new",
    tokenHelpText:
      'Go to Bitbucket → Personal settings → App passwords → Create app password. Grant "Repositories: Read/Write" permission.',
  },
  {
    id: "gitea",
    label: "Gitea",
    hint: "Self-hosted Gitea instances (token only — no browser sign-in)",
    supportsDeviceFlow: false,
    defaultClientId: "",
    defaultScopes: "",
    tokenHelpUrl: "",
    tokenHelpText:
      "In your Gitea instance, go to Settings → Applications → Generate New Token.",
  },
];

function forgeKeychainService(provider: ForgeProvider): string {
  return `crab-forge-${provider}`;
}

const PAT_ACCOUNT = "pat-token";
const OAUTH_ACCOUNT = "oauth-token";

interface ProviderStatus {
  hasPat: boolean;
  hasOAuth: boolean;
  loading: boolean;
}

interface DeviceFlowState {
  provider: ForgeProvider;
  userCode: string;
  verificationUri: string;
  polling: boolean;
}

function ForgeAuthentication() {
  const [statuses, setStatuses] = useState<
    Record<ForgeProvider, ProviderStatus>
  >({
    github: { hasPat: false, hasOAuth: false, loading: true },
    gitlab: { hasPat: false, hasOAuth: false, loading: true },
    bitbucket: { hasPat: false, hasOAuth: false, loading: true },
    gitea: { hasPat: false, hasOAuth: false, loading: true },
  });
  const [editingProvider, setEditingProvider] = useState<ForgeProvider | null>(
    null,
  );
  const [tokenInput, setTokenInput] = useState("");
  const [showToken, setShowToken] = useState(false);
  const [saving, setSaving] = useState(false);
  const [removing, setRemoving] = useState<ForgeProvider | null>(null);
  const [feedback, setFeedback] = useState<{
    type: "success" | "error";
    message: string;
  } | null>(null);
  const [deviceFlow, setDeviceFlow] = useState<DeviceFlowState | null>(null);
  const [copied, setCopied] = useState(false);

  const loadStatuses = useCallback(async () => {
    const results: Record<string, ProviderStatus> = {};
    for (const p of FORGE_PROVIDERS) {
      try {
        const pat = await window.crab.keychain.getPassword(
          forgeKeychainService(p.id),
          PAT_ACCOUNT,
        );
        const oauth = await window.crab.keychain.getPassword(
          forgeKeychainService(p.id),
          OAUTH_ACCOUNT,
        );
        results[p.id] = {
          hasPat: pat !== null,
          hasOAuth: oauth !== null,
          loading: false,
        };
      } catch {
        results[p.id] = { hasPat: false, hasOAuth: false, loading: false };
      }
    }
    setStatuses(results as Record<ForgeProvider, ProviderStatus>);
  }, []);

  useEffect(() => {
    loadStatuses();
  }, [loadStatuses]);

  // Listen for the crab:open-settings event from CloneDialog
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail as
        | { section?: string }
        | undefined;
      if (detail?.section === "integrations") {
        // The SettingsView already navigates to "forge" section via
        // the parent — this handler is a no-op placeholder for future
        // deep-link support within the forge section.
      }
    };
    window.addEventListener("crab:open-settings", handler);
    return () => window.removeEventListener("crab:open-settings", handler);
  }, []);

  // Listen for auth completion events from the main process
  useEffect(() => {
    const unsub = window.crab.onEvent((evt) => {
      if (evt.method === "auth/completed") {
        // Token was stored by the main process — refresh statuses
        setDeviceFlow(null);
        loadStatuses();
        setFeedback({
          type: "success",
          message: "OAuth authentication successful.",
        });
      } else if (evt.method === "auth/failed") {
        // Device flow expired or was denied
        setDeviceFlow(null);
        setFeedback({
          type: "error",
          message:
            "Authorization was denied or the code expired. Please try again.",
        });
      }
    });
    return unsub;
  }, [loadStatuses]);

  async function startDeviceFlow(provider: ForgeProvider) {
    const providerInfo = FORGE_PROVIDERS.find((p) => p.id === provider);
    if (!providerInfo?.supportsDeviceFlow) return;

    if (!providerInfo.defaultClientId) {
      setFeedback({
        type: "error",
        message: `No OAuth client ID configured for ${providerInfo.label}. Set VITE_${provider.toUpperCase()}_CLIENT_ID in your .env file.`,
      });
      return;
    }

    setFeedback(null);
    setDeviceFlow({
      provider,
      userCode: "",
      verificationUri: "",
      polling: true,
    });

    try {
      const result = (await call("forge_auth_start", {
        provider,
        client_id: providerInfo.defaultClientId,
        scopes: providerInfo.defaultScopes,
      })) as { user_code: string; verification_uri: string };

      setDeviceFlow({
        provider,
        userCode: result.user_code,
        verificationUri: result.verification_uri,
        polling: true,
      });
    } catch (e) {
      setDeviceFlow(null);
      setFeedback({
        type: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }

  function cancelDeviceFlow() {
    setDeviceFlow(null);
  }

  async function copyUserCode(code: string) {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard API may fail in some contexts
    }
  }

  async function saveToken(provider: ForgeProvider) {
    if (!tokenInput.trim()) return;
    setSaving(true);
    setFeedback(null);
    try {
      await window.crab.keychain.setPassword(
        forgeKeychainService(provider),
        PAT_ACCOUNT,
        tokenInput.trim(),
      );
      setFeedback({
        type: "success",
        message: `Token saved for ${FORGE_PROVIDERS.find((p) => p.id === provider)?.label}.`,
      });
      setTokenInput("");
      setShowToken(false);
      setEditingProvider(null);
      await loadStatuses();
    } catch (e) {
      setFeedback({
        type: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  }

  async function removeToken(provider: ForgeProvider, account: string) {
    setRemoving(provider);
    setFeedback(null);
    try {
      await window.crab.keychain.deletePassword(
        forgeKeychainService(provider),
        account,
      );
      setFeedback({
        type: "success",
        message: `Credential removed for ${FORGE_PROVIDERS.find((p) => p.id === provider)?.label}.`,
      });
      await loadStatuses();
    } catch (e) {
      setFeedback({
        type: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setRemoving(null);
    }
  }

  return (
    <Section
      title="Integrations"
      subtitle="Connect to code-hosting providers for cloning private repos, viewing PRs, and managing issues. Credentials are stored securely in the OS keychain."
    >
      {feedback && (
        <div
          className={clsx(
            "rounded border p-2 text-xs",
            feedback.type === "success"
              ? "border-success/40 bg-success/10 text-success"
              : "border-destructive/40 bg-destructive/10 text-destructive",
          )}
        >
          {feedback.message}
        </div>
      )}

      {/* Device flow modal overlay */}
      {deviceFlow && deviceFlow.userCode && (
        <div className="rounded-lg border border-primary/40 bg-primary/5 p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <div className="text-sm font-semibold text-foreground">
                Authorize{" "}
                {
                  FORGE_PROVIDERS.find((p) => p.id === deviceFlow.provider)
                    ?.label
                }
              </div>
              <p className="mt-1 text-xs text-muted-foreground">
                A browser window has opened. Enter the code below to authorize
                Crab Desktop:
              </p>
            </div>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-1 text-muted-foreground hover:text-foreground"
              onClick={cancelDeviceFlow}
              aria-label="Cancel"
            >
              <X size={14} />
            </Button>
          </div>
          <div className="mt-3 flex items-center gap-3">
            <code className="rounded-md border border-border bg-background px-4 py-2 font-mono text-lg font-bold tracking-widest text-primary">
              {deviceFlow.userCode}
            </code>
            <Button
              variant="outline"
              type="button"
              className="text-xs"
              onClick={() => copyUserCode(deviceFlow.userCode)}
            >
              {copied ? (
                <>
                  <Check size={11} /> Copied
                </>
              ) : (
                <>
                  <Copy size={11} /> Copy
                </>
              )}
            </Button>
          </div>
          <div className="mt-2 flex items-center gap-2 text-[11px] text-muted-foreground">
            <Loader2 size={11} className="animate-spin" />
            Waiting for authorization…
          </div>
        </div>
      )}

      {/* Device flow initiating state (before user code is received) */}
      {deviceFlow && !deviceFlow.userCode && (
        <div className="flex items-center gap-2 rounded-lg border border-border bg-card p-3 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" />
          Starting OAuth device flow for{" "}
          {FORGE_PROVIDERS.find((p) => p.id === deviceFlow.provider)?.label}…
        </div>
      )}

      <div className="space-y-3">
        {FORGE_PROVIDERS.map((provider) => {
          const status = statuses[provider.id];
          const isEditing = editingProvider === provider.id;
          const isConnected = status.hasPat || status.hasOAuth;

          return (
            <div
              key={provider.id}
              className="rounded border border-border bg-card p-4"
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-semibold text-foreground">
                      {provider.label}
                    </span>
                    {status.loading ? (
                      <span className="inline-flex items-center gap-1 rounded-full bg-background px-2 py-0.5 text-[10px] text-muted-foreground">
                        <Loader2 size={10} className="animate-spin" /> Checking…
                      </span>
                    ) : isConnected ? (
                      <span className="inline-flex items-center rounded-full bg-success/15 px-2 py-0.5 text-[10px] font-medium text-success">
                        Connected
                        {status.hasOAuth && status.hasPat
                          ? " (OAuth + Token)"
                          : status.hasOAuth
                            ? " (OAuth)"
                            : " (Token)"}
                      </span>
                    ) : (
                      <span className="inline-flex items-center rounded-full bg-background px-2 py-0.5 text-[10px] text-muted-foreground">
                        Not configured
                      </span>
                    )}
                  </div>
                  <div className="mt-0.5 text-[11px] text-muted-foreground">
                    {provider.hint}
                  </div>
                </div>
                <div className="flex items-center gap-1.5">
                  {/* OAuth device flow button */}
                  {provider.supportsDeviceFlow &&
                    !isConnected &&
                    !isEditing && (
                      <Button
                        variant="default"
                        type="button"
                        className="text-xs"
                        onClick={() => startDeviceFlow(provider.id)}
                        disabled={deviceFlow !== null}
                      >
                        Sign in
                      </Button>
                    )}
                  {/* PAT entry button */}
                  {!isEditing && (
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs"
                      onClick={() => {
                        setEditingProvider(provider.id);
                        setTokenInput("");
                        setShowToken(false);
                        setFeedback(null);
                      }}
                    >
                      {isConnected ? "Update token" : "Use access token"}
                    </Button>
                  )}
                  {/* Remove buttons */}
                  {status.hasOAuth && !isEditing && (
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs text-destructive hover:bg-destructive/10"
                      onClick={() => removeToken(provider.id, OAUTH_ACCOUNT)}
                      disabled={removing === provider.id}
                      aria-label={`Remove OAuth token for ${provider.label}`}
                      title="Remove OAuth token"
                    >
                      {removing === provider.id ? (
                        <Loader2 size={11} className="animate-spin" />
                      ) : (
                        <Trash2 size={11} />
                      )}
                    </Button>
                  )}
                  {status.hasPat && !isEditing && (
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs text-destructive hover:bg-destructive/10"
                      onClick={() => removeToken(provider.id, PAT_ACCOUNT)}
                      disabled={removing === provider.id}
                      aria-label={`Remove access token for ${provider.label}`}
                      title="Remove access token"
                    >
                      {removing === provider.id ? (
                        <Loader2 size={11} className="animate-spin" />
                      ) : (
                        <Trash2 size={11} />
                      )}
                    </Button>
                  )}
                </div>
              </div>

              {/* Token editing form */}
              {isEditing && (
                <div className="mt-3 space-y-2 border-t border-border/60 pt-3">
                  <label className="block text-[11px] font-medium text-foreground">
                    Access Token
                  </label>
                  <p className="text-[11px] text-muted-foreground">
                    {provider.tokenHelpText}
                    {provider.tokenHelpUrl && (
                      <>
                        {" "}
                        <a
                          href={provider.tokenHelpUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                          className="text-primary underline hover:text-primary/80"
                        >
                          Open token settings ↗
                        </a>
                      </>
                    )}
                  </p>
                  <div className="flex items-center gap-2">
                    <Input
                      type={showToken ? "text" : "password"}
                      className="flex-1 rounded border border-border bg-background px-2 py-1.5 font-mono text-xs focus-visible:outline-none focus:border-primary"
                      placeholder="Paste your token here"
                      value={tokenInput}
                      onChange={(e) => setTokenInput(e.target.value)}
                      autoFocus
                    />
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs"
                      onClick={() => setShowToken((v) => !v)}
                      title={showToken ? "Hide token" : "Show token"}
                      aria-label={showToken ? "Hide token" : "Show token"}
                    >
                      {showToken ? <EyeOff size={13} /> : <Eye size={13} />}
                    </Button>
                  </div>
                  <div className="flex items-center gap-2">
                    <Button
                      variant="default"
                      type="button"
                      className="text-xs"
                      onClick={() => saveToken(provider.id)}
                      disabled={saving || !tokenInput.trim()}
                    >
                      {saving ? (
                        <>
                          <Loader2 size={11} className="animate-spin" /> Saving…
                        </>
                      ) : (
                        "Save"
                      )}
                    </Button>
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs"
                      onClick={() => {
                        setEditingProvider(null);
                        setTokenInput("");
                        setShowToken(false);
                      }}
                    >
                      Cancel
                    </Button>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </Section>
  );
}

// ---- Section: Storage & Cache (merged: Remote Storage + Cache + Memory + Prefetch) ----

function StorageAndCache() {
  const { settings, update } = useSettingsEditor();
  const { profiles, add, remove } = usePrefetchProfiles();
  const activeRepo = useActiveRepo();
  const transfers = useFileTransfers(activeRepo);
  const [newName, setNewName] = useState("");
  const [newPatterns, setNewPatterns] = useState("");
  const [runningId, setRunningId] = useState<string | null>(null);
  const [prefetchErr, setPrefetchErr] = useState<string | null>(null);
  const [prefetchResult, setPrefetchResult] = useState<string | null>(null);

  async function runProfile(profileId: string, patterns: string[]) {
    if (!transfers.hydrate) {
      setPrefetchErr(
        "Open a working repository with verified file transfers to run a prefetch profile.",
      );
      return;
    }
    setRunningId(profileId);
    setPrefetchErr(null);
    setPrefetchResult(null);
    try {
      const count = await transfers.hydrate({ kind: "patterns", patterns });
      if (count !== null)
        setPrefetchResult(
          count === 0
            ? "No matching files need hydration."
            : `Hydrated ${count} ${count === 1 ? "file" : "files"}.`,
        );
    } catch (e) {
      setPrefetchErr(e instanceof Error ? e.message : String(e));
    } finally {
      setRunningId(null);
    }
  }

  function saveNew() {
    const patterns = newPatterns
      .split(/[\n,]/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (!newName.trim() || patterns.length === 0) return;
    add(newName.trim(), patterns);
    setNewName("");
    setNewPatterns("");
  }

  return (
    <Section
      title="Storage & Cache"
      subtitle="Remote object storage, local cache, memory limits, and prefetch profiles."
    >
      {/* Remote Storage */}
      <SubSection title="Remote Storage">
        <StorageConfigSection />
      </SubSection>

      {/* Local Cache */}
      <SubSection title="Local Cache">
        <CacheSettings
          key={activeRepo?.repo_id ?? "local"}
          repoId={activeRepo?.repo_id}
          label={activeRepo?.label}
        />
      </SubSection>

      {/* Memory */}
      <SubSection title="Memory">
        <Row
          label="Heap cap"
          hint="Maximum renderer JS heap usage (in MB) before tab state eviction kicks in. Requires performance.memory (Chromium / Electron)."
        >
          <NumberInput
            value={settings.heapCapMb}
            onChange={(v) => update("heapCapMb", clamp(v, 128, 8192))}
            min={128}
            max={8192}
            step={64}
            suffix="MB"
          />
        </Row>
      </SubSection>

      {/* Prefetch Profiles */}
      <SubSection title="Prefetch Profiles">
        <div className="mb-2 text-[11px] text-muted-foreground">
          Named hydrate patterns you can run with one click. Great for
          &ldquo;training set&rdquo;, &ldquo;eval split&rdquo;, or
          &ldquo;reviewer-subset&rdquo; workflows.
        </div>
        <div className="space-y-3 rounded border border-border bg-card p-4">
          <div className="text-xs text-muted-foreground">
            Create a new profile
          </div>
          <Input
            className="w-full rounded border border-border bg-background px-2 py-1 text-sm focus-visible:outline-none focus:border-primary"
            placeholder="Name (e.g. training-subset)"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
          />
          <Textarea
            className="h-20 w-full rounded border border-border bg-background p-2 font-mono text-xs focus-visible:outline-none focus:border-primary"
            placeholder={`data/train/**/*.parquet\nmodels/llama-7b/*.safetensors`}
            value={newPatterns}
            onChange={(e) => setNewPatterns(e.target.value)}
          />
          <Button
            variant="default"
            type="button"
            className="text-xs"
            onClick={saveNew}
            disabled={!newName.trim() || !newPatterns.trim()}
          >
            Save profile
          </Button>
        </div>

        {prefetchErr && (
          <div className="rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {prefetchErr}
          </div>
        )}
        {prefetchResult && (
          <div role="status" className="text-xs text-muted-foreground">
            {prefetchResult}
          </div>
        )}

        {profiles.length === 0 ? (
          <div className="text-xs text-muted-foreground">
            No profiles saved yet.
          </div>
        ) : (
          <ul className="space-y-2">
            {profiles.map((p) => (
              <li
                key={p.id}
                className="rounded border border-border bg-card p-3"
              >
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0">
                    <div className="truncate text-sm font-semibold">
                      {p.name}
                    </div>
                    <div className="truncate font-mono text-[11px] text-muted-foreground">
                      {p.patterns.join(", ")}
                    </div>
                  </div>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs"
                      onClick={() => runProfile(p.id, p.patterns)}
                      disabled={runningId !== null || !transfers.hydrate}
                      title={
                        transfers.hydrate
                          ? "Hydrate using this profile"
                          : "Verified file transfers are unavailable"
                      }
                    >
                      {runningId === p.id ? (
                        <>
                          <Loader2 size={11} className="animate-spin" /> Running
                        </>
                      ) : (
                        <>
                          <Play size={11} /> Run
                        </>
                      )}
                    </Button>
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs"
                      onClick={() => remove(p.id)}
                      title="Delete profile"
                    >
                      <Trash2 size={11} />
                    </Button>
                  </div>
                </div>
              </li>
            ))}
          </ul>
        )}
      </SubSection>
    </Section>
  );
}

// ---- Section: Diagnostics (merged: Logs + PTY Daemon + Telemetry + Notifications) ----

function DiagnosticsSection() {
  const { settings, update } = useSettingsEditor();
  const notif = settings.notifications;
  const [showPreview, setShowPreview] = useState(false);
  const [noTelemetryFlag, setNoTelemetryFlag] = useState(false);

  function updateNotif<K extends keyof typeof notif>(
    key: K,
    value: (typeof notif)[K],
  ) {
    update("notifications", { ...notif, [key]: value });
  }

  function handleDndToggle(enabled: boolean) {
    if (enabled) {
      update("notifications", {
        ...notif,
        doNotDisturb: true,
        doNotDisturbUntil: null,
      });
    } else {
      update("notifications", {
        ...notif,
        doNotDisturb: false,
        doNotDisturbUntil: null,
      });
    }
  }

  function handleDndDuration(value: string) {
    if (value === "indefinite") {
      update("notifications", {
        ...notif,
        doNotDisturb: true,
        doNotDisturbUntil: null,
      });
    } else {
      const minutes = Number(value);
      const until = Date.now() + minutes * 60 * 1000;
      update("notifications", {
        ...notif,
        doNotDisturb: true,
        doNotDisturbUntil: until,
      });
    }
  }

  function currentDndValue(): string {
    if (!notif.doNotDisturb) return "indefinite";
    if (notif.doNotDisturbUntil === null) return "indefinite";
    const remaining = notif.doNotDisturbUntil - Date.now();
    if (remaining <= 0) return "indefinite";
    const mins = Math.round(remaining / 60000);
    if (mins <= 15) return "15";
    if (mins <= 30) return "30";
    if (mins <= 60) return "60";
    return "120";
  }

  useEffect(() => {
    if (typeof window !== "undefined" && window.crab?.getNoTelemetryFlag) {
      window.crab
        .getNoTelemetryFlag()
        .then(setNoTelemetryFlag)
        .catch(() => undefined);
    }
  }, []);

  useEffect(() => {
    telemetry.setNoTelemetryFlag(noTelemetryFlag);
    telemetry.setOptIn(settings.telemetryOptIn);
  }, [settings.telemetryOptIn, noTelemetryFlag]);

  const bufferContents = showPreview ? telemetry.getBuffer() : [];

  return (
    <Section
      title="Diagnostics"
      subtitle="Logging, notifications, telemetry, and system health."
    >
      <HealthView />
      {/* Notifications */}
      <SubSection title="Notifications">
        <Row
          label="Mute job completion notifications"
          hint="Suppress toasts when background jobs (push, fetch, hydrate) finish."
          setting="notifications"
        >
          <Toggle
            checked={notif.muteJobCompletion}
            onChange={(v) => updateNotif("muteJobCompletion", v)}
          />
        </Row>
        <Row
          label="Mute PR update notifications"
          hint="Suppress toasts for pull request activity (comments, reviews, status changes)."
          setting="notifications"
        >
          <Toggle
            checked={notif.mutePrUpdates}
            onChange={(v) => updateNotif("mutePrUpdates", v)}
          />
        </Row>
        <Row
          label="Mute agent error notifications"
          hint="Suppress toasts for agent errors and unexpected crashes."
          setting="notifications"
        >
          <Toggle
            checked={notif.muteAgentErrors}
            onChange={(v) => updateNotif("muteAgentErrors", v)}
          />
        </Row>
        <Row
          label="Do not disturb"
          hint="Silence all notifications. When enabled, no toasts will appear regardless of per-type settings."
          setting="notifications"
        >
          <Toggle checked={notif.doNotDisturb} onChange={handleDndToggle} />
        </Row>
        {notif.doNotDisturb && (
          <Row
            label="DND duration"
            hint="How long to keep Do Not Disturb active. After the duration expires, notifications resume automatically."
          >
            <SettingsSelect
              value={currentDndValue()}
              onChange={handleDndDuration}
              options={DND_DURATIONS.map((d) => ({
                value: d.value,
                label: d.label,
              }))}
            />
          </Row>
        )}
      </SubSection>

      {/* Logs */}
      <SubSection title="Logs">
        <Row
          label="Activity buffer size"
          hint="How many progress lines to keep in the Activity drawer."
        >
          <SettingsSelect
            value={String(settings.maxLogLines)}
            onChange={(v) =>
              update("maxLogLines", Number(v) as Settings["maxLogLines"])
            }
            options={[
              { value: "1000", label: "1,000" },
              { value: "2000", label: "2,000 (default)" },
              { value: "5000", label: "5,000" },
              { value: "10000", label: "10,000" },
            ]}
          />
        </Row>
        <Row
          label="Agent log level"
          hint="Applies after restarting Crab Desktop. Logs go to the launcher terminal."
        >
          <SettingsSelect
            value={settings.agentLogLevel}
            onChange={(v) =>
              update("agentLogLevel", v as Settings["agentLogLevel"])
            }
            options={[
              { value: "off", label: "Off" },
              { value: "info", label: "Info (default)" },
              { value: "debug", label: "Debug" },
              { value: "trace", label: "Trace (verbose)" },
            ]}
          />
        </Row>
        <Row
          label="Open DevTools on launch"
          hint="Useful for debugging; leaves a DevTools window open every launch."
        >
          <Toggle
            checked={settings.openDevtoolsOnLaunch}
            onChange={(v) => update("openDevtoolsOnLaunch", v)}
          />
        </Row>
      </SubSection>

      {/* PTY Daemon */}
      <SubSection title="PTY Daemon">
        <DiagnosticsView />
      </SubSection>

      {/* Telemetry */}
      <SubSection title="Telemetry">
        <div className="mb-2 text-[11px] text-muted-foreground">
          Anonymous usage data helps improve Crab Desktop. No personally
          identifiable information is collected. Repo URLs are hashed to a
          non-reversible token.
        </div>
        {noTelemetryFlag && (
          <div className="rounded border border-warning/40 bg-warning/10 p-3 text-xs text-warning">
            Telemetry is disabled by the{" "}
            <code className="font-mono">--no-telemetry</code> launch flag. This
            overrides the setting below.
          </div>
        )}
        <Row
          label="Send anonymous telemetry"
          hint="When enabled, anonymous usage events are batched locally and flushed to Crab servers every 5 minutes. Default: off."
        >
          <Toggle
            checked={settings.telemetryOptIn}
            onChange={(v) => update("telemetryOptIn", v)}
          />
        </Row>
        <Row
          label="Preview telemetry payload"
          hint="See exactly what would be sent. The buffer shows events collected since the last flush."
        >
          <Button
            variant="outline"
            type="button"
            className="text-xs"
            onClick={() => setShowPreview((v) => !v)}
          >
            {showPreview ? "Hide preview" : "Show preview"}
          </Button>
        </Row>
        {showPreview && (
          <div className="rounded border border-border bg-background p-3">
            {bufferContents.length === 0 ? (
              <div className="text-xs text-muted-foreground">
                No events in the buffer. Events are collected when telemetry is
                enabled and you use the app.
              </div>
            ) : (
              <pre className="max-h-64 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-foreground">
                {bufferContents
                  .map((e) => JSON.stringify(e, null, 2))
                  .join("\n")}
              </pre>
            )}
          </div>
        )}
      </SubSection>
    </Section>
  );
}

// ---- Section: General (merged: Shortcuts + Reset + About) ------------

interface AboutData {
  agent_version: string;
  crab_cli_version: string | null;
  os: string;
  arch: string;
}

function GeneralSection() {
  const { update, reset } = useSettingsEditor();
  const [data, setData] = useState<AboutData | null>(null);
  const [aboutErr, setAboutErr] = useState<string | null>(null);

  useEffect(() => {
    rpc
      .about()
      .then(setData)
      .catch((e: Error) => setAboutErr(e.message));
  }, []);

  const desktopVersion = "0.1.0";

  async function resetAll() {
    const ok = await mainProcess.dialogConfirm(
      "Reset all Crab Desktop settings to defaults? Open repositories won't be affected.",
    );
    if (ok) {
      reset();
    }
  }

  const shortcutRows: Array<[string, string]> = [
    ["Toggle Repositories sidebar", "⇧⌘B / Ctrl+Shift+B"],
    ["Toggle Explorer sidebar", "⌘B / Ctrl+B"],
    ["Quick open file", "⌘P / Ctrl+P"],
    ["Close tab", "(hover tab → ✕)"],
  ];

  return (
    <Section title="General" subtitle="About, keyboard shortcuts, and reset.">
      {/* About */}
      <SubSection title="About Crab Desktop">
        <div className="rounded border border-border bg-card p-5">
          <div className="mb-3 text-lg font-semibold">Crab Desktop</div>
          <div className="grid grid-cols-2 gap-3 text-sm">
            <KV label="Desktop version" value={desktopVersion} />
            <KV label="Agent version" value={data?.agent_version ?? "…"} />
            <KV
              label="Crab CLI"
              value={
                data?.crab_cli_version ??
                (aboutErr ? "unavailable" : data ? "not installed" : "…")
              }
            />
            <KV
              label="Platform"
              value={data ? `${data.os}/${data.arch}` : "…"}
            />
          </div>
          {aboutErr && (
            <div className="mt-3 rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
              {aboutErr}
            </div>
          )}
        </div>
        <Row
          label="Show welcome tour"
          hint="Reopen the first-run welcome tour."
        >
          <Button
            variant="outline"
            type="button"
            className="text-xs"
            onClick={() => update("welcomeSeen", false)}
            aria-label="Show welcome tour"
          >
            <Play size={12} /> Reopen tour
          </Button>
        </Row>
        <div className="mt-2 text-xs text-muted-foreground">
          Crab stores large files in cloud object storage and works alongside
          standard git.
        </div>
      </SubSection>

      {/* Keyboard Shortcuts */}
      <SubSection title="Keyboard Shortcuts">
        <div className="mb-2 text-[11px] text-muted-foreground">
          Shortcuts are not yet customizable.
        </div>
        <div className="overflow-hidden rounded border border-border">
          <table className="w-full text-sm">
            <tbody>
              {shortcutRows.map(([action, keys], i) => (
                <tr
                  key={action}
                  className={
                    i < shortcutRows.length - 1
                      ? "border-b border-border/60"
                      : undefined
                  }
                >
                  <td className="px-3 py-2 text-foreground">{action}</td>
                  <td className="px-3 py-2 text-right">
                    <span className="kbd">{keys}</span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </SubSection>

      {/* Reset */}
      <SubSection title="Reset">
        <Row
          label="Reset all settings"
          hint="Returns every toggle on this page to its default."
        >
          <Button
            variant="outline"
            type="button"
            className="text-xs"
            onClick={resetAll}
          >
            <RotateCcw size={12} /> Reset
          </Button>
        </Row>
      </SubSection>
    </Section>
  );
}

// ---- Section: SSH Remote -----------------------------------------------

function SshSettings() {
  const { settings, update } = useSettingsEditor();
  const features = settings.features;
  const [knownHosts, setKnownHosts] = useState<
    Array<{
      hostname: string;
      port: number;
      fingerprint: string;
      keyType: string;
      addedAt: number;
    }>
  >([]);
  const [hostError, setHostError] = useState<string | null>(null);
  const [stats, setStats] = useState<{
    successRate: number;
    avgLatencyMs: number;
    reconnections: number;
  } | null>(null);

  useEffect(() => {
    // Load known hosts
    (async () => {
      try {
        const hosts = await window.crab.ssh.listKnownHosts();
        setKnownHosts(hosts);
      } catch (error: unknown) {
        setHostError(error instanceof Error ? error.message : String(error));
      }
    })();
    // Load diagnostics stats
    (async () => {
      try {
        const s = await (window as any).crab?.ssh?.getConnectionStats?.();
        if (s) setStats(s);
      } catch {
        // SSH not available
      }
    })();
  }, []);

  function updateFeature(key: keyof typeof features, value: boolean) {
    update("features", { ...features, [key]: value });
  }

  async function removeKnownHost(
    hostname: string,
    port: number,
    keyType: string,
  ) {
    try {
      await window.crab.ssh.removeKnownHost(hostname, port, keyType);
      setKnownHosts(await window.crab.ssh.listKnownHosts());
      setHostError(null);
    } catch (error: unknown) {
      setHostError(error instanceof Error ? error.message : String(error));
    }
  }

  return (
    <Section
      title="SSH Remote"
      subtitle="Configure SSH remote connection defaults and manage known hosts."
    >
      {/* Feature flag */}
      <Row
        label="Enable SSH Remote Connections"
        hint="When disabled, the SSH view and related menu items are hidden."
      >
        <Toggle
          checked={features.sshRemoteEnabled}
          onChange={(v) => updateFeature("sshRemoteEnabled", v)}
        />
      </Row>

      {features.sshRemoteEnabled && (
        <>
          {/* Global defaults */}
          <div className="mt-4 border-t border-border pt-4">
            <h3 className="mb-3 text-xs font-semibold text-foreground">
              Global Defaults
            </h3>
            <Row
              label="Keep-alive interval"
              hint="Seconds between SSH keep-alive packets."
            >
              <NumberInput
                value={15}
                onChange={() => {}}
                min={5}
                max={300}
                step={5}
                suffix="sec"
              />
            </Row>
            <Row
              label="Auto-reconnect"
              hint="Automatically reconnect on connection loss."
            >
              <Toggle checked={true} onChange={() => {}} />
            </Row>
            <Row
              label="Auto-deploy agent"
              hint="Automatically deploy the remote agent on first connection."
            >
              <Toggle checked={true} onChange={() => {}} />
            </Row>
            <Row
              label="Enable compression"
              hint="Use zlib compression for SSH connections."
            >
              <Toggle checked={false} onChange={() => {}} />
            </Row>
            <Row
              label="Idle timeout"
              hint="Disconnect after this many minutes of inactivity. 0 = never."
            >
              <NumberInput
                value={0}
                onChange={() => {}}
                min={0}
                max={1440}
                step={5}
                suffix="min"
              />
            </Row>
          </div>

          {/* Known Hosts */}
          <div className="mt-4 border-t border-border pt-4">
            <h3 className="mb-3 text-xs font-semibold text-foreground">
              Saved host keys
            </h3>
            <p className="mb-2 text-xs text-muted-foreground">
              Keys approved in Crab. OpenSSH user and system trust records are
              also checked when connecting.
            </p>
            {hostError && (
              <p role="alert" className="mb-2 text-xs text-destructive">
                {hostError}
              </p>
            )}
            {knownHosts.length === 0 ? (
              <p className="text-xs text-muted-foreground">
                No host keys approved in Crab yet.
              </p>
            ) : (
              <ul className="space-y-2">
                {knownHosts.map((host) => (
                  <li
                    key={`${host.hostname}:${host.port}:${host.keyType}`}
                    className="flex items-center justify-between rounded border border-border bg-card p-2"
                  >
                    <div className="min-w-0">
                      <div className="truncate text-xs font-medium text-foreground">
                        {host.hostname}:{host.port}
                      </div>
                      <div className="truncate font-mono text-[10px] text-muted-foreground">
                        {host.keyType} {host.fingerprint.slice(0, 32)}...
                      </div>
                      <div className="text-[10px] text-muted-foreground">
                        Added {new Date(host.addedAt).toLocaleDateString()}
                      </div>
                    </div>
                    <Button
                      variant="outline"
                      type="button"
                      className="text-xs text-destructive hover:text-destructive"
                      onClick={() =>
                        removeKnownHost(host.hostname, host.port, host.keyType)
                      }
                      aria-label={`Remove ${host.hostname} from known hosts`}
                    >
                      <Trash2 size={12} />
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </div>

          {/* Diagnostics */}
          <div className="mt-4 border-t border-border pt-4">
            <h3 className="mb-3 text-xs font-semibold text-foreground">
              Diagnostics
            </h3>
            {stats ? (
              <div className="grid grid-cols-3 gap-4">
                <div className="rounded border border-border bg-card p-3 text-center">
                  <div className="text-lg font-bold text-foreground">
                    {stats.successRate}%
                  </div>
                  <div className="text-[10px] text-muted-foreground">
                    Success Rate
                  </div>
                </div>
                <div className="rounded border border-border bg-card p-3 text-center">
                  <div className="text-lg font-bold text-foreground">
                    {stats.avgLatencyMs}ms
                  </div>
                  <div className="text-[10px] text-muted-foreground">
                    Avg Latency
                  </div>
                </div>
                <div className="rounded border border-border bg-card p-3 text-center">
                  <div className="text-lg font-bold text-foreground">
                    {stats.reconnections}
                  </div>
                  <div className="text-[10px] text-muted-foreground">
                    Reconnections
                  </div>
                </div>
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">
                No connection statistics available yet. Stats are collected
                after your first SSH connection.
              </p>
            )}
          </div>
        </>
      )}
    </Section>
  );
}

// ---- Building blocks -------------------------------------------------

function Section({
  title,
  subtitle,
  children,
}: {
  title: string;
  subtitle?: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h2 className="mb-1 text-lg font-semibold text-foreground">{title}</h2>
      {subtitle && (
        <div className="mb-4 text-xs text-muted-foreground">{subtitle}</div>
      )}
      <div className="space-y-5">{children}</div>
    </section>
  );
}

function SubSection({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mt-6 border-t border-border pt-5 first:mt-0 first:border-t-0 first:pt-0">
      <h3 className="mb-3 text-sm font-semibold text-foreground">{title}</h3>
      <div className="space-y-4">{children}</div>
    </div>
  );
}

const SettingsControlLabel = createContext<string | undefined>(undefined);

function Row({
  label,
  hint,
  setting,
  children,
}: {
  label: string;
  hint?: string;
  setting?: keyof Settings;
  children: React.ReactNode;
}) {
  const scope = useContext(SettingsEditorContext),
    workspace = useWorkspaceSettings();
  const overridden =
    setting &&
    workspace.workspaceSettings &&
    Object.hasOwn(workspace.workspaceSettings, setting);
  return (
    <fieldset
      aria-label={label}
      disabled={
        scope === "workspace" && (!setting || !isWorkspaceSetting(setting))
      }
      className="grid grid-cols-[1fr_auto] items-start gap-6 border-b border-border/60 pb-4 last:border-b-0"
    >
      <div>
        <div className="flex items-center text-sm text-foreground">
          {label}
          {overridden && <ScopeChip />}
          {scope === "workspace" && setting && overridden && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="ml-2 text-xs text-primary"
              aria-label={`Use User value for ${label}`}
              onClick={() => void workspace.clearWorkspaceSetting(setting)}
            >
              Use User value
            </Button>
          )}
        </div>
        {hint && (
          <div className="mt-1 max-w-md text-[11.5px] text-muted-foreground">
            {hint}
          </div>
        )}
      </div>
      <div className="min-w-[200px]"><SettingsControlLabel.Provider value={label}>{children}</SettingsControlLabel.Provider></div>
    </fieldset>
  );
}

function Toggle({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  const controlLabel = useContext(SettingsControlLabel);
  return <Switch aria-label={controlLabel} checked={checked} onCheckedChange={onChange} />;
}

function SettingsSelect({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  options: Array<{ value: string; label: string; disabled?: boolean }>;
}) {
  const controlLabel = useContext(SettingsControlLabel);
  return (
    <Select
      value={":" + String(value)}
      onValueChange={(selectedValue) => {
        const value = selectedValue.slice(1);
        return onChange(value);
      }}
    >
      <SelectTrigger aria-label={controlLabel} className="rounded border border-border bg-background px-2 py-1 text-sm focus-visible:outline-none focus:border-primary">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectGroup>
          {options.map((o) => (
            <SelectItem
              key={o.value}
              disabled={o.disabled}
              value={":" + String(o.value)}
            >
              {o.label}
            </SelectItem>
          ))}
        </SelectGroup>
      </SelectContent>
    </Select>
  );
}

function NumberInput({
  value,
  onChange,
  min,
  max,
  step,
  suffix,
}: {
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  step?: number;
  suffix?: string;
}) {
  const controlLabel = useContext(SettingsControlLabel);
  return (
    <div className="flex items-center gap-2">
      <Input aria-label={controlLabel}
        type="number"
        value={value}
        min={min}
        max={max}
        step={step}
        onChange={(e) => {
          const n = Number(e.target.value);
          if (!Number.isFinite(n)) return;
          onChange(n);
        }}
        className="w-24 rounded border border-border bg-background px-2 py-1 text-sm font-mono focus-visible:outline-none focus:border-primary"
      />
      {suffix && (
        <span className="text-xs text-muted-foreground">{suffix}</span>
      )}
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="mt-0.5 font-mono text-sm text-foreground">{value}</div>
    </div>
  );
}

function clamp(n: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, n));
}

// Re-export defaults for any external reset path. (Not strictly
// needed yet but keeps the public surface future-proof.)
export { DEFAULT_SETTINGS };
