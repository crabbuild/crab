import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuPortal,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { requestConfirmation, requestPrompt } from "@/lib/application-dialogs";
import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  Pencil,
  Plus,
  RefreshCw,
  Terminal,
  Trash2,
} from "lucide-react";
import { useEffect, useRef, useState, type ReactElement } from "react";
import type { DetectedAgent } from "../../electron/orchestrator-adapters";
import { publishToast } from "../lib/toasts";
import {
  createPresetManager,
  availableAgentPresets,
  type StartupCommand,
  type SubOption,
} from "../lib/terminal-presets";
import { AgentIcon, agentAccentColor } from "./orchestrator/AgentIcon";

interface PresetActions {
  onSelect: (command: StartupCommand) => void;
  onSpawnPlain: () => void;
}

interface CommandForm {
  id?: string;
  name: string;
  command: string;
  cwd: string;
  env: string;
}

export function PresetDropdown(props: PresetActions) {
  return (
    <div className="inline-flex">
      <Button
        variant="ghost"
        size="icon-xs"
        onClick={props.onSpawnPlain}
        title="New terminal (⌘⇧`)"
      >
        <Plus />
      </Button>
      <TerminalPresetMenu {...props}>
        <Button
          variant="ghost"
          size="icon-xs"
          aria-label="Terminal presets"
        >
          <ChevronDown />
        </Button>
      </TerminalPresetMenu>
    </div>
  );
}

// Both terminal entry points share menu behavior and persisted custom commands.
export function TerminalPresetMenu({
  children,
  onSelect, onSpawnPlain,
}: PresetActions & { children: ReactElement }) {
  const [open, setOpen] = useState(false);
  const [commands, setCommands] = useState(() => createPresetManager().getAll());
  const [catalog, setCatalog] = useState<{ agents: DetectedAgent[]; loading: boolean; error?: string }>({ agents: [], loading: true });
  const [probeRevision, setProbeRevision] = useState(0);
  const lastProbeRevision = useRef(0);
  const grouped = {
    builtin: availableAgentPresets(commands, catalog.agents),
    custom: commands.filter(command => command.category === "custom" && command.visible),
  };
  const [form, setForm] = useState<CommandForm | null>(null);
  const [errors, setErrors] = useState<string[]>([]);
  const refresh = () => setCommands(createPresetManager().getAll());
  useEffect(() => {
    if (!open) return;
    let active = true;
    const force = lastProbeRevision.current !== probeRevision;
    lastProbeRevision.current = probeRevision;
    setCatalog({ agents: [], loading: true });
    // These presets spawn local shells in both the pane and top bar. Remote
    // repository detection must not advertise a server CLI for a local launch.
    void (async () => {
      try {
        const result = await window.crab.orchestrator.detectAgents({ force });
        if (!result.success) throw new Error(result.error ?? "Agent detection failed");
        if (active) setCatalog({ agents: result.agents ?? [], loading: false });
      } catch (error) {
        if (active) setCatalog({ agents: [], loading: false, error: error instanceof Error ? error.message : String(error) });
      }
    })();
    return () => { active = false; };
  }, [open, probeRevision]);
    function edit(command?: StartupCommand) {
    setErrors([]);
    setForm({
      id: command?.id,
      name: command?.name ?? "",
      command: command?.command ?? "",
      cwd: command?.cwd ?? "",
      env: Object.entries(command?.env ?? {})
        .map(([key, value]) => `${key}=${value}`)
        .join("\n"),
    });
  }

  async function launch(command: StartupCommand) {
    setOpen(false);
    try {
      if (!command.agentType) { onSelect(command); return; }
      const prepared = await window.crab.termAgentCommand(command.agentType, command.command);
      onSelect({ ...command, command: prepared.command, env: { ...command.env, ...prepared.env } });
    } catch (error) {
      publishToast({ kind: "error", title: "Cannot launch agent CLI", body: error instanceof Error ? error.message : String(error) });
    }
  }

  function launchOption(
    parent: StartupCommand,
    option: SubOption,
    value?: string,
  ) {
    launch({
      ...parent,
      name: `${parent.name} · ${option.label}${value ? ` · ${value}` : ""}`,
      command: option.command + (value ?? ""),
    });
  }

  async function customValue(parent: StartupCommand, option: SubOption) {
    const value = await requestPrompt(
      `${option.label}: ${option.inputPlaceholder ?? "value"}`,
    );
    if (value?.trim()) launchOption(parent, option, value.trim());
  }

  async function remove(command: StartupCommand) {
    if (
      !(await requestConfirmation(`Delete custom command “${command.name}”?`))) return;
    createPresetManager().removeCustom(command.id);
    refresh();
  }

  function move(id: string, delta: number) {
    const ids = grouped.custom.map((command) => command.id);
    const index = ids.indexOf(id);
    const next = index + delta;
    if (index < 0 || next < 0 || next >= ids.length) return;
    [ids[index], ids[next]] = [ids[next], ids[index]];
    createPresetManager().reorder(ids);
    refresh();
  }

  function save() {
    if (!form) return;
    const manager = createPresetManager();
    const validation = manager.validate(form);
    if (!validation.valid) {
      setErrors(validation.errors);
      return;
    }
    const env: Record<string, string> = {};
    for (const line of form.env.split("\n")) {
        const separator = line.indexOf("=");
        if (separator > 0)
        env[line.slice(0, separator).trim()] = line.slice(separator + 1).trim();
        }
    const command = {
        name: form.name,
        command: form.command,
        cwd: form.cwd || undefined,
        env: Object.keys(env).length ? env : undefined,
      };
    if (form.id) manager.updateCustom(form.id, command);
    else manager.addCustom({ ...command, visible: true });
    setForm(null);
    refresh();
  }

  return (
    <>
      <DropdownMenu
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (next) refresh();
        }}
      >
        <DropdownMenuTrigger asChild>{children}</DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-64">
          <DropdownMenuGroup>
            <DropdownMenuItem onSelect={onSpawnPlain}>
              <Terminal size={13} />
              Open Terminal
            </DropdownMenuItem>
          </DropdownMenuGroup>
          <DropdownMenuSeparator />
          <DropdownMenuLabel>Agent CLIs</DropdownMenuLabel>
          {catalog.loading && <DropdownMenuLabel role="status">Checking available CLIs…</DropdownMenuLabel>}
          {catalog.error && <DropdownMenuLabel role="status">Could not detect agent CLIs. Refresh to retry.</DropdownMenuLabel>}
          {!catalog.loading && !catalog.error && grouped.builtin.length === 0 && <DropdownMenuLabel role="status">No agent CLIs found</DropdownMenuLabel>}
          <DropdownMenuGroup>
            {grouped.builtin.map((preset) =>
              preset.subOptions?.length ? (
                <DropdownMenuSub key={preset.agentType}>
                  <DropdownMenuSubTrigger>
                    <AgentIcon
                      agentName={preset.icon}
                      size={13}
                      className={agentAccentColor(preset.icon)}
                    />
                    {preset.name}
                  </DropdownMenuSubTrigger>
                  <DropdownMenuPortal>
                    <DropdownMenuSubContent className="min-w-64">
                      <DropdownMenuItem onSelect={() => launch(preset)}>
                        Launch {preset.name}
                        <span className="ml-auto font-mono text-muted-foreground">
                          {preset.command}
                        </span>
                      </DropdownMenuItem>
                      <DropdownMenuSeparator />
                      <DropdownMenuGroup>
                        {preset.subOptions.map((option) =>
                          option.choices?.length ? (
                            <DropdownMenuSub key={option.id}>
                              <DropdownMenuSubTrigger>
                                {option.label}
                              </DropdownMenuSubTrigger>
                              <DropdownMenuPortal>
                                <DropdownMenuSubContent>
                                  <DropdownMenuGroup>
                                    {option.choices.map((choice) => (
                                      <DropdownMenuItem
                                        key={choice.id}
                                        onSelect={() =>
                                          launchOption(
                                            preset,
                                            option,
                                            choice.value,
                                          )
                                        }
                                      >
                                        {choice.label}
                                      </DropdownMenuItem>
                                    ))}
                                  </DropdownMenuGroup>
                                  <DropdownMenuSeparator />
                                  <DropdownMenuItem
                                    onSelect={() =>
                                      void customValue(preset, option)
                                    }
                                  >
                                    Custom value…
                                  </DropdownMenuItem>
                                </DropdownMenuSubContent>
                              </DropdownMenuPortal>
                            </DropdownMenuSub>
                          ) : (
                            <DropdownMenuItem
                              key={option.id}
                              onSelect={() => {
                                if (
                                  option.needsInput ||
                                  option.command.endsWith(" ")
                                )
                                  void customValue(preset, option);
                                else launchOption(preset, option);
                              }}
                            >
                              {option.label}
                              {(option.needsInput ||
                                option.command.endsWith(" ")) &&
                                "…"}
                            </DropdownMenuItem>
                          ),
                        )}
                      </DropdownMenuGroup>
                    </DropdownMenuSubContent>
                  </DropdownMenuPortal>
                </DropdownMenuSub>
              ) : (
                <DropdownMenuItem
                  key={preset.agentType}
                  onSelect={() => launch(preset)}
                >
                  <AgentIcon
                    agentName={preset.icon}
                    size={13}
                    className={agentAccentColor(preset.icon)}
                  />
                  {preset.name}
                </DropdownMenuItem>
              ),
            )}
          </DropdownMenuGroup>
          <DropdownMenuGroup>
            <DropdownMenuItem disabled={catalog.loading} onSelect={event => { event.preventDefault(); setProbeRevision(value => value + 1); }}>
              <RefreshCw /> Refresh CLI list
            </DropdownMenuItem>
          </DropdownMenuGroup>
          {grouped.custom.length > 0 && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>Custom commands</DropdownMenuLabel>
              <DropdownMenuGroup>
                {grouped.custom.map((command, index) => (
                  <DropdownMenuSub key={command.id}>
                    <DropdownMenuSubTrigger>
                      {command.name}
                    </DropdownMenuSubTrigger>
                    <DropdownMenuPortal>
                      <DropdownMenuSubContent>
                        <DropdownMenuGroup>
                          <DropdownMenuItem onSelect={() => launch(command)}>
                            <Terminal size={12} />
                            Run {command.name}
                          </DropdownMenuItem>
                          <DropdownMenuItem onSelect={() => edit(command)}>
                            <Pencil size={12} />
                            Edit
                          </DropdownMenuItem>
                          <DropdownMenuItem
                            disabled={index === 0}
                            onSelect={() => move(command.id, - 1)}
                          >
                            <ArrowUp size={12} />
                            Move up
                          </DropdownMenuItem>
                          <DropdownMenuItem
                            disabled={index === grouped.custom.length - 1}
                            onSelect={() => move(command.id, 1)}
                          >
                            <ArrowDown size={12} />
                            Move down
                          </DropdownMenuItem>
                          <DropdownMenuItem
                            className="text-destructive focus:text-destructive"
                            onSelect={() => void remove(command)}
                          >
                            <Trash2 size={12} />
                            Delete
                          </DropdownMenuItem>
                        </DropdownMenuGroup>
                      </DropdownMenuSubContent>
                    </DropdownMenuPortal>
                  </DropdownMenuSub>
                ))}
              </DropdownMenuGroup>
            </>
          )}
          <DropdownMenuSeparator />
          <DropdownMenuItem onSelect={() => edit()}>
            <Plus size={12} />
            Add custom command…
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <Dialog
        open={form !== null}
        onOpenChange={(next) => {
          if (!next) setForm(null);
        }}
      >
        <DialogContent aria-describedby={undefined}>
          <DialogTitle className="text-sm">
            {form?.id ? "Edit command" : "Add custom command"}
          </DialogTitle>
          {form && (
            <form
              className="grid gap-3"
              onSubmit={(event) => {
                event.preventDefault();
                save();
              }}
            >
              <label className="grid gap-1 text-xs">
                Name
                <Input
                  autoFocus
                  required
                  value={form.name}
                  onChange={(event) =>
                    setForm({ ...form, name: event.target.value })
                  }
                />
              </label>
              <label className="grid gap-1 text-xs">
                Shell command
                <Input
                  required
                  className="font-mono"
                  value={form.command}
                  onChange={(event) =>
                    setForm({ ...form, command: event.target.value })
                  }
                />
              </label>
              <label className="grid gap-1 text-xs">
                Working directory (optional)
                <Input
                  value={form.cwd}
                  onChange={(event) =>
                    setForm({ ...form, cwd: event.target.value })
                  }
                />
              </label>
              <label className="grid gap-1 text-xs">
                Environment variables (KEY=VALUE per line)
                <Textarea
                  rows={3}
                  value={form.env}
                  onChange={(event) =>
                    setForm({ ...form, env: event.target.value })
                  }
                />
              </label>
              {errors.length > 0 && (
                <p role="alert" className="text-xs text-destructive">
                  {errors.join(". ")}
                </p>
              )}
              <div className="flex justify-end gap-2">
                <Button type="button"
        onClick={() => setForm(null)}>
                  Cancel
                </Button>
                <Button variant="default" type="submit">
                  Save
                </Button>
              </div>
            </form>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}
