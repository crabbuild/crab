import { Input } from "@/components/ui/input";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Search, ChevronLeft } from "lucide-react";
import clsx from "clsx";
import {
  filterCommands,
  executeCommand,
  useCommands,
} from "../lib/command-registry";
import type {
  Command,
  CommandContext,
  CommandResult,
  PromptConfig,
} from "../lib/command-registry";

export interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  repoId: string;
  context: CommandContext;
  onCommandExecuted?: (commandId: string, result: CommandResult) => void;
}

type PaletteMode =
  | { kind: "commands"; query: string; selectedIndex: number }
  | {
      kind: "prompt";
      command: Command;
      promptConfig: PromptConfig;
      query: string;
      selectedIndex: number;
      previousInputs: string[];
    };

/**
 * Command palette — a modal overlay with fuzzy search, keyboard navigation,
 * and a secondary-prompt state machine for commands that need additional input.
 */
export function CommandPalette({
  open,
  onClose,
  repoId: _repoId,
  context,
  onCommandExecuted,
}: CommandPaletteProps) {
  void _repoId; // Available for future use (e.g., fetching branch lists in prompt mode).
  const [mode, setMode] = useState<PaletteMode>({
    kind: "commands",
    query: "",
    selectedIndex: 0,
  });

  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  // Subscribe to registry changes so the list updates if commands are registered/unregistered.
  useCommands();

  // Capture the previously focused element when the palette opens.
  useEffect(() => {
    if (open) {
      previousFocusRef.current = document.activeElement as HTMLElement | null;
      setMode({ kind: "commands", query: "", selectedIndex: 0 });
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  // Restore focus to the previously focused element on close.
  useEffect(() => {
    if (!open && previousFocusRef.current) {
      previousFocusRef.current.focus();
      previousFocusRef.current = null;
    }
  }, [open]);

  // Compute filtered results based on current mode.
  const results: Command[] = useMemo(() => {
    if (mode.kind === "commands") {
      return filterCommands(mode.query, context);
    }
    // In prompt mode, we don't show commands — prompt options are handled separately.
    return [];
  }, [mode, context]);

  // For prompt mode with "choice" kind, filter choices by query.
  const promptChoices = useMemo(() => {
    if (mode.kind !== "prompt") return [];
    const { promptConfig, query } = mode;
    if (promptConfig.kind === "choice" && promptConfig.choices) {
      if (!query.trim()) return promptConfig.choices;
      const q = query.toLowerCase();
      return promptConfig.choices.filter(
        (c) => c.label.toLowerCase().includes(q) || c.value.toLowerCase().includes(q),
      );
    }
    return [];
  }, [mode]);

  // The total number of items in the current list (for navigation bounds).
  const itemCount = useMemo(() => {
    if (mode.kind === "commands") return results.length;
    if (mode.kind === "prompt" && mode.promptConfig.kind === "choice") {
      return promptChoices.length;
    }
    // For free-text prompts (branch, commit, ref), there's no list to navigate.
    return 0;
  }, [mode, results.length, promptChoices.length]);

  // Keep selected index in bounds when results change.
  useEffect(() => {
    if (mode.kind === "commands" && mode.selectedIndex >= results.length) {
      setMode((prev) => ({
        ...prev,
        selectedIndex: Math.max(0, results.length - 1),
      }));
    }
  }, [mode, results.length]);

  // Scroll the selected item into view.
  useEffect(() => {
    if (!listRef.current) return;
    const idx = mode.selectedIndex;
    const el = listRef.current.querySelector(`[data-idx="${idx}"]`);
    if (el) {
      el.scrollIntoView({ block: "nearest" });
    }
  }, [mode.selectedIndex]);

  const handleClose = useCallback(() => {
    onClose();
  }, [onClose]);

  const executeSelectedCommand = useCallback(
    async (cmd: Command, input?: string) => {
      handleClose();
      const result = await executeCommand(cmd.id, context, input);
      onCommandExecuted?.(cmd.id, result);
    },
    [handleClose, context, onCommandExecuted],
  );

  const selectCommand = useCallback(
    (cmd: Command) => {
      if (cmd.prompt) {
        // Transition to prompt mode.
        setMode({
          kind: "prompt",
          command: cmd,
          promptConfig: cmd.prompt,
          query: "",
          selectedIndex: 0,
          previousInputs: [],
        });
        requestAnimationFrame(() => inputRef.current?.focus());
      } else {
        executeSelectedCommand(cmd);
      }
    },
    [executeSelectedCommand],
  );

  const submitPrompt = useCallback(
    (value: string) => {
      if (mode.kind !== "prompt") return;
      const { command, promptConfig, previousInputs } = mode;

      if (promptConfig.next) {
        // Multi-step prompt: advance to the next prompt.
        setMode({
          kind: "prompt",
          command,
          promptConfig: promptConfig.next,
          query: "",
          selectedIndex: 0,
          previousInputs: [...previousInputs, value],
        });
        requestAnimationFrame(() => inputRef.current?.focus());
      } else {
        // Final prompt step — execute the command with joined inputs.
        const allInputs = [...previousInputs, value];
        const input = allInputs.join(":");
        executeSelectedCommand(command, input);
      }
    },
    [mode, executeSelectedCommand],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "Escape":
          e.preventDefault();
          if (mode.kind === "prompt") {
            // Return to command list from prompt mode.
            setMode({ kind: "commands", query: "", selectedIndex: 0 });
          } else {
            handleClose();
          }
          break;

        case "ArrowDown":
          e.preventDefault();
          setMode((prev) => ({
            ...prev,
            selectedIndex: Math.min(prev.selectedIndex + 1, Math.max(0, itemCount - 1)),
          }));
          break;

        case "ArrowUp":
          e.preventDefault();
          setMode((prev) => ({
            ...prev,
            selectedIndex: Math.max(prev.selectedIndex - 1, 0),
          }));
          break;

        case "Enter":
          e.preventDefault();
          if (mode.kind === "commands") {
            const cmd = results[mode.selectedIndex];
            if (cmd && cmd.isEnabled(context)) {
              selectCommand(cmd);
            }
          } else if (mode.kind === "prompt") {
            if (mode.promptConfig.kind === "choice") {
              const choice = promptChoices[mode.selectedIndex];
              if (choice) {
                submitPrompt(choice.value);
              }
            } else {
              // Free-text prompt (branch, commit, ref) — submit the query as input.
              if (mode.query.trim()) {
                submitPrompt(mode.query.trim());
              }
            }
          }
          break;
      }
    },
    [mode, itemCount, results, context, selectCommand, promptChoices, submitPrompt, handleClose],
  );

  const handleQueryChange = useCallback((value: string) => {
    setMode((prev) => ({ ...prev, query: value, selectedIndex: 0 }));
  }, []);

  if (!open) return null;

  const placeholder =
    mode.kind === "prompt" ? mode.promptConfig.placeholder : "Run command…";

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center pt-[12vh]"
      onClick={handleClose}
      role="presentation"
    >
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/50" aria-hidden="true" />

      {/* Palette container */}
      <div
        className="relative z-10 flex w-full max-w-lg flex-col overflow-hidden rounded-lg border border-border bg-card shadow-2xl"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={handleKeyDown}
        role="dialog"
        aria-label="Command palette"
        aria-modal="true"
      >
        {/* Search input */}
        <div className="flex items-center gap-2 border-b border-border px-3 py-2">
          {mode.kind === "prompt" && (
            <button
              type="button"
              className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
              onClick={() => setMode({ kind: "commands", query: "", selectedIndex: 0 })}
              aria-label="Back to commands"
            >
              <ChevronLeft size={16} />
            </button>
          )}
          <Search size={16} className="shrink-0 text-muted-foreground" aria-hidden="true" />
          <Input
            ref={inputRef}
            type="text"
            className="flex-1 bg-transparent text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-none"
            placeholder={placeholder}
            value={mode.query}
            onChange={(e) => handleQueryChange(e.target.value)}
            aria-label={placeholder}
            aria-activedescendant={
              mode.kind === "commands" && results[mode.selectedIndex]
                ? `palette-item-${results[mode.selectedIndex].id}`
                : mode.kind === "prompt" && promptChoices[mode.selectedIndex]
                  ? `palette-choice-${mode.selectedIndex}`
                  : undefined
            }
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-results"
            aria-autocomplete="list"
          />
        </div>

        {/* Results list */}
        <div
          ref={listRef}
          id="palette-results"
          className="max-h-[320px] overflow-y-auto py-1"
          role="listbox"
        >
          {mode.kind === "commands" && (
            <>
              {results.length === 0 ? (
                <div className="px-3 py-4 text-center text-sm text-muted-foreground">
                  No matching commands
                </div>
              ) : (
                results.map((cmd, idx) => {
                  const isSelected = idx === mode.selectedIndex;
                  const isDisabled = !cmd.isEnabled(context);
                  return (
                    <div
                      key={cmd.id}
                      id={`palette-item-${cmd.id}`}
                      data-idx={idx}
                      role="option"
                      aria-selected={isSelected}
                      aria-disabled={isDisabled}
                      className={clsx(
                        "flex cursor-pointer items-center gap-2 px-3 py-1.5 text-sm",
                        isDisabled && "opacity-50 cursor-default",
                        isSelected
                          ? "bg-primary/15 text-foreground"
                          : "text-foreground/80 hover:bg-accent",
                      )}
                      onClick={() => {
                        if (!isDisabled) selectCommand(cmd);
                      }}
                      onMouseEnter={() =>
                        setMode((prev) => ({ ...prev, selectedIndex: idx }))
                      }
                    >
                      {cmd.icon && (
                        <cmd.icon size={14} className="shrink-0 text-muted-foreground" aria-hidden="true" />
                      )}
                      <span className="truncate">{cmd.label}</span>
                      <span className="ml-auto flex items-center gap-2">
                        <span className="shrink-0 rounded bg-background px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
                          {cmd.category}
                        </span>
                        {cmd.shortcut && (
                          <span className="shrink-0 text-[11px] text-muted-foreground">
                            {cmd.shortcut}
                          </span>
                        )}
                      </span>
                    </div>
                  );
                })
              )}
            </>
          )}

          {mode.kind === "prompt" && mode.promptConfig.kind === "choice" && (
            <>
              {promptChoices.length === 0 ? (
                <div className="px-3 py-4 text-center text-sm text-muted-foreground">
                  No matching options
                </div>
              ) : (
                promptChoices.map((choice, idx) => {
                  const isSelected = idx === mode.selectedIndex;
                  return (
                    <div
                      key={choice.value}
                      id={`palette-choice-${idx}`}
                      data-idx={idx}
                      role="option"
                      aria-selected={isSelected}
                      className={clsx(
                        "flex cursor-pointer items-center gap-2 px-3 py-1.5 text-sm",
                        isSelected
                          ? "bg-primary/15 text-foreground"
                          : "text-foreground/80 hover:bg-accent",
                      )}
                      onClick={() => submitPrompt(choice.value)}
                      onMouseEnter={() =>
                        setMode((prev) => ({ ...prev, selectedIndex: idx }))
                      }
                    >
                      <span className="truncate">{choice.label}</span>
                    </div>
                  );
                })
              )}
            </>
          )}

          {mode.kind === "prompt" && mode.promptConfig.kind !== "choice" && (
            <div className="px-3 py-3 text-xs text-muted-foreground">
              Type a value and press Enter to confirm, or Escape to go back.
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
