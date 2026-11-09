import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import clsx from "clsx";
import {
  AtSign,
  Bookmark,
  Bot,
  Brain,
  Check,
  ChevronRight,
  ClipboardList,
  FileText,
  FolderOpen,
  GitCommit,
  Image as ImageIcon,
  Link2,
  Paperclip,
  Plus,
  Send,
  Sparkles,
  Square,
  Terminal,
  Workflow,
  X,
  Zap,
} from "lucide-react";
import { SlashCommandDropdown, SlashCommand } from "./SlashCommandDropdown";
import { ContextPicker, ContextKind, ContextPickerResult } from "./ContextPicker";
import { AtContextDropdown, AtContextItem } from "./AtContextDropdown";
import { FileMentionDropdown, FileMatch } from "./FileMentionDropdown";
import { QuickAttachBar } from "./QuickAttachBar";
import { AgentIcon, agentAccentColor } from "./AgentIcon";
import { AgentWorkflowDefinition, rpc, DirEntry } from "../../ipc";

// ── Types ───────────────────────────────────────────────────────────

export interface ChatInputAgent {
  /** Registry identifier, e.g. "claude-code". */
  type: string;
  /** Display name, e.g. "Claude Code". */
  name: string;
  available: boolean;
}

export interface ChatInputModel {
  value: string;
  label: string;
}

/** Lightweight reference to a piece of attached context. */
export interface ChatAttachment {
  id: string;
  kind: "file" | "folder" | "image" | "document" | "url" | "commit" | "terminal" | "skill" | "workflow";
  /** Filesystem path, URL, SHA, or token depending on kind. */
  value: string;
  /** Short label shown in the chip. */
  label: string;
}

export type ChatMode = "instant" | "plan";

export interface ChatInputSubmission {
  text: string;
  attachments: ChatAttachment[];
  agentType: string;
  model: string | null;
  mode: ChatMode;
}

interface Props {
  /** Current placeholder text for the textarea. */
  placeholder?: string;
  /** Whether the agent is currently streaming (disables the input, shows stop). */
  isStreaming: boolean;
  /** Called when the user submits a message. */
  onSubmit: (submission: ChatInputSubmission) => void;
  /** Called when the user clicks the stop button during streaming. */
  onStop: () => void;
  /** Agents the user can pick from. If empty, the bot menu is disabled. */
  agents: ChatInputAgent[];
  /** Currently selected agent type. */
  agentType: string;
  /** Called when the user selects a different agent. */
  onAgentChange: (agentType: string) => void;
  /** Model options for the current agent (may be empty for custom agents). */
  models?: ChatInputModel[];
  /** Currently selected model value (empty string = agent default). */
  model?: string;
  /** Called when the user selects a model. */
  onModelChange?: (model: string) => void;
  /** Available intelligence/reasoning effort levels for the current model. */
  intelligenceLevels?: string[];
  /** Currently selected intelligence level. */
  intelligence?: string;
  /** Called when the user selects an intelligence level. */
  onIntelligenceChange?: (level: string) => void;
  /** Current chat mode — plan or instant. */
  mode: ChatMode;
  /** Called when the user toggles mode. */
  onModeChange: (mode: ChatMode) => void;
  /** Slash commands to surface in the `/` dropdown. */
  slashCommands?: SlashCommand[];
  /** Called when the user selects a slash command (empty string = dismiss). */
  onSlashSelect?: (name: string) => void;
  /** Called when the user clicks "Create command" in the dropdown. */
  onCreateCommand?: (initialName?: string) => void;
  /** Called when the user picks context files (e.g. via the @ menu). */
  onPickContextFiles?: () => Promise<string[] | null>;
  /** Called when the user wants to open the Agent Manager view. */
  onOpenAgentManager?: () => void;
  /** Repository id used by the context picker for file/commit lookups. */
  repoId?: string | null;
  /** Agent workflows surfaced in the @ menu. */
  workflows?: AgentWorkflowDefinition[];
  /**
   * Task id, used by the quick-attach bar when attaching the working
   * tree diff or wiring verify-style one-shots back to the composer.
   * When absent the quick-attach "Diff vs HEAD" chip is hidden.
   */
  taskId?: string | null;
  /**
   * Absolute path of the editor file currently in focus — drives the
   * "Attach current file" quick chip.
   */
  activeEditorFile?: string | null;
  /** Display label for the active editor file (defaults to its basename). */
  activeEditorLabel?: string | null;
}

// ── Component ───────────────────────────────────────────────────────

export interface ChatInputHandle {
  focus: () => void;
  setText: (text: string) => void;
}

/**
 * Production-grade chat composer inspired by Codex / Claude Code.
 *
 * Layout:
 *
 *   ┌─────────────────────────────────────────────┐
 *   │  [textarea]                                 │
 *   │  [chips: attachments]                       │
 *   │  ┌────────────┐        ┌──────────────────┐ │
 *   │  │ Agent @ +  │        │  Model  Mode  ▶  │ │
 *   │  └────────────┘        └──────────────────┘ │
 *   └─────────────────────────────────────────────┘
 */
export const ChatInput = forwardRef<ChatInputHandle, Props>(function ChatInput(
  {
    placeholder,
    isStreaming,
    onSubmit,
    onStop,
    agents,
    agentType,
    onAgentChange,
    models = [],
    model = "",
    onModelChange,
    intelligenceLevels = [],
    intelligence = "",
    onIntelligenceChange,
    mode,
    onModeChange,
    slashCommands = [],
    onSlashSelect,
    onCreateCommand,
    onPickContextFiles,
    onOpenAgentManager,
    repoId = null,
    workflows = [],
    taskId = null,
    activeEditorFile = null,
    activeEditorLabel = null,
  },
  ref,
) {
  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<ChatAttachment[]>([]);
  const [openMenu, setOpenMenu] = useState<null | "agent" | "context" | "attach" | "model">(null);
  const [showSlash, setShowSlash] = useState(false);
  const [slashFilter, setSlashFilter] = useState("");
  /** When set, opens the ContextPicker modal with this initial tab. */
  const [pickerKind, setPickerKind] = useState<ContextKind | null>(null);

  // ── `#` file mention state ──────────────────────────────────────
  //
  // When the user types `#` at a word boundary we show a FileMention
  // dropdown over the composer. Matches come from a lazy-loaded repo
  // listing; the filter is the substring typed after `#`.
  const [fileMentionActive, setFileMentionActive] = useState(false);
  const [fileMentionQuery, setFileMentionQuery] = useState("");
  const hashTriggerRef = useRef<{ start: number; queryLen: number } | null>(null);
  const [repoEntries, setRepoEntries] = useState<DirEntry[] | null>(null);
  const [repoEntriesLoading, setRepoEntriesLoading] = useState(false);

  // ── `@` inline context dropdown state ───────────────────────────
  //
  // Typing `@` at a word boundary shows a compact inline dropdown
  // (Cursor-style) that fuzzy-searches across all context kinds.
  const [showAt, setShowAt] = useState(false);
  const [atFilter, setAtFilter] = useState("");
  /**
   * Position of the active `@` trigger in the composer. When non-null,
   * the range `[triggerStart, triggerStart + '@'.length + query.length]`
   * gets stripped on pick/dismiss so the `@foo` fragment doesn't end up
   * in the submitted message.
   */
  const atTriggerRef = useRef<{ start: number; queryLen: number } | null>(null);

  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const attachInputRef = useRef<HTMLInputElement>(null);
  const imageInputRef = useRef<HTMLInputElement>(null);
  const docInputRef = useRef<HTMLInputElement>(null);

  useLayoutEffect(() => {
    const composer = containerRef.current;
    const content = composer?.closest<HTMLElement>("[data-chat-content]");
    if (!composer || !content) return;
    // Inline menus scroll within the chat's available space; full dialogs
    // render at the document layer so they can cover navigation normally.
    const measure = () => {
      const height = Math.max(48, composer.getBoundingClientRect().top - content.getBoundingClientRect().top - 8);
      composer.style.setProperty("--composer-popup-height", `${height}px`);
    };
    const observer = new ResizeObserver(measure);
    observer.observe(composer);
    observer.observe(content);
    window.addEventListener("resize", measure);
    measure();
    return () => { observer.disconnect(); window.removeEventListener("resize", measure); };
  }, []);

  useImperativeHandle(ref, () => ({
    focus: () => textareaRef.current?.focus(),
    setText: (value: string) => setText(value),
  }));

  // Close popover menus on outside click.
  useEffect(() => {
    if (!openMenu) return;
    const handler = (e: MouseEvent) => {
      if (!containerRef.current?.contains(e.target as Node)) {
        setOpenMenu(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [openMenu]);

  // Auto-grow textarea up to ~8 rows.
  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    const max = 8 * 20 + 16; // ~8 rows @ line-height 20px + padding
    el.style.height = Math.min(el.scrollHeight, max) + "px";
  }, [text]);

  // Lazily load the repo file list the first time the user types `#`.
  // Keeping the fetch deferred means ChatInput's mount stays cheap even
  // on large repos; the user only pays the cost when they reach for the
  // feature. We refresh if the repo changes.
  useEffect(() => {
    if (!repoId || repoEntries !== null || repoEntriesLoading) return;
    if (!fileMentionActive) return;
    let cancelled = false;
    setRepoEntriesLoading(true);
    rpc.list(repoId, {})
      .then((result) => {
        if (!cancelled) setRepoEntries(result.entries);
      })
      .catch(() => {
        if (!cancelled) setRepoEntries([]);
      })
      .finally(() => {
        if (!cancelled) setRepoEntriesLoading(false);
      });
    return () => { cancelled = true; };
  }, [repoId, repoEntries, repoEntriesLoading, fileMentionActive]);

  // Invalidate cached entries when the repo changes.
  useEffect(() => {
    setRepoEntries(null);
  }, [repoId]);

  /** Memoized matches for the file mention dropdown. */
  const fileMatches = useMemo<FileMatch[]>(() => {
    if (!fileMentionActive) return [];
    const pool = (repoEntries ?? []).filter(
      (e) => e.kind !== "dir" && e.kind !== "submodule",
    );
    const query = fileMentionQuery.trim().toLowerCase();
    const scored = pool.map((e) => ({ entry: e, score: scoreFileMatch(e.path, query) }));
    const ranked = scored
      .filter((s) => s.score > 0 || query === "")
      .sort((a, b) => b.score - a.score)
      .slice(0, 40)
      .map((s) => ({
        path: s.entry.path,
        label: s.entry.path.split("/").pop() ?? s.entry.path,
      }));
    return ranked;
  }, [fileMentionActive, fileMentionQuery, repoEntries]);

  const selectedAgent = useMemo(
    () => agents.find((a) => a.type === agentType) ?? null,
    [agents, agentType],
  );

  const selectedModel = useMemo(
    () => models.find((m) => m.value === model) ?? null,
    [models, model],
  );

  // ── Submit ─────────────────────────────────────────────────────

  const handleSubmit = useCallback(() => {
    const trimmed = text.trim();
    if (!trimmed || isStreaming) return;
    onSubmit({
      text: trimmed,
      attachments,
      agentType,
      model: model || null,
      mode,
    });
    setText("");
    setAttachments([]);
    setShowSlash(false);
  }, [text, isStreaming, onSubmit, attachments, agentType, model, mode]);

  const handleTextareaChange = useCallback(
    (e: React.ChangeEvent<HTMLTextAreaElement>) => {
      const value = e.target.value;
      const caret = e.target.selectionStart ?? value.length;
      setText(value);

      // ── Slash commands ─────────────────────────────────────────
      if (value.startsWith("/") && value.length > 0) {
        const afterSlash = value.slice(1).split(/\s/)[0];
        setSlashFilter(afterSlash);
        setShowSlash(true);
      } else {
        setShowSlash(false);
        setSlashFilter("");
      }

      // ── `#` file mention ───────────────────────────────────────
      //
      // Mirrors the `@` logic but is narrower — `#` only ever resolves
      // to a repo file. The dropdown is inline over the composer, so
      // users don't have to open a modal just to drop in a path.
      const hashIdx = findActiveTriggerIndex(value, caret, "#");
      if (hashIdx !== -1 && repoId) {
        const frag = value.slice(hashIdx + 1, caret);
        hashTriggerRef.current = { start: hashIdx, queryLen: frag.length };
        setFileMentionQuery(frag);
        setFileMentionActive(true);
      } else if (fileMentionActive) {
        hashTriggerRef.current = null;
        setFileMentionActive(false);
        setFileMentionQuery("");
      }

      // ── `@` inline context trigger ─────────────────────────────
      //
      // Scan backwards from the caret for the closest `@` at a word
      // boundary. Show the inline dropdown and keep the filter in sync
      // as the user types.
      const atIdx = findActiveAtIndex(value, caret);
      if (atIdx !== -1) {
        const frag = value.slice(atIdx + 1, caret);
        atTriggerRef.current = { start: atIdx, queryLen: frag.length };
        setAtFilter(frag);
        setShowAt(true);
      } else if (atTriggerRef.current !== null) {
        // User deleted past the `@` or typed whitespace — tear down.
        atTriggerRef.current = null;
        setAtFilter("");
        setShowAt(false);
      }
    },
    [repoId, fileMentionActive],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      // Slash / file-mention / @ dropdowns consume keys while open.
      if (showSlash) return;
      if (fileMentionActive) return;
      if (showAt) return;

      // Cmd/Ctrl+Enter always submits.
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        handleSubmit();
        return;
      }
      // Plain Enter submits; Shift+Enter inserts a newline.
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        handleSubmit();
      }
    },
    [handleSubmit, showSlash, fileMentionActive, showAt],
  );

  const handleSlashSelectInner = useCallback(
    (name: string) => {
      setShowSlash(false);
      if (!name) return;
      if (onSlashSelect) {
        onSlashSelect(name);
        setText("");
      } else {
        setText(`/${name} `);
      }
    },
    [onSlashSelect],
  );

  // ── File mention selection ──────────────────────────────────────
  //
  // Empty path = user dismissed (Escape). Otherwise strip the `#query`
  // from the textarea and add a file attachment. Focus stays on the
  // textarea so the user can keep typing.
  const handleFileMentionSelect = useCallback(
    (picked: string) => {
      const trigger = hashTriggerRef.current;
      setFileMentionActive(false);
      setFileMentionQuery("");
      hashTriggerRef.current = null;

      if (!picked) return;

      setText((prev) => {
        if (!trigger) return prev;
        const { start, queryLen } = trigger;
        return prev.slice(0, start) + prev.slice(start + 1 + queryLen);
      });

      const basename = picked.split("/").pop() ?? picked;
      setAttachments((prev) => {
        const dup = prev.some((a) => a.kind === "file" && a.value === picked);
        if (dup) return prev;
        return [
          ...prev,
          {
            id: `mention-${Date.now()}`,
            kind: "file",
            value: picked,
            label: basename,
          },
        ];
      });
      queueMicrotask(() => textareaRef.current?.focus());
    },
    [],
  );

  // ── Attachments ────────────────────────────────────────────────

  const addAttachment = useCallback((a: ChatAttachment) => {
    setAttachments((prev) => {
      if (prev.some((p) => p.value === a.value && p.kind === a.kind)) return prev;
      return [...prev, a];
    });
  }, []);

  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  }, []);

  const handlePickerResult = useCallback((result: ContextPickerResult) => {
    if (result.attachment) {
      addAttachment(result.attachment);
    }
    if (result.textToInsert) {
      setText((prev) => {
        const trimmed = prev.trimEnd();
        if (!trimmed) return result.textToInsert!;
        return `${trimmed}\n\n${result.textToInsert}`;
      });
      queueMicrotask(() => textareaRef.current?.focus());
    }
  }, [addAttachment]);

  const handlePickerClose = useCallback(() => {
    setPickerKind(null);
    queueMicrotask(() => textareaRef.current?.focus());
  }, []);

  // ── Inline `@` dropdown selection ──────────────────────────────
  const handleAtSelect = useCallback((item: AtContextItem | null) => {
    // Strip the `@query` fragment from the composer.
    if (atTriggerRef.current) {
      const { start, queryLen } = atTriggerRef.current;
      setText((prev) => prev.slice(0, start) + prev.slice(start + 1 + queryLen));
      atTriggerRef.current = null;
    }
    setShowAt(false);
    setAtFilter("");

    if (!item) return; // Escape — dismiss without picking.

    // Convert the picked item into an attachment or text insertion.
    const now = Date.now();
    if (item.kind === "instruction") {
      // Instructions insert their body text into the composer.
      setText((prev) => {
        const trimmed = prev.trimEnd();
        if (!trimmed) return item.value;
        return `${trimmed}\n\n${item.value}`;
      });
    } else if (item.kind === "url") {
      const label = (() => {
        try { return new URL(item.value).hostname.replace(/^www\./, ""); } catch { return item.value; }
      })();
      addAttachment({ id: `url-${now}`, kind: "url", value: item.value, label });
    } else {
      addAttachment({
        id: `${item.kind}-${now}`,
        kind: item.kind as ChatAttachment["kind"],
        value: item.value,
        label: item.label,
      });
    }
    queueMicrotask(() => textareaRef.current?.focus());
  }, [addAttachment]);

  const addFilesFromPicker = useCallback(async (intent: "file" | "image" | "document") => {
    if (!onPickContextFiles) return;
    try {
      const picked = await onPickContextFiles();
      if (!picked || picked.length === 0) return;
      const now = Date.now();
      picked.forEach((p, i) => {
        const name = p.split("/").pop() ?? p;
        const ext = (name.includes(".") ? name.slice(name.lastIndexOf(".") + 1) : "").toLowerCase();
        const kind: ChatAttachment["kind"] =
          intent === "image" || ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)
            ? "image"
            : intent === "document" || ["pdf", "docx", "doc", "xlsx", "pptx", "md", "txt"].includes(ext)
              ? "document"
              : "file";
        addAttachment({
          id: `${kind}-${now}-${i}`,
          kind,
          value: p,
          label: name,
        });
      });
    } catch {
      // picker cancelled or unavailable
    }
  }, [onPickContextFiles, addAttachment]);

  // Handle drop for files/images.
  const handleDrop = useCallback((e: React.DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    const now = Date.now();
    const getPath = (window as any).crab?.getPathForFile as ((f: File) => string) | undefined;
    Array.from(e.dataTransfer.files ?? []).forEach((f, i) => {
      const isImage = f.type.startsWith("image/");
      // Electron 32+ removed `File.path`. Use the preload-exposed
      // webUtils.getPathForFile bridge; fall back to the legacy field for
      // older runtimes and to the bare filename as a last resort so the
      // chip still shows something useful.
      const resolved = getPath?.(f) || (f as any).path || f.name;
      addAttachment({
        id: `drop-${now}-${i}`,
        kind: isImage ? "image" : "file",
        value: resolved,
        label: f.name,
      });
    });
  }, [addAttachment]);

  // ── Render ─────────────────────────────────────────────────────

  const canSubmit = text.trim().length > 0 && !isStreaming;

  return (
    <div
      ref={containerRef}
      onBlur={(event) => {
        if (event.currentTarget.contains(event.relatedTarget)) return;
        // These popovers listen on document; hidden chat must not consume Skills input.
        setShowSlash(false);
        setShowAt(false);
        setFileMentionActive(false);
      }}
      onDrop={handleDrop}
      onDragOver={(e) => e.preventDefault()}
      className="relative isolate rounded-lg border border-gx-border bg-gx-bg focus-within:border-gx-accent/70 transition"
    >
      {/* Slash command dropdown (positioned above the input group) */}
      <SlashCommandDropdown
        commands={slashCommands}
        filter={slashFilter}
        onSelect={handleSlashSelectInner}
        visible={showSlash}
        onCreateCommand={onCreateCommand}
      />

      {/* Inline @ context dropdown (Cursor-style typeahead). */}
      <AtContextDropdown
        filter={atFilter}
        visible={showAt}
        repoId={repoId}
        workflows={workflows}
        onSelect={handleAtSelect}
        onCategorySelect={(kind) => {
          // Dismiss the @ dropdown and strip the trigger text.
          if (atTriggerRef.current) {
            const { start, queryLen } = atTriggerRef.current;
            setText((prev) => prev.slice(0, start) + prev.slice(start + 1 + queryLen));
            atTriggerRef.current = null;
          }
          setShowAt(false);
          setAtFilter("");
          // Open the full context picker modal on the corresponding tab.
          setPickerKind(kind as ContextKind);
        }}
      />

      {/* File mention dropdown (`#file.ts` typeahead). */}
      <FileMentionDropdown
        matches={fileMatches}
        loading={repoEntriesLoading}
        onSelect={handleFileMentionSelect}
        visible={fileMentionActive}
      />

      {/* Quick attach chips — current file, diff, terminal, last commit, clipboard */}
      <QuickAttachBar
        taskId={taskId ?? undefined}
        activeEditorFile={activeEditorFile}
        activeEditorLabel={activeEditorLabel}
        repoId={repoId}
        onAttach={addAttachment}
      />

      {/* Textarea */}
      <textarea
        ref={textareaRef}
        className="block w-full resize-none bg-transparent px-3 pt-2.5 pb-1 text-xs leading-5 text-gx-text placeholder:text-gx-muted/60 focus:outline-none"
        placeholder={placeholder ?? "Type a message, / for commands, @ to add context, # for files…"}
        rows={1}
        value={text}
        onChange={handleTextareaChange}
        onKeyDown={handleKeyDown}
        disabled={isStreaming}
        aria-label="Chat message input"
      />

      {/* Attachment chips */}
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-1.5 px-2.5 pb-1.5">
          {attachments.map((a) => (
            <AttachmentChip key={a.id} attachment={a} onRemove={() => removeAttachment(a.id)} />
          ))}
        </div>
      )}

      {/* Toolbar */}
      <div className="flex items-center gap-1 px-2 pb-2 pt-1">
        {/* Left cluster: agent, @, + */}
        <IconButton
          ariaLabel="Select AI agent"
          title={selectedAgent ? `Agent: ${selectedAgent.name}` : "Select agent"}
          active={openMenu === "agent"}
          onClick={() => setOpenMenu(openMenu === "agent" ? null : "agent")}
          disabled={agents.length === 0}
        >
          {selectedAgent ? (
            <AgentIcon
              agentName={selectedAgent.type}
              size={13}
              className={agentAccentColor(selectedAgent.type)}
            />
          ) : (
            <Bot size={13} />
          )}
          {selectedAgent && (
            <span className="text-[10px] font-medium text-gx-text/90">{shortAgentLabel(selectedAgent.name)}</span>
          )}
        </IconButton>

        <IconButton
          ariaLabel="Add context"
          title="Add context (files, URLs, commits, skills)"
          active={openMenu === "context"}
          onClick={() => setOpenMenu(openMenu === "context" ? null : "context")}
        >
          <AtSign size={13} />
        </IconButton>

        <IconButton
          ariaLabel="Attach file"
          title="Attach files, images, or documents"
          active={openMenu === "attach"}
          onClick={() => setOpenMenu(openMenu === "attach" ? null : "attach")}
        >
          <Plus size={13} />
        </IconButton>

        {/* Spacer */}
        <div className="flex-1" />

        {/* Right cluster: model, mode, send */}
        {models.length > 0 && (
          <IconButton
            ariaLabel="Select model and intelligence"
            title={selectedModel ? `Model: ${selectedModel.label}${intelligence ? ` · ${intelligence}` : ""}` : "Model"}
            active={openMenu === "model"}
            onClick={() => setOpenMenu(openMenu === "model" ? null : "model")}
          >
            <Sparkles size={12} className="text-gx-accent" />
            <span className="text-[10px] font-medium">
              {selectedModel ? selectedModel.label : "Default"}
              {intelligence && <span className="ml-1 text-gx-muted">{intelligence.charAt(0).toUpperCase() + intelligence.slice(1).replace("-", " ")}</span>}
            </span>
          </IconButton>
        )}

        <button
          type="button"
          onClick={() => onModeChange(mode === "plan" ? "instant" : "plan")}
          aria-pressed={mode === "plan"}
          title={
            mode === "plan"
              ? "Plan mode — agent proposes a plan before making changes"
              : "Instant mode — agent acts immediately"
          }
          className={clsx(
            "flex h-7 items-center gap-1 rounded-md border px-2 text-[10px] font-medium transition",
            mode === "plan"
              ? "border-gx-warn/50 bg-gx-warn/10 text-gx-warn hover:bg-gx-warn/15"
              : "border-gx-border bg-gx-surface text-gx-muted hover:text-gx-text",
          )}
        >
          {mode === "plan" ? <ClipboardList size={11} /> : <Zap size={11} />}
          {mode === "plan" ? "Plan" : "Instant"}
        </button>

        {isStreaming ? (
          <button
            type="button"
            onClick={onStop}
            className="ml-1 flex h-7 w-7 items-center justify-center rounded-md bg-gx-error/20 text-gx-error hover:bg-gx-error/30 transition"
            aria-label="Stop agent"
          >
            <Square size={13} />
          </button>
        ) : (
          <button
            type="button"
            onClick={handleSubmit}
            disabled={!canSubmit}
            className={clsx(
              "ml-1 flex h-7 w-7 items-center justify-center rounded-md transition",
              canSubmit
                ? "bg-gx-accent text-white hover:bg-gx-accent-hover"
                : "bg-gx-surface text-gx-muted cursor-not-allowed",
            )}
            aria-label="Send message"
          >
            <Send size={13} />
          </button>
        )}
      </div>

      {/* Popovers ───────────────────────────────────────────────── */}
      {openMenu === "agent" && (
        <PopoverMenu anchor="left" onClose={() => setOpenMenu(null)}>
          <MenuHeader label="Agent" />
          {agents.filter((a) => a.available).map((a) => (
            <MenuItem
              key={a.type}
              active={a.type === agentType}
              onClick={() => {
                onAgentChange(a.type);
                setOpenMenu(null);
              }}
              icon={<AgentIcon agentName={a.type} size={12} className={agentAccentColor(a.type)} />}
              label={a.name}
            />
          ))}
          {agents.filter((a) => a.available).length === 0 && (
            <div className="px-3 py-2 text-[10px] text-gx-muted">No agents installed.</div>
          )}
          {/* Divider + Add Agent shortcut */}
          <div className="my-1 border-t border-gx-border/50" />
          <MenuItem
            icon={<Plus size={12} className="text-gx-muted" />}
            label="Add Agent…"
            onClick={() => {
              setOpenMenu(null);
              onOpenAgentManager?.();
            }}
          />
        </PopoverMenu>
      )}

      {openMenu === "context" && (
        <PopoverMenu anchor="left" onClose={() => setOpenMenu(null)}>
          <MenuHeader label="Add context" />
          <MenuItem
            icon={<Bookmark size={12} />}
            label="Instructions"
            hint="Saved prompts"
            onClick={() => { setOpenMenu(null); setPickerKind("instruction"); }}
          />
          <MenuItem
            icon={<FileText size={12} />}
            label="Files"
            hint="Search & select"
            onClick={() => { setOpenMenu(null); setPickerKind("file"); }}
          />
          <MenuItem
            icon={<FolderOpen size={12} />}
            label="Folders"
            hint="Search & select"
            onClick={() => { setOpenMenu(null); setPickerKind("folder"); }}
          />
          <MenuItem
            icon={<Link2 size={12} />}
            label="URL"
            hint="Paste link"
            onClick={() => { setOpenMenu(null); setPickerKind("url"); }}
          />
          <MenuItem
            icon={<GitCommit size={12} />}
            label="Commit"
            hint="Search history"
            onClick={() => { setOpenMenu(null); setPickerKind("commit"); }}
          />
          <MenuItem
            icon={<Terminal size={12} />}
            label="Terminal output"
            hint="From sessions"
            onClick={() => { setOpenMenu(null); setPickerKind("terminal"); }}
          />
          <MenuItem
            icon={<Brain size={12} />}
            label="Skills"
            hint="Install & manage"
            onClick={() => { setOpenMenu(null); setPickerKind("skill"); }}
          />
        </PopoverMenu>
      )}

      {openMenu === "attach" && (
        <PopoverMenu anchor="left" onClose={() => setOpenMenu(null)}>
          <MenuHeader label="Attach" />
          <MenuItem
            icon={<FolderOpen size={12} />}
            label="File"
            hint="From workspace"
            onClick={() => {
              setOpenMenu(null);
              addFilesFromPicker("file");
            }}
          />
          <MenuItem
            icon={<ImageIcon size={12} />}
            label="Image"
            hint="PNG, JPG, SVG"
            onClick={() => {
              setOpenMenu(null);
              imageInputRef.current?.click();
            }}
          />
          <MenuItem
            icon={<FileText size={12} />}
            label="Document"
            hint="PDF, DOCX, MD"
            onClick={() => {
              setOpenMenu(null);
              docInputRef.current?.click();
            }}
          />
        </PopoverMenu>
      )}

      {openMenu === "model" && models.length > 0 && (
        <ModelIntelligencePopover
          models={models}
          selectedModel={model}
          onModelChange={(v) => { onModelChange?.(v); setOpenMenu(null); }}
          intelligenceLevels={intelligenceLevels}
          intelligence={intelligence}
          onIntelligenceChange={onIntelligenceChange}
          onClose={() => setOpenMenu(null)}
        />
      )}

      {/* Hidden native file inputs for image / document attachments */}
      <input
        ref={attachInputRef}
        type="file"
        multiple
        hidden
        onChange={(e) => handleNativeFiles(e.target.files, addAttachment, "file")}
      />
      <input
        ref={imageInputRef}
        type="file"
        multiple
        accept="image/*"
        hidden
        onChange={(e) => handleNativeFiles(e.target.files, addAttachment, "image")}
      />
      <input
        ref={docInputRef}
        type="file"
        multiple
        accept=".pdf,.doc,.docx,.md,.txt,.rtf,.xlsx,.pptx"
        hidden
        onChange={(e) => handleNativeFiles(e.target.files, addAttachment, "document")}
      />

      {/* Unified context picker modal (opened via toolbar @ button) */}
      <ContextPicker
        open={pickerKind !== null}
        initialKind={pickerKind ?? "instruction"}
        repoId={repoId}
        onClose={handlePickerClose}
        onPick={handlePickerResult}
      />
    </div>
  );
});

// ── Helpers ─────────────────────────────────────────────────────────

function handleNativeFiles(
  fileList: FileList | null,
  addAttachment: (a: ChatAttachment) => void,
  kind: "file" | "image" | "document",
) {
  if (!fileList) return;
  const now = Date.now();
  const getPath = (window as any).crab?.getPathForFile as ((f: File) => string) | undefined;
  Array.from(fileList).forEach((f, i) => {
    const resolved = getPath?.(f) || (f as any).path || f.name;
    addAttachment({
      id: `native-${now}-${i}`,
      kind: kind === "image" ? "image" : kind === "document" ? "document" : "file",
      value: resolved,
      label: f.name,
    });
  });
}

/**
 * Return the index of the `@` that terminates an active mention trigger,
 * or `-1` if the caret is not currently inside one.
 *
 * A trigger is active when:
 *   1. There's an `@` somewhere before the caret.
 *   2. The character immediately before that `@` is whitespace or the
 *      start of the buffer (so `user@example.com` isn't misread).
 *   3. Every character between the `@` and the caret is a non-whitespace
 *      identifier-ish character (letters, digits, `.`, `-`, `_`, `/`).
 *      This matches file paths, URLs, commit SHAs, skill names, etc.
 */
function findActiveAtIndex(value: string, caret: number): number {
  return findActiveTriggerIndex(value, caret, "@");
}

/**
 * Generalised trigger lookup — returns the index of an active `char`
 * trigger (e.g. `@`, `#`) closest to the caret, or -1 if none is
 * currently being typed. See findActiveAtIndex for the rules.
 */
function findActiveTriggerIndex(value: string, caret: number, char: string): number {
  for (let i = caret - 1; i >= 0; i--) {
    const ch = value[i];
    if (ch === char) {
      const prev = i === 0 ? " " : value[i - 1];
      return /\s/.test(prev) ? i : -1;
    }
    if (/\s/.test(ch)) return -1;
  }
  return -1;
}

/**
 * Lightweight fuzzy-ish score for file-mention matches. Exact basename
 * matches rank highest, prefix matches beat substring matches, and
 * shorter paths beat long ones at the same tier.
 */
function scoreFileMatch(path: string, query: string): number {
  if (!query) return 1; // Any non-filtered entry is eligible.
  const lower = path.toLowerCase();
  const base = (path.split("/").pop() ?? path).toLowerCase();
  const q = query.toLowerCase();

  if (base === q) return 1000;
  if (base.startsWith(q)) return 500 - Math.min(path.length, 100);
  if (base.includes(q)) return 200 - Math.min(path.length, 100);
  if (lower.includes(q)) return 80 - Math.min(path.length, 100);
  return 0;
}

/** Condense "Claude Code" → "Claude", "Codex" → "Codex" for compact display. */
function shortAgentLabel(name: string): string {
  const first = name.trim().split(/\s+/)[0];
  return first.length > 10 ? first.slice(0, 10) + "…" : first;
}

// ── Sub-components ──────────────────────────────────────────────────

function IconButton({
  children,
  onClick,
  ariaLabel,
  title,
  active,
  disabled,
}: {
  children: React.ReactNode;
  onClick: () => void;
  ariaLabel: string;
  title?: string;
  active?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={ariaLabel}
      title={title}
      disabled={disabled}
      className={clsx(
        "flex h-7 items-center gap-1 rounded-md border px-2 text-gx-muted transition",
        active
          ? "border-gx-accent/60 bg-gx-accent/10 text-gx-text"
          : "border-transparent hover:border-gx-border hover:bg-gx-surface hover:text-gx-text",
        disabled && "opacity-40 cursor-not-allowed hover:border-transparent hover:bg-transparent",
      )}
    >
      {children}
    </button>
  );
}

function PopoverMenu({
  anchor,
  onClose: _onClose,
  children,
}: {
  anchor: "left" | "right";
  onClose: () => void;
  children: React.ReactNode;
}) {
  return (
    <div
      role="menu"
      className={clsx(
        "absolute bottom-full z-40 mb-2 max-h-[var(--composer-popup-height)] min-w-[200px] overflow-auto rounded-lg border border-gx-border bg-gx-bg shadow-xl",
        anchor === "left" ? "left-2" : "right-2",
      )}
    >
      {children}
    </div>
  );
}

function MenuHeader({ label }: { label: string }) {
  return (
    <div className="px-3 py-1.5 border-b border-gx-border text-[10px] font-semibold uppercase tracking-wider text-gx-muted">
      {label}
    </div>
  );
}

function MenuItem({
  icon,
  label,
  hint,
  active,
  disabled,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  active?: boolean;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      role="menuitem"
      className={clsx(
        "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition",
        disabled
          ? "cursor-not-allowed text-gx-muted/60"
          : active
            ? "bg-gx-accent/10 text-gx-text"
            : "text-gx-text hover:bg-gx-surface-hover",
      )}
    >
      <span className="shrink-0 text-gx-accent">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {hint && <span className="shrink-0 text-[10px] text-gx-muted">{hint}</span>}
      {active && <Check size={11} className="shrink-0 text-gx-accent" />}
    </button>
  );
}

function AttachmentChip({
  attachment,
  onRemove,
}: {
  attachment: ChatAttachment;
  onRemove: () => void;
}) {
  const Icon = iconForAttachment(attachment.kind);
  return (
    <span className="inline-flex items-center gap-1 rounded border border-gx-border bg-gx-surface px-1.5 py-0.5 text-[10px] text-gx-text">
      <Icon size={10} className="text-gx-accent" />
      <span className="max-w-[160px] truncate">{attachment.label}</span>
      <button
        type="button"
        onClick={onRemove}
        className="ml-0.5 text-gx-muted hover:text-gx-text"
        aria-label={`Remove ${attachment.label}`}
      >
        <X size={10} />
      </button>
    </span>
  );
}

function iconForAttachment(kind: ChatAttachment["kind"]) {
  switch (kind) {
    case "image": return ImageIcon;
    case "document": return FileText;
    case "folder": return FolderOpen;
    case "url": return Link2;
    case "commit": return GitCommit;
    case "terminal": return Terminal;
    case "skill": return Brain;
    case "workflow": return Workflow;
    case "file":
    default:
      return Paperclip;
  }
}

// ── Model + Intelligence Popover ────────────────────────────────────

/** Max primary models shown before "More models" submenu. */
const MAX_PRIMARY_MODELS = 2;

function ModelIntelligencePopover({
  models,
  selectedModel,
  onModelChange,
  intelligenceLevels,
  intelligence,
  onIntelligenceChange,
  onClose: _onClose,
}: {
  models: ChatInputModel[];
  selectedModel: string;
  onModelChange: (value: string) => void;
  intelligenceLevels: string[];
  intelligence: string;
  onIntelligenceChange?: (level: string) => void;
  onClose: () => void;
}) {
  const [showMore, setShowMore] = useState(false);

  const primaryModels = models.slice(0, MAX_PRIMARY_MODELS);
  const otherModels = models.slice(MAX_PRIMARY_MODELS);

  return (
    <div
      role="menu"
      className="absolute bottom-full right-2 z-40 mb-2 min-w-[180px] rounded-lg border border-gx-border bg-gx-bg shadow-xl"
    >
      {/* Intelligence section */}
      {intelligenceLevels.length > 0 && (
        <>
          <div className="px-3 py-1 text-[9px] font-semibold uppercase tracking-wider text-gx-muted">
            Intelligence
          </div>
          {intelligenceLevels.map((level) => (
            <button
              key={level}
              type="button"
              onClick={() => onIntelligenceChange?.(level)}
              className={clsx(
                "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
                intelligence === level
                  ? "text-gx-accent"
                  : "text-gx-text hover:bg-gx-surface-hover",
              )}
            >
              <span className="flex-1">{level.charAt(0).toUpperCase() + level.slice(1).replace("-", " ")}</span>
              {intelligence === level && <Check size={11} className="shrink-0" />}
            </button>
          ))}
          <div className="my-1 border-t border-gx-border/50" />
        </>
      )}

      {/* Model section */}
      <div className="px-3 py-1 text-[9px] font-semibold uppercase tracking-wider text-gx-muted">
        Model
      </div>
      <button
        type="button"
        onClick={() => onModelChange("")}
        className={clsx(
          "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
          !selectedModel ? "text-gx-accent" : "text-gx-text hover:bg-gx-surface-hover",
        )}
      >
        <span className="flex-1">Agent default</span>
        {!selectedModel && <Check size={11} className="shrink-0" />}
      </button>
      {primaryModels.map((m) => (
        <button
          key={m.value}
          type="button"
          onClick={() => onModelChange(m.value)}
          className={clsx(
            "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
            selectedModel === m.value ? "text-gx-accent" : "text-gx-text hover:bg-gx-surface-hover",
          )}
        >
          <span className="flex-1">{m.label}</span>
          {selectedModel === m.value && <Check size={11} className="shrink-0" />}
        </button>
      ))}

      {/* More models submenu */}
      {otherModels.length > 0 && (
        <div
          className="relative"
          onMouseEnter={() => setShowMore(true)}
          onMouseLeave={() => setShowMore(false)}
        >
          <button
            type="button"
            onClick={() => setShowMore(!showMore)}
            className="flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover transition"
          >
            <span className="flex-1">More models</span>
            <ChevronRight size={11} className="shrink-0 text-gx-muted" />
          </button>

          {showMore && (
            <div className="absolute right-full top-0 z-50 mr-1 min-w-[150px] rounded-lg border border-gx-border bg-gx-bg shadow-lg">
              <div className="py-1">
                {otherModels.map((m) => (
                  <button
                    key={m.value}
                    type="button"
                    onClick={() => onModelChange(m.value)}
                    className={clsx(
                      "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
                      selectedModel === m.value ? "text-gx-accent" : "text-gx-text hover:bg-gx-surface-hover",
                    )}
                  >
                    <span className="flex-1">{m.label}</span>
                    {selectedModel === m.value && <Check size={11} className="shrink-0" />}
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
