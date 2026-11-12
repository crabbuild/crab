import { Button } from "@/components/ui/button";
import {
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  Command as CommandRoot,
} from "@/components/ui/command";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import { ChevronLeft } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  Command,
  CommandContext,
  CommandResult,
  PromptConfig,
} from "../lib/command-registry";
import {
  executeCommand,
  filterCommands,
  useCommands,
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

  // Subscribe to registry changes so the list updates if commands are registered/unregistered.
  useCommands();

  // Reset command search when the palette opens.
  useEffect(() => {
    if (open) {
      setMode({ kind: "commands", query: "", selectedIndex: 0 });
      requestAnimationFrame(() => inputRef.current?.focus());
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
    (event: React.KeyboardEvent) => {
      if (
        event.key === "Enter" &&
        mode.kind === "prompt" && mode.promptConfig.kind !== "choice") {
        event.preventDefault();
        if (mode.query.trim()) submitPrompt(mode.query.trim());
      }
    },
    [mode, submitPrompt],
  );

  const handleQueryChange = useCallback((value: string) => {
    setMode((prev) => ({ ...prev, query: value, selectedIndex: 0 }));
  }, []);

  if (!open) return null;

  const placeholder =
    mode.kind === "prompt" ? mode.promptConfig.placeholder : "Run command…";

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) handleClose();
      }}
    >
      {/* Backdrop */}

      {/* Palette container */}
      <DialogContent
        onEscapeKeyDown={(event) => {
          if (mode.kind === "prompt") {
            event.preventDefault();
            setMode({ kind: "commands", query: "", selectedIndex: 0 });
          }
        }}
        showCloseButton={false}
        aria-describedby={undefined}
        className={
          "flex w-full max-w-lg flex-col overflow-hidden rounded-lg border border-border bg-card shadow-2xl" +
          " p-0 gap-0"
        }
      >
        <DialogTitle className="sr-only">Command palette</DialogTitle>
        <CommandRoot
          shouldFilter={false}
          value={String(mode.selectedIndex)}
          onValueChange={(value) =>
            setMode((prev) => ({ ...prev, selectedIndex: Number(value) }))}
        onKeyDown={handleKeyDown}
        >
        {/* Search input */}
        <div className="flex items-center gap-2 border-b border-border px-3 py-2">
          {mode.kind === "prompt" && (
            <Button
                variant="ghost"
                size="compact"
                type="button"
              className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
              onClick={() => setMode({ kind: "commands", query: "", selectedIndex: 0 })}
              aria-label="Back to commands"
            >
              <ChevronLeft size={16} />
            </Button>
            )}

            <CommandInput
              ref={inputRef}
              className="flex-1 bg-transparent text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-none"
            placeholder={placeholder}
            value={mode.query}
              onValueChange={(value) => handleQueryChange(value)}
            aria-label={placeholder}
            />
        </div>

        {/* Results list */}
        <CommandList
            ref={listRef}
          id="palette-results"
          className="max-h-[320px] overflow-y-auto py-1"
          >
            <CommandGroup>
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
                    <CommandItem
                          value={String(idx)}
                          key={cmd.id}
                          data-idx={idx}
                          disabled={isDisabled}
                      className={clsx(
                        "flex cursor-pointer items-center gap-2 px-3 py-1.5 text-sm",
                        isDisabled && "opacity-50 cursor-default",
                        isSelected
                          ? "bg-primary/15 text-foreground"
                          : "text-foreground/80 hover:bg-accent",
                      )}
                          onSelect={() => {
                        if (!isDisabled) selectCommand(cmd);
                      }}
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
                    </CommandItem>
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
                    <CommandItem
                            value={String(idx)}
                            key={choice.value}
                            data-idx={idx}
                      className={clsx(
                        "flex cursor-pointer items-center gap-2 px-3 py-1.5 text-sm",
                        isSelected
                          ? "bg-primary/15 text-foreground"
                          : "text-foreground/80 hover:bg-accent",
                      )}
                            onSelect={() => submitPrompt(choice.value)
                      }
                    >
                      <span className="truncate">{choice.label}</span>
                    </CommandItem>
                  );
                })
              )}
            </>
          )}

          {mode.kind === "prompt" && mode.promptConfig.kind !== "choice" && (
            <div className="px-3 py-3 text-xs text-muted-foreground">
                    Type a value and press Enter to confirm, or Escape to go
                    back.
                  </div>
          )}
        </CommandGroup>
          </CommandList>
        </CommandRoot>
      </DialogContent>
    </Dialog>
  );
}
