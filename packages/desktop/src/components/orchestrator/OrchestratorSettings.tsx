import { NativeSelect } from "@/components/ui/native-select";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  Coins,
  FileText,
  FolderOpen,
  Loader2,
  Play,
  Server,
  Settings,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { useSettings } from "../../settings";
import { TemplatesSection } from "./TemplatesSection";
import { CommandsSection } from "./CommandsSection";

// ── Types ───────────────────────────────────────────────────────────

interface RegisteredAgent {
  type: string;
  name: string;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Simplified orchestrator workspace settings — rendered inside the
 * unified Agents page as a collapsible "Task Settings" section.
 *
 * Organized into focused collapsible groups:
 *   1. General — enable, default agent, concurrency, API key passthrough
 *   2. Worktree — base dir, retention
 *   3. Scripts — setup, run, archive, port range
 *   4. Preferences — prompt injection for agents
 *   5. Extensions — MCP servers, templates, commands
 *   6. Usage — cost/token tracking
 *
 * Per-agent configuration (binary path, env vars, etc.) is handled by
 * the AgentConfigurePanel in AgentManagerView — not here.
 */
export function OrchestratorSettings() {
  const { settings, update } = useSettings();
  const [registeredAgents, setRegisteredAgents] = useState<RegisteredAgent[]>([]);

  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.listRegisteredAgents()
      .then((result: { success?: boolean; agents?: RegisteredAgent[] }) => {
        if (result?.agents) setRegisteredAgents(result.agents);
      })
      .catch(() => {});
  }, []);

  return (
    <div className="space-y-3">
      {/* ── General ──────────────────────────────────────────────── */}
      <CollapsibleGroup title="General" icon={Settings} defaultOpen>
        <SettingsRow
          label="Enable orchestrator"
          hint="Show the Tasks panel in the activity bar for delegating work to AI agents."
        >
          <Toggle
            checked={settings.orchestratorEnabled}
            onChange={(v) => update("orchestratorEnabled", v)}
          />
        </SettingsRow>

        <SettingsRow
          label="Default agent"
          hint="Which agent CLI to use for new tasks."
        >
          <NativeSelect
            className="rounded border border-border bg-background px-2 py-1 text-sm focus-visible:outline-none focus:border-primary"
            value={settings.orchestratorDefaultAgent}
            onChange={(e) => update("orchestratorDefaultAgent", e.target.value)}
          >
            {registeredAgents.length > 0 ? (
              registeredAgents.map(agent => (
                <option key={agent.type} value={agent.type}>{agent.name}</option>
              ))
            ) : (
              <>
                <option value="claude-code">Claude Code</option>
                <option value="codex">Codex</option>
              </>
            )}
          </NativeSelect>
        </SettingsRow>

        <SettingsRow
          label="Max concurrent tasks"
          hint="Limit simultaneous agent runs to avoid resource exhaustion."
        >
          <NumberInput
            value={settings.orchestratorMaxConcurrent}
            min={1}
            max={10}
            onChange={(n) => update("orchestratorMaxConcurrent", n)}
            suffix="tasks"
          />
        </SettingsRow>

        <SettingsRow
          label="API key passthrough"
          hint="Pass your configured AI provider API key to agent CLIs via environment variable."
        >
          <Toggle
            checked={settings.orchestratorApiKeyPassthrough}
            onChange={(v) => update("orchestratorApiKeyPassthrough", v)}
          />
        </SettingsRow>
      </CollapsibleGroup>

      {/* ── Worktree ─────────────────────────────────────────────── */}
      <CollapsibleGroup title="Worktree" icon={FolderOpen}>
        <SettingsRow
          label="Base directory"
          hint="Where agent worktrees are created. Leave empty for system temp."
        >
          <Input
            className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary"
            placeholder="(system temp)"
            value={settings.orchestratorWorktreeBaseDir}
            onChange={(e) => update("orchestratorWorktreeBaseDir", e.target.value)}
          />
        </SettingsRow>

        <SettingsRow
          label="Auto-prune retention"
          hint="Completed worktrees are removed after this many days."
        >
          <NumberInput
            value={settings.orchestratorRetentionDays}
            min={1}
            max={90}
            onChange={(n) => update("orchestratorRetentionDays", n)}
            suffix="days"
          />
        </SettingsRow>
      </CollapsibleGroup>

      {/* ── Scripts ──────────────────────────────────────────────── */}
      <CollapsibleGroup title="Scripts" icon={Play}>
        <div className="mb-2 text-[10px] text-muted-foreground">
          Lifecycle scripts run in the workspace directory. Override per-repo via{" "}
          <code className="font-mono text-foreground/80">crab-orchestrator.json</code>.
        </div>

        <SettingsRow label="Setup script" hint="Runs before the first agent turn in a new or restored worktree (e.g. npm install).">
          <Textarea
            className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary resize-y min-h-[48px]"
            placeholder="npm install"
            value={settings.orchestratorSetupScript}
            onChange={(e) => update("orchestratorSetupScript", e.target.value)}
            rows={2}
          />
        </SettingsRow>

        <SettingsRow label="Run script" hint="Runs when you click 'Run' in the workspace.">
          <Textarea
            className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary resize-y min-h-[48px]"
            placeholder="npm run dev"
            value={settings.orchestratorRunScript}
            onChange={(e) => update("orchestratorRunScript", e.target.value)}
            rows={2}
          />
        </SettingsRow>

        <SettingsRow label="Archive script" hint="Runs before worktree removal.">
          <Textarea
            className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary resize-y min-h-[48px]"
            placeholder="npm run clean"
            value={settings.orchestratorArchiveScript}
            onChange={(e) => update("orchestratorArchiveScript", e.target.value)}
            rows={2}
          />
        </SettingsRow>

        <SettingsRow
          label="Nonconcurrent run"
          hint="Only one workspace can have an active run process at a time."
        >
          <Toggle
            checked={settings.orchestratorNonconcurrentRun}
            onChange={(v) => update("orchestratorNonconcurrentRun", v)}
          />
        </SettingsRow>

        <SettingsRow label="Port range" hint="Auto-assign unique ports per workspace.">
          <div className="flex items-center gap-2">
            <Input
              type="number"
              value={settings.orchestratorPortRangeStart}
              min={1024}
              max={65535}
              onChange={(e) => {
                const n = Number(e.target.value);
                if (Number.isFinite(n) && n >= 1024 && n <= 65535) {
                  update("orchestratorPortRangeStart", n);
                }
              }}
              className="w-20 rounded border border-border bg-background px-2 py-1 text-sm font-mono focus-visible:outline-none focus:border-primary"
            />
            <span className="text-xs text-muted-foreground">–</span>
            <Input
              type="number"
              value={settings.orchestratorPortRangeEnd}
              min={1024}
              max={65535}
              onChange={(e) => {
                const n = Number(e.target.value);
                if (Number.isFinite(n) && n >= 1024 && n <= 65535) {
                  update("orchestratorPortRangeEnd", n);
                }
              }}
              className="w-20 rounded border border-border bg-background px-2 py-1 text-sm font-mono focus-visible:outline-none focus:border-primary"
            />
          </div>
        </SettingsRow>
      </CollapsibleGroup>

      {/* ── Preferences ──────────────────────────────────────────── */}
      <CollapsibleGroup title="Preferences" icon={FileText}>
        <div className="mb-2 text-[10px] text-muted-foreground">
          Injected into agent prompts. Override per-repo via{" "}
          <code className="font-mono text-foreground/80">.crab/orchestrator/preferences.md</code>.
        </div>
        <PreferencesSection />
      </CollapsibleGroup>

      {/* ── Extensions ───────────────────────────────────────────── */}
      <CollapsibleGroup title="Extensions" icon={Server}>
        <div className="space-y-4">
          <div>
            <h4 className="mb-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
              MCP Servers
            </h4>
            <MCPServersSection />
          </div>

          <div className="border-t border-border/40 pt-3">
            <h4 className="mb-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
              Templates
            </h4>
            <TemplatesSection />
          </div>

          <div className="border-t border-border/40 pt-3">
            <h4 className="mb-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
              Slash Commands
            </h4>
            <CommandsSection />
          </div>
        </div>
      </CollapsibleGroup>

      {/* ── Usage ────────────────────────────────────────────────── */}
      <CollapsibleGroup title="Usage" icon={Coins}>
        <UsageSection />
      </CollapsibleGroup>
    </div>
  );
}

// ── Collapsible Group ───────────────────────────────────────────────

function CollapsibleGroup({
  title,
  icon: Icon,
  defaultOpen = false,
  children,
}: {
  title: string;
  icon: LucideIcon;
  defaultOpen?: boolean;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);

  return (
    <div className="rounded-lg border border-border/60 bg-card/50">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex w-full items-center gap-2 px-4 py-3 text-left"
      >
        <Icon size={13} className="text-muted-foreground" />
        <span className="flex-1 text-xs font-medium text-foreground">{title}</span>
        <ChevronDown
          size={13}
          className={clsx(
            "text-muted-foreground transition-transform",
            !open && "-rotate-90",
          )}
        />
      </button>
      {open && (
        <div className="space-y-4 border-t border-border/40 px-4 pb-4 pt-3">
          {children}
        </div>
      )}
    </div>
  );
}

// ── Building Blocks ─────────────────────────────────────────────────

function SettingsRow({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="grid grid-cols-[1fr_auto] items-start gap-4 pb-3 last:pb-0">
      <div>
        <div className="text-xs text-foreground">{label}</div>
        {hint && <div className="mt-0.5 max-w-sm text-[10px] text-muted-foreground">{hint}</div>}
      </div>
      <div className="min-w-[180px]">{children}</div>
    </div>
  );
}

function Toggle({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className={clsx(
        "relative inline-flex h-5 w-9 items-center rounded-full border transition",
        checked
          ? "border-primary bg-primary/70"
          : "border-border bg-background",
      )}
    >
      <span
        className={clsx(
          "inline-block h-3.5 w-3.5 transform rounded-full bg-background shadow transition",
          checked ? "translate-x-[18px]" : "translate-x-[2px]",
        )}
      />
    </button>
  );
}

function NumberInput({
  value,
  min,
  max,
  onChange,
  suffix,
}: {
  value: number;
  min: number;
  max: number;
  onChange: (v: number) => void;
  suffix?: string;
}) {
  return (
    <div className="flex items-center gap-2">
      <Input
        type="number"
        value={value}
        min={min}
        max={max}
        onChange={(e) => {
          const n = Number(e.target.value);
          if (Number.isFinite(n) && n >= min && n <= max) onChange(n);
        }}
        className="w-20 rounded border border-border bg-background px-2 py-1 text-sm font-mono focus-visible:outline-none focus:border-primary"
      />
      {suffix && <span className="text-[10px] text-muted-foreground">{suffix}</span>}
    </div>
  );
}

// ── Preferences Section ─────────────────────────────────────────────

function PreferencesSection() {
  const [prefs, setPrefs] = useState({
    general: "",
    codeReview: "",
    pr: "",
    conflictResolution: "",
  });
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.getPreferences?.("")
      .then((result: any) => {
        if (result?.success && result.preferences) setPrefs(result.preferences);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  const handleSave = useCallback(() => {
    (window as any).crab?.orchestrator?.savePreferences?.("", prefs).catch(() => {});
  }, [prefs]);

  if (!loaded) {
    return (
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Loader2 size={11} className="animate-spin" /> Loading…
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <PrefField
        label="General"
        placeholder="e.g., Always use TypeScript strict mode…"
        value={prefs.general}
        onChange={(v) => setPrefs((p) => ({ ...p, general: v }))}
        onBlur={handleSave}
      />
      <PrefField
        label="Code review"
        placeholder="e.g., Focus on security and performance…"
        value={prefs.codeReview}
        onChange={(v) => setPrefs((p) => ({ ...p, codeReview: v }))}
        onBlur={handleSave}
      />
      <PrefField
        label="PR descriptions"
        placeholder="e.g., Include testing instructions…"
        value={prefs.pr}
        onChange={(v) => setPrefs((p) => ({ ...p, pr: v }))}
        onBlur={handleSave}
      />
      <PrefField
        label="Conflict resolution"
        placeholder="e.g., Prefer incoming changes for generated files…"
        value={prefs.conflictResolution}
        onChange={(v) => setPrefs((p) => ({ ...p, conflictResolution: v }))}
        onBlur={handleSave}
      />
    </div>
  );
}

function PrefField({
  label,
  placeholder,
  value,
  onChange,
  onBlur,
}: {
  label: string;
  placeholder: string;
  value: string;
  onChange: (v: string) => void;
  onBlur: () => void;
}) {
  return (
    <div>
      <label className="mb-1 block text-[10px] font-medium text-muted-foreground">{label}</label>
      <Textarea
        className="w-full rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary resize-y min-h-[40px]"
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onBlur={onBlur}
        rows={2}
      />
    </div>
  );
}

// ── MCP Servers Section ─────────────────────────────────────────────

function MCPServersSection() {
  const [servers, setServers] = useState<Array<{ name: string; command: string }>>([]);
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.getMcpServers?.("")
      .then((result: any) => {
        if (result?.success && result.servers) setServers(result.servers);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  if (!loaded) {
    return (
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Loader2 size={11} className="animate-spin" /> Scanning…
      </div>
    );
  }

  if (servers.length === 0) {
    return (
      <p className="text-[10px] text-muted-foreground">
        No MCP servers configured. Add a <code className="font-mono text-foreground/80">.mcp.json</code> to your repo root.
      </p>
    );
  }

  return (
    <div className="space-y-1.5">
      {servers.map((s) => (
        <div key={s.name} className="flex items-center gap-2 rounded border border-border/60 bg-background px-3 py-1.5">
          <Server size={10} className="text-primary shrink-0" />
          <span className="text-[10px] font-medium text-foreground">{s.name}</span>
          <span className="flex-1 truncate text-[9px] font-mono text-muted-foreground">{s.command}</span>
        </div>
      ))}
    </div>
  );
}

// ── Usage Section ───────────────────────────────────────────────────

type DateRange = "7d" | "30d" | "all";

interface WorkspaceCost {
  workspaceId: string;
  agentType: string;
  model: string;
  estimatedCostUsd?: number;
}

function UsageSection() {
  const [costs, setCosts] = useState<WorkspaceCost[]>([]);
  const [range, setRange] = useState<DateRange>("30d");
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.getAllCosts?.()
      .then((result: any) => {
        if (result?.success && result.costs) setCosts(result.costs);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  if (!loaded) {
    return (
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Loader2 size={11} className="animate-spin" /> Loading usage…
      </div>
    );
  }

  const totalCost = costs.every(cost => cost.estimatedCostUsd !== undefined)
    ? costs.reduce((sum, cost) => sum + cost.estimatedCostUsd!, 0) : undefined;

  if (costs.length === 0) {
    return (
      <p className="text-[10px] text-muted-foreground">No usage data yet. Costs are tracked per task.</p>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <div>
          <div className="text-lg font-semibold text-foreground">
            {totalCost === undefined ? "Unavailable" : `$${totalCost.toFixed(2)}`}
          </div>
          <div className="text-[10px] text-muted-foreground">
            Total across {costs.length} workspace{costs.length !== 1 ? "s" : ""}
          </div>
        </div>
        <NativeSelect
          className="rounded border border-border bg-background px-2 py-0.5 text-[10px] focus-visible:outline-none"
          value={range}
          onChange={(e) => setRange(e.target.value as DateRange)}
        >
          <option value="7d">Last 7 days</option>
          <option value="30d">Last 30 days</option>
          <option value="all">All time</option>
        </NativeSelect>
      </div>
    </div>
  );
}
