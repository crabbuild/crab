/**
 * Preset dropdown UI for the terminal create (+) button.
 *
 * Displays a grouped list of built-in agent presets and custom commands.
 * Hovering over a built-in preset reveals a flyout sub-menu to the right
 * with popular flags/modes for that agent.
 * Includes an "Add Custom Command" option that opens an inline form.
 * Supports drag-and-drop reordering via move up/down buttons.
 */

import { useEffect, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Plus,
  Pencil,
  Trash2,
  ArrowUp,
  ArrowDown,
} from "lucide-react";
import {
  createPresetManager,
  type StartupCommand,
  type SubOption,
} from "../lib/terminal-presets";
import { AgentIcon, agentAccentColor } from "./orchestrator/AgentIcon";

// --- Types ---

interface PresetDropdownProps {
  /** Callback when a preset is selected. Receives the command to execute. */
  onSelect: (command: StartupCommand) => void;
  /** Callback to spawn a plain terminal (no preset). */
  onSpawnPlain: () => void;
}

interface CustomCommandFormData {
  name: string;
  command: string;
  cwd: string;
  env: string; // key=value pairs, one per line
}

// --- Component ---

export function PresetDropdown({ onSelect, onSpawnPlain }: PresetDropdownProps) {
  const [open, setOpen] = useState(false);
  const [manager] = useState(() => createPresetManager());
  const [grouped, setGrouped] = useState(() => manager.getGrouped());
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [hoveredId, setHoveredId] = useState<string | null>(null);
  const [formData, setFormData] = useState<CustomCommandFormData>({
    name: "",
    command: "",
    cwd: "",
    env: "",
  });
  const [formErrors, setFormErrors] = useState<string[]>([]);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const hoverTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** Tracks which sub-option is showing an inline input (for needsInput without choices). */
  const [inputForSub, setInputForSub] = useState<{ presetId: string; subId: string; command: string; placeholder: string } | null>(null);
  const [inputValue, setInputValue] = useState("");
  /** Tracks which sub-option with choices is showing its nested choice menu. */
  const [choicesForSub, setChoicesForSub] = useState<string | null>(null);

  // Close dropdown on outside click
  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false);
        setShowForm(false);
        setEditingId(null);
        setHoveredId(null);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  // Cleanup hover timer on unmount
  useEffect(() => {
    return () => {
      if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    };
  }, []);

  function refreshGrouped() {
    setGrouped(manager.getGrouped());
  }

  function handleSelect(cmd: StartupCommand) {
    setOpen(false);
    setHoveredId(null);
    onSelect(cmd);
  }

  function handleSubOptionSelect(parent: StartupCommand, sub: SubOption) {
    // If sub-option has known choices, show nested choice menu
    if (sub.needsInput && sub.choices && sub.choices.length > 0) {
      setChoicesForSub((prev) => (prev === sub.id ? null : sub.id));
      return;
    }
    // If sub-option needs free-text input, show inline input
    if (sub.needsInput || sub.command.endsWith(" ")) {
      setInputForSub({
        presetId: parent.id,
        subId: sub.id,
        command: sub.command,
        placeholder: sub.inputPlaceholder || "value",
      });
      setInputValue("");
      return;
    }
    // Otherwise execute immediately
    setOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    setChoicesForSub(null);
    onSelect({
      ...parent,
      name: `${parent.name} · ${sub.label}`,
      command: sub.command,
    });
  }

  function handleChoiceSelect(parent: StartupCommand, sub: SubOption, choice: { label: string; value: string }) {
    setOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    setChoicesForSub(null);
    onSelect({
      ...parent,
      name: `${parent.name} · ${sub.label} · ${choice.label}`,
      command: `${sub.command}${choice.value}`,
    });
  }

  function handleInputSubmit(parent: StartupCommand, sub: SubOption) {
    if (!inputValue.trim()) return;
    setOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    onSelect({
      ...parent,
      name: `${parent.name} · ${sub.label} · ${inputValue.trim()}`,
      command: `${sub.command}${inputValue.trim()}`,
    });
  }

  function handleMouseEnter(id: string) {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(id), 80);
  }

  function handleMouseLeave() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(null), 150);
  }

  function handleSubMenuEnter() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
  }

  function handleSubMenuLeave() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(null), 150);
  }

  function handleAddCustom() {
    setShowForm(true);
    setEditingId(null);
    setFormData({ name: "", command: "", cwd: "", env: "" });
    setFormErrors([]);
  }

  function handleEdit(cmd: StartupCommand) {
    setShowForm(true);
    setEditingId(cmd.id);
    const envStr = cmd.env
      ? Object.entries(cmd.env).map(([k, v]) => `${k}=${v}`).join("\n")
      : "";
    setFormData({
      name: cmd.name,
      command: cmd.command,
      cwd: cmd.cwd || "",
      env: envStr,
    });
    setFormErrors([]);
  }

  function handleDelete(id: string) {
    if (!window.confirm("Delete this custom command?")) return;
    manager.removeCustom(id);
    refreshGrouped();
  }

  function handleSave() {
    const validation = manager.validate({ name: formData.name, command: formData.command });
    if (!validation.valid) {
      setFormErrors(validation.errors);
      return;
    }

    // Parse environment variables
    const env: Record<string, string> = {};
    if (formData.env.trim()) {
      for (const line of formData.env.split("\n")) {
        const eqIdx = line.indexOf("=");
        if (eqIdx > 0) {
          env[line.slice(0, eqIdx).trim()] = line.slice(eqIdx + 1).trim();
        }
      }
    }

    if (editingId) {
      manager.updateCustom(editingId, {
        name: formData.name,
        command: formData.command,
        cwd: formData.cwd || undefined,
        env: Object.keys(env).length > 0 ? env : undefined,
      });
    } else {
      manager.addCustom({
        name: formData.name,
        command: formData.command,
        cwd: formData.cwd || undefined,
        env: Object.keys(env).length > 0 ? env : undefined,
        visible: true,
      });
    }

    setShowForm(false);
    setEditingId(null);
    refreshGrouped();
  }

  function handleMoveUp(id: string) {
    const customs = grouped.custom;
    const idx = customs.findIndex((c) => c.id === id);
    if (idx <= 0) return;
    const ids = customs.map((c) => c.id);
    [ids[idx - 1], ids[idx]] = [ids[idx], ids[idx - 1]];
    manager.reorder(ids);
    refreshGrouped();
  }

  function handleMoveDown(id: string) {
    const customs = grouped.custom;
    const idx = customs.findIndex((c) => c.id === id);
    if (idx < 0 || idx >= customs.length - 1) return;
    const ids = customs.map((c) => c.id);
    [ids[idx], ids[idx + 1]] = [ids[idx + 1], ids[idx]];
    manager.reorder(ids);
    refreshGrouped();
  }

  return (
    <div className="relative inline-flex" ref={dropdownRef}>
      {/* Main + button */}
      <button
        type="button"
        onClick={onSpawnPlain}
        className="flex h-full items-center px-1.5 text-gx-muted hover:text-gx-text"
        title="New terminal (⌘⇧`)"
      >
        <Plus size={12} />
      </button>

      {/* Dropdown trigger (▾) */}
      <button
        type="button"
        onClick={() => { setOpen(!open); setShowForm(false); setHoveredId(null); }}
        className="flex h-full items-center px-0.5 text-gx-muted hover:text-gx-text"
        title="Terminal presets"
      >
        <ChevronDown size={10} />
      </button>

      {/* Dropdown menu */}
      {open && (
        <div className="absolute left-0 top-full z-50 mt-1 min-w-[240px] rounded border border-gx-border bg-gx-surface shadow-lg">
          {!showForm ? (
            <div className="py-1">
              {/* Built-in presets */}
              {grouped.builtin.length > 0 && (
                <>
                  <div className="px-3 py-1 text-[10px] font-medium uppercase text-gx-muted tracking-wide">
                    Agent CLIs
                  </div>
                  {grouped.builtin.map((preset) => (
                    <div
                      key={preset.id}
                      className="relative"
                      onMouseEnter={() => handleMouseEnter(preset.id)}
                      onMouseLeave={handleMouseLeave}
                    >
                      <div className="flex w-full items-center hover:bg-gx-surface-hover">
                        {/* Main preset button — clicking launches the default command */}
                        <button
                          type="button"
                          onClick={() => handleSelect(preset)}
                          className="flex flex-1 items-center gap-2 pl-3 pr-1 py-1.5 text-left text-[11px] text-gx-text"
                        >
                          <AgentIcon agentName={preset.icon} size={12} className={agentAccentColor(preset.icon)} />
                          <span className="truncate">{preset.name}</span>
                          <span className="ml-auto text-gx-muted font-mono text-[10px]">{preset.command}</span>
                        </button>
                        {/* Chevron — clicking toggles the sub-menu (fallback for hover) */}
                        {preset.subOptions && preset.subOptions.length > 0 && (
                          <button
                            type="button"
                            onClick={(e) => {
                              e.stopPropagation();
                              setHoveredId((prev) => (prev === preset.id ? null : preset.id));
                            }}
                            className="flex items-center justify-center px-2 py-1.5 text-gx-muted hover:text-gx-text shrink-0"
                          >
                            <ChevronRight size={10} />
                          </button>
                        )}
                      </div>

                      {/* Flyout sub-menu (appears on hover/click to the right) */}
                      {hoveredId === preset.id && preset.subOptions && preset.subOptions.length > 0 && (
                        <div
                          className="fixed z-[9999] min-w-[220px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg"
                          ref={(el) => {
                            if (!el) return;
                            // Position the flyout to the right of the parent row
                            const parent = el.parentElement;
                            if (!parent) return;
                            const parentRect = parent.getBoundingClientRect();
                            el.style.left = `${parentRect.right + 2}px`;
                            el.style.top = `${parentRect.top}px`;
                            // Ensure it doesn't overflow the viewport bottom
                            const elRect = el.getBoundingClientRect();
                            if (elRect.bottom > window.innerHeight - 8) {
                              el.style.top = `${window.innerHeight - 8 - elRect.height}px`;
                            }
                          }}
                          onMouseEnter={handleSubMenuEnter}
                          onMouseLeave={handleSubMenuLeave}
                        >
                          {preset.subOptions.map((sub) => (
                            <div key={sub.id}>
                              {/* Sub-option with choices: show as expandable with nested items */}
                              {sub.needsInput && sub.choices && sub.choices.length > 0 ? (
                                <div>
                                  <button
                                    type="button"
                                    onClick={() => setChoicesForSub((prev) => (prev === sub.id ? null : sub.id))}
                                    className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                                    title={sub.description}
                                  >
                                    <span className="truncate">{sub.label}</span>
                                    <ChevronRight size={9} className="ml-auto shrink-0 opacity-60" />
                                  </button>
                                  {choicesForSub === sub.id && (
                                    <div className="border-l-2 border-gx-accent/30 ml-3 pl-1 py-0.5">
                                      {sub.choices.map((choice) => (
                                        <button
                                          key={choice.id}
                                          type="button"
                                          onClick={() => handleChoiceSelect(preset, sub, choice)}
                                          className="flex w-full items-center gap-2 px-2 py-1 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text rounded"
                                          title={choice.description}
                                        >
                                          <span className="truncate">{choice.label}</span>
                                          <span className="ml-auto font-mono text-[9px] opacity-60">{choice.value}</span>
                                        </button>
                                      ))}
                                      {/* Also allow custom input for choices */}
                                      <div className="px-2 py-1">
                                        <form
                                          onSubmit={(e) => {
                                            e.preventDefault();
                                            if (inputValue.trim()) {
                                              handleChoiceSelect(preset, sub, { label: inputValue.trim(), value: inputValue.trim() });
                                            }
                                          }}
                                          className="flex items-center gap-1"
                                        >
                                          <input
                                            type="text"
                                            value={inputForSub?.subId === sub.id ? inputValue : ""}
                                            onChange={(e) => { setInputValue(e.target.value); setInputForSub({ presetId: preset.id, subId: sub.id, command: sub.command, placeholder: sub.inputPlaceholder || "custom" }); }}
                                            placeholder={sub.inputPlaceholder || "custom value"}
                                            className="flex-1 min-w-0 px-1.5 py-0.5 text-[10px] rounded border border-gx-border bg-gx-bg text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                                            autoFocus={choicesForSub === sub.id}
                                          />
                                          <button type="submit" className="text-[10px] text-gx-accent hover:text-gx-text px-1">↵</button>
                                        </form>
                                      </div>
                                    </div>
                                  )}
                                </div>
                              ) : sub.needsInput || sub.command.endsWith(" ") ? (
                                /* Sub-option that needs free-text input */
                                <div>
                                  <button
                                    type="button"
                                    onClick={() => {
                                      setInputForSub({
                                        presetId: preset.id,
                                        subId: sub.id,
                                        command: sub.command,
                                        placeholder: sub.inputPlaceholder || "value",
                                      });
                                      setInputValue("");
                                    }}
                                    className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                                    title={sub.description}
                                  >
                                    <span className="truncate">{sub.label}</span>
                                    <span className="ml-auto text-[9px] italic opacity-60">requires input</span>
                                  </button>
                                  {inputForSub?.subId === sub.id && (
                                    <div className="px-3 py-1">
                                      <form
                                        onSubmit={(e) => { e.preventDefault(); handleInputSubmit(preset, sub); }}
                                        className="flex items-center gap-1"
                                      >
                                        <input
                                          type="text"
                                          value={inputValue}
                                          onChange={(e) => setInputValue(e.target.value)}
                                          placeholder={sub.inputPlaceholder || "value"}
                                          className="flex-1 min-w-0 px-1.5 py-0.5 text-[10px] rounded border border-gx-border bg-gx-bg text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                                          autoFocus
                                        />
                                        <button
                                          type="submit"
                                          disabled={!inputValue.trim()}
                                          className="text-[10px] text-gx-accent hover:text-gx-text px-1 disabled:opacity-30"
                                        >
                                          ↵
                                        </button>
                                      </form>
                                    </div>
                                  )}
                                </div>
                              ) : (
                                /* Normal sub-option — execute immediately */
                                <button
                                  type="button"
                                  onClick={() => handleSubOptionSelect(preset, sub)}
                                  className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                                  title={sub.description}
                                >
                                  <span className="truncate">{sub.label}</span>
                                  <span className="ml-auto font-mono text-[9px] opacity-70 truncate max-w-[130px]">{sub.command}</span>
                                </button>
                              )}
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  ))}
                </>
              )}

              {/* Custom commands */}
              {grouped.custom.length > 0 && (
                <>
                  <div className="border-t border-gx-border my-1" />
                  <div className="px-3 py-1 text-[10px] font-medium uppercase text-gx-muted tracking-wide">
                    Custom Commands
                  </div>
                  {grouped.custom.map((cmd, idx) => (
                    <div
                      key={cmd.id}
                      className="group flex items-center gap-1 px-3 py-1.5 hover:bg-gx-surface-hover"
                    >
                      <button
                        type="button"
                        onClick={() => handleSelect(cmd)}
                        className="flex-1 text-left text-[11px] text-gx-text truncate"
                      >
                        {cmd.name}
                      </button>
                      <div className="hidden group-hover:flex items-center gap-0.5">
                        <button
                          type="button"
                          onClick={(e) => { e.stopPropagation(); handleMoveUp(cmd.id); }}
                          className="p-0.5 text-gx-muted hover:text-gx-text"
                          title="Move up"
                          disabled={idx === 0}
                        >
                          <ArrowUp size={10} />
                        </button>
                        <button
                          type="button"
                          onClick={(e) => { e.stopPropagation(); handleMoveDown(cmd.id); }}
                          className="p-0.5 text-gx-muted hover:text-gx-text"
                          title="Move down"
                          disabled={idx === grouped.custom.length - 1}
                        >
                          <ArrowDown size={10} />
                        </button>
                        <button
                          type="button"
                          onClick={(e) => { e.stopPropagation(); handleEdit(cmd); }}
                          className="p-0.5 text-gx-muted hover:text-gx-text"
                          title="Edit"
                        >
                          <Pencil size={10} />
                        </button>
                        <button
                          type="button"
                          onClick={(e) => { e.stopPropagation(); handleDelete(cmd.id); }}
                          className="p-0.5 text-gx-muted hover:text-gx-error"
                          title="Delete"
                        >
                          <Trash2 size={10} />
                        </button>
                      </div>
                    </div>
                  ))}
                </>
              )}

              {/* Add custom command */}
              <div className="border-t border-gx-border my-1" />
              <button
                type="button"
                onClick={handleAddCustom}
                className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
              >
                <Plus size={12} />
                Add Custom Command
              </button>
            </div>
          ) : (
            /* Custom command form */
            <div className="p-3 flex flex-col gap-2">
              <div className="text-[11px] font-medium text-gx-text">
                {editingId ? "Edit Command" : "Add Custom Command"}
              </div>

              {/* Name field */}
              <div>
                <label className="text-[10px] text-gx-muted">Name *</label>
                <input
                  type="text"
                  value={formData.name}
                  onChange={(e) => setFormData({ ...formData, name: e.target.value })}
                  placeholder="My Tool"
                  className="w-full mt-0.5 px-2 py-1 text-[11px] rounded border border-gx-border bg-gx-surface-hover text-gx-text focus:outline-none focus:border-gx-accent"
                  autoFocus
                />
              </div>

              {/* Command field */}
              <div>
                <label className="text-[10px] text-gx-muted">Shell Command *</label>
                <input
                  type="text"
                  value={formData.command}
                  onChange={(e) => setFormData({ ...formData, command: e.target.value })}
                  placeholder="mytool --interactive"
                  className="w-full mt-0.5 px-2 py-1 text-[11px] rounded border border-gx-border bg-gx-surface-hover text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                />
              </div>

              {/* Working directory (optional) */}
              <div>
                <label className="text-[10px] text-gx-muted">Working Directory (optional)</label>
                <input
                  type="text"
                  value={formData.cwd}
                  onChange={(e) => setFormData({ ...formData, cwd: e.target.value })}
                  placeholder="/path/to/project"
                  className="w-full mt-0.5 px-2 py-1 text-[11px] rounded border border-gx-border bg-gx-surface-hover text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                />
              </div>

              {/* Environment variables (optional) */}
              <div>
                <label className="text-[10px] text-gx-muted">Environment Variables (optional, KEY=VALUE per line)</label>
                <textarea
                  value={formData.env}
                  onChange={(e) => setFormData({ ...formData, env: e.target.value })}
                  placeholder="API_KEY=xxx&#10;DEBUG=1"
                  rows={2}
                  className="w-full mt-0.5 px-2 py-1 text-[11px] rounded border border-gx-border bg-gx-surface-hover text-gx-text font-mono focus:outline-none focus:border-gx-accent resize-none"
                />
              </div>

              {/* Errors */}
              {formErrors.length > 0 && (
                <div className="text-[10px] text-gx-error">
                  {formErrors.join(". ")}
                </div>
              )}

              {/* Actions */}
              <div className="flex items-center gap-2 mt-1">
                <button
                  type="button"
                  onClick={handleSave}
                  className="px-3 py-1 text-[11px] rounded bg-gx-accent text-white hover:opacity-90"
                >
                  {editingId ? "Save" : "Add"}
                </button>
                <button
                  type="button"
                  onClick={() => { setShowForm(false); setEditingId(null); }}
                  className="px-3 py-1 text-[11px] rounded text-gx-muted hover:text-gx-text"
                >
                  Cancel
                </button>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
