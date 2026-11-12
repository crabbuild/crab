import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import clsx from "clsx";
import {
  AlertCircle,
  Bot,
  Brain,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  FilePen,
  FileText,
  Filter,
  MessageSquare,
  Search,
  Terminal,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface AgentEvent {
  type:
    | "text"
    | "tool_use"
    | "tool_result"
    | "file_edit"
    | "command_run"
    | "thinking"
    | "error"
    | "done";
  timestamp: number;
  content?: string;
  tool?: string;
  input?: unknown;
  output?: unknown;
  path?: string;
  diff?: string;
  command?: string;
  stdout?: string;
  stderr?: string;
  exitCode?: number;
  error?: string;
  /** Agent ID for multi-agent workspaces. */
  agentId?: string;
  /** Agent role label for multi-agent workspaces. */
  agentRole?: string;
}

export type EventFilter = "all" | "files" | "commands" | "errors";

/** Colour palette for multi-agent feeds.
 *  Semantic tokens where possible; fall back to 600-scale so labels
 *  remain AA-legible on light themes while staying distinguishable. */
const AGENT_COLORS = [
  "text-info",
  "text-thinking",
  "text-success text-success",
  "text-warning text-warning",
  "text-thinking text-thinking",
];

function getAgentColor(agentId: string | undefined, agentMap: Map<string, number>): string {
  if (!agentId) return "";
  if (!agentMap.has(agentId)) {
    agentMap.set(agentId, agentMap.size % AGENT_COLORS.length);
  }
  return AGENT_COLORS[agentMap.get(agentId)!];
}

interface Props {
  events: AgentEvent[];
  isStreaming: boolean;
}

// ── Helpers ─────────────────────────────────────────────────────────

type EventCategory = "file_read" | "file_edit" | "command" | "thinking" | "error" | "text" | "done" | "search";

function getEventCategory(event: AgentEvent): EventCategory {
  if (event.type === "error") return "error";
  if (event.type === "file_edit") return "file_edit";
  if (event.type === "command_run") return "command";
  if (event.type === "thinking") return "thinking";
  if (event.type === "done") return "done";
  if (event.type === "tool_use") {
    const tool = (event.tool ?? "").toLowerCase();
    if (tool.includes("read") || tool === "read") return "file_read";
    if (tool.includes("write") || tool.includes("edit") || tool.includes("create")) return "file_edit";
    if (tool.includes("bash") || tool.includes("exec") || tool.includes("run") || tool.includes("command")) return "command";
    if (tool.includes("grep") || tool.includes("search") || tool.includes("find")) return "search";
    return "text";
  }
  return "text";
}

function matchesFilter(event: AgentEvent, filter: EventFilter): boolean {
  if (filter === "all") return true;
  const category = getEventCategory(event);
  if (filter === "files") return category === "file_read" || category === "file_edit";
  if (filter === "commands") return category === "command";
  if (filter === "errors") return category === "error";
  return true;
}

function formatRelativeTime(timestamp: number): string {
  const seconds = Math.floor((Date.now() - timestamp) / 1000);
  if (seconds < 5) return "now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ago`;
}

function truncateLines(text: string, maxLines: number, fromEnd = false): { lines: string[]; truncated: boolean } {
  const allLines = text.split("\n");
  if (allLines.length <= maxLines) return { lines: allLines, truncated: false };
  if (fromEnd) {
    return { lines: allLines.slice(-maxLines), truncated: true };
  }
  return { lines: allLines.slice(0, maxLines), truncated: true };
}

// ── Component ───────────────────────────────────────────────────────

export function ActivityFeed({ events, isStreaming }: Props) {
  const [filter, setFilter] = useState<EventFilter>("all");
  const [expandedIds, setExpandedIds] = useState<Set<number>>(new Set());
  const [autoScroll, setAutoScroll] = useState(true);
  const scrollRef = useRef<HTMLDivElement>(null);
  const prevEventCountRef = useRef(0);
  const agentColorMap = useRef(new Map<string, number>());

  // Auto-scroll to bottom during streaming.
  useEffect(() => {
    if (autoScroll && scrollRef.current && events.length > prevEventCountRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
    prevEventCountRef.current = events.length;
  }, [events.length, autoScroll]);

  // Detect user scroll-up to disable auto-scroll.
  const handleScroll = useCallback(() => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    setAutoScroll(scrollHeight - scrollTop - clientHeight < 40);
  }, []);

  const toggleExpand = useCallback((idx: number) => {
    setExpandedIds((prev) => {
      const next = new Set(prev);
      if (next.has(idx)) {
        next.delete(idx);
      } else {
        next.add(idx);
      }
      return next;
    });
  }, []);

  // Filtered events.
  const filteredEvents = useMemo(
    () => events.map((e, i) => ({ event: e, originalIndex: i })).filter(({ event }) => matchesFilter(event, filter)),
    [events, filter],
  );

  // Event count summary.
  const summary = useMemo(() => {
    let filesRead = 0;
    let filesWritten = 0;
    let commandsRun = 0;
    for (const event of events) {
      const cat = getEventCategory(event);
      if (cat === "file_read") filesRead++;
      if (cat === "file_edit") filesWritten++;
      if (cat === "command") commandsRun++;
    }
    return { filesRead, filesWritten, commandsRun };
  }, [events]);

  return (
    <div className="flex h-full flex-col">
      {/* Header: summary + filter */}
      <div className="shrink-0 border-b border-border bg-card px-4 py-2.5">
        <div className="flex items-center justify-between gap-3">
          {/* Header: summary + filter */}
          <div className="flex items-center gap-3 text-[11px] text-muted-foreground">
            {summary.filesRead > 0 && (
              <span className="flex items-center gap-1">
                <FileText size={11} className="text-info" />
                {summary.filesRead} read
              </span>
            )}
            {summary.filesWritten > 0 && (
              <span className="flex items-center gap-1">
                <FilePen size={11} className="text-success" />
                {summary.filesWritten} written
              </span>
            )}
            {summary.commandsRun > 0 && (
              <span className="flex items-center gap-1">
                <Terminal size={11} className="text-warning" />
                {summary.commandsRun} run
              </span>
            )}
            {summary.filesRead === 0 && summary.filesWritten === 0 && summary.commandsRun === 0 && <span>No events yet</span>}
          </div>

          {/* Filter dropdown */}
          <div className="relative">
            <Select
              value={":" + String(filter)}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                return setFilter(value as EventFilter);
              }}
            >
              <SelectTrigger
                className="appearance-none rounded border border-border bg-background px-2 py-0.5 pr-6 text-[11px] text-foreground focus:border-primary focus:outline-none"
                aria-label="Filter events"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value={":" + String("all")}>All</SelectItem>
                  <SelectItem value={":" + String("files")}>
                    Files only
                  </SelectItem>
                  <SelectItem value={":" + String("commands")}>
                    Commands only
                  </SelectItem>
                  <SelectItem value={":" + String("errors")}>
                    Errors only
                  </SelectItem>
                </SelectGroup>
              </SelectContent>
            </Select>
            <Filter size={10} className="pointer-events-none absolute right-1.5 top-1/2 -translate-y-1/2 text-muted-foreground" />
          </div>
        </div>
      </div>

      {/* Event list */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex-1 overflow-auto px-3 py-3 space-y-1.5"
      >
        {filteredEvents.length === 0 && (
          <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
            {events.length === 0 ? "Waiting for agent events…" : "No events match the current filter."}
          </div>
        )}

        {filteredEvents.map(({ event, originalIndex }) => (
          <EventCard
            key={originalIndex}
            event={event}
            expanded={expandedIds.has(originalIndex)}
            onToggle={() => toggleExpand(originalIndex)}
            isLatest={originalIndex === events.length - 1 && isStreaming}
            agentColor={getAgentColor(event.agentId, agentColorMap.current)}
          />
        ))}
      </div>

      {/* Jump to bottom button */}
      {!autoScroll && (
        <div className="flex justify-center pb-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => {
              setAutoScroll(true);
              scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: "smooth" });
            }}
            className="flex items-center gap-1 rounded-full border border-border bg-card px-2.5 py-1 text-[10px] text-muted-foreground hover:text-foreground transition"
          >
            <ChevronDown size={10} /> Jump to bottom
          </Button>
        </div>
      )}
    </div>
  );
}

// ── Event Card ──────────────────────────────────────────────────────

function EventCard({
  event,
  expanded,
  onToggle,
  isLatest,
  agentColor,
}: {
  event: AgentEvent;
  expanded: boolean;
  onToggle: () => void;
  isLatest: boolean;
  agentColor?: string;
}) {
  const category = getEventCategory(event);
  const { icon, bgColor, label, detail } = getEventDisplay(event, category);

  const hasExpandableContent = !!detail;

  // Text events render as chat bubbles
  if (category === "text") {
    return (
      <div className={clsx("flex gap-2", isLatest && "animate-pulse-subtle")}>
        <div className="mt-0.5 shrink-0">
          <div className="flex h-6 w-6 items-center justify-center rounded-full bg-primary/15">
            <Bot size={12} className="text-primary" />
          </div>
        </div>
        <div className="min-w-0 flex-1">
          <div className="rounded-lg rounded-tl-sm bg-card border border-border/50 px-3 py-2">
            <p className="text-xs text-foreground whitespace-pre-wrap break-words leading-relaxed">
              {event.content?.slice(0, 300) ?? ""}
              {event.content && event.content.length > 300 && (
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={onToggle}
                  className="ml-1 text-primary hover:underline"
                >
                  {expanded ? "show less" : "…more"}
                </Button>
              )}
            </p>
            {expanded && event.content && event.content.length > 300 && (
              <p className="mt-1 text-xs text-foreground whitespace-pre-wrap break-words leading-relaxed">
                {event.content.slice(300)}
              </p>
            )}
          </div>
          <div className="mt-0.5 flex items-center gap-2 px-1">
            {event.agentId && (
              <span className={clsx("text-[9px] font-medium", agentColor)}>
                {event.agentRole ?? event.agentId.slice(0, 6)}
              </span>
            )}
            <span className="text-[9px] text-muted-foreground">
              {formatRelativeTime(event.timestamp)}
            </span>
          </div>
        </div>
      </div>
    );
  }

  // Thinking events render as collapsible muted sections
  if (category === "thinking") {
    return (
      <div className="group">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onToggle}
          className="justify-start flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition hover:bg-card"
          aria-expanded={expanded}
        >
          <Brain size={13} className="shrink-0 text-thinking/80" />
          <span className="min-w-0 flex-1 truncate text-[11px] text-muted-foreground italic">
            {expanded ? "Thinking…" : (event.content?.slice(0, 80) ?? "Thinking…")}
          </span>
          <span className="shrink-0 text-[9px] text-muted-foreground">
            {formatRelativeTime(event.timestamp)}
          </span>
          <span className="shrink-0 text-muted-foreground">
            {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          </span>
        </Button>
        {expanded && event.content && (
          <div className="ml-7 mt-1 rounded-md border border-border/30 bg-background px-3 py-2">
            <p className="text-[11px] text-muted-foreground italic whitespace-pre-wrap leading-relaxed">
              {event.content}
            </p>
          </div>
        )}
      </div>
    );
  }

  // Done events render as a green success card
  if (category === "done") {
    return (
      <div className="flex items-center gap-2 rounded-md border border-success/25 bg-success/5 px-3 py-2">
        <CheckCircle2 size={14} className="shrink-0 text-success" />
        <span className="min-w-0 flex-1 text-xs text-success font-medium">
          {event.content ?? "Agent completed"}
        </span>
        <span className="shrink-0 text-[9px] text-muted-foreground">
          {formatRelativeTime(event.timestamp)}
        </span>
      </div>
    );
  }

  // Error events render as red alert cards
  if (category === "error") {
    return (
      <div className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2">
        <div className="flex items-center gap-2">
          <AlertCircle size={14} className="shrink-0 text-destructive" />
          <span className="min-w-0 flex-1 truncate text-xs text-destructive font-medium">
            {event.error ?? event.content ?? "Error"}
          </span>
          <span className="shrink-0 text-[9px] text-muted-foreground">
            {formatRelativeTime(event.timestamp)}
          </span>
        </div>
        {(event.error ?? event.content ?? "").length > 80 && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={onToggle}
            className="mt-1 ml-6 text-[10px] text-destructive/80 hover:text-destructive transition"
          >
            {expanded ? "Hide details" : "Show details"}
          </Button>
        )}
        {expanded && (
          <p className="mt-2 ml-6 text-[11px] text-destructive/90 whitespace-pre-wrap">
            {event.error ?? event.content}
          </p>
        )}
      </div>
    );
  }

  // Tool use events (file_read, file_edit, command, search) render as compact cards
  return (
    <div
      className={clsx(
        "rounded-md border px-3 py-2 transition",
        isLatest ? "border-primary/20 bg-primary/5" : "border-border/40 bg-background",
      )}
    >
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={hasExpandableContent ? onToggle : undefined}
        className="justify-start flex w-full items-center gap-2.5 text-left"
        aria-expanded={hasExpandableContent ? expanded : undefined}
      >
        {/* Multi-agent label */}
        <div className={clsx("flex h-6 w-6 shrink-0 items-center justify-center rounded", bgColor)}>
          {icon}
        </div>

        {/* One-line summary */}
        {event.agentId && (
          <span className={clsx("shrink-0 text-[9px] font-medium rounded px-1 py-0.5 bg-card", agentColor)}>
            {event.agentRole ?? event.agentId.slice(0, 6)}
          </span>
        )}

        {/* Timestamp */}
        <span className="min-w-0 flex-1 truncate text-[11px] text-foreground">
          {label}
        </span>

        {/* Expand indicator */}
        <span className="shrink-0 text-[9px] text-muted-foreground">
          {formatRelativeTime(event.timestamp)}
        </span>

        {/* Expandable detail */}
        {hasExpandableContent && (
          <span className="shrink-0 text-muted-foreground">
            {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          </span>
        )}
      </Button>

      {/* Expandable detail */}
      {expanded && detail && <div className="mt-2 ml-8">{detail}</div>}
    </div>
  );
}

// ── Event display logic ─────────────────────────────────────────────

function getEventDisplay(
  event: AgentEvent,
  category: EventCategory,
): { icon: React.ReactNode; bgColor: string; label: string; detail: React.ReactNode | null } {
  switch (category) {
    case "file_read":
      return {
        icon: <FileText size={12} className="text-info" />,
        bgColor: "bg-info/10",
        label: `Read ${extractPath(event)}`,
        detail: null,
      };

    case "file_edit":
      return {
        icon: <FilePen size={12} className="text-success" />,
        bgColor: "bg-success/10",
        label: `Edited ${extractPath(event)}`,
        detail: event.diff ? <MiniDiff diff={event.diff} /> : null,
      };

    case "command":
      return {
        icon: <Terminal size={12} className="text-warning" />,
        bgColor: "bg-warning/10",
        label: `Ran: ${event.command ?? "command"}${event.exitCode != null && event.exitCode !== 0 ? ` (exit ${event.exitCode})` : ""}`,
        detail: <CommandOutput event={event} />,
      };

    case "search":
      return {
        icon: <Search size={12} className="text-thinking" />,
        bgColor: "bg-thinking/10",
        label: `Searched for '${extractSearchPattern(event)}'`,
        detail: null,
      };

    case "done":
      return {
        icon: <CheckCircle2 size={12} className="text-success" />,
        bgColor: "bg-success/10",
        label: event.content ?? "Agent completed",
        detail: null,
      };

    case "error":
      return {
        icon: <AlertCircle size={12} className="text-destructive" />,
        bgColor: "bg-destructive/10",
        label: event.error ?? event.content ?? "Error",
        detail: null,
      };

    case "thinking":
      return {
        icon: <Brain size={12} className="text-thinking/80" />,
        bgColor: "bg-thinking/10",
        label: event.content?.slice(0, 80) ?? "Thinking…",
        detail: null,
      };

    default:
      return {
        icon: <MessageSquare size={12} className="text-muted-foreground" />,
        bgColor: "bg-card",
        label: event.content?.slice(0, 100) ?? "Event",
        detail:
          event.content && event.content.length > 100 ? (
            <p className="text-[11px] text-foreground whitespace-pre-wrap">
              {event.content}
            </p>
          ) : null,
      };
  }
}

// ── Path extraction helpers ─────────────────────────────────────────

function extractPath(event: AgentEvent): string {
  if (event.path) return event.path;
  if (event.input && typeof event.input === "object") {
    const inp = event.input as Record<string, unknown>;
    if (typeof inp.path === "string") return inp.path;
    if (typeof inp.file_path === "string") return inp.file_path;
  }
  return event.tool ?? "file";
}

function extractSearchPattern(event: AgentEvent): string {
  if (event.input && typeof event.input === "object") {
    const inp = event.input as Record<string, unknown>;
    if (typeof inp.pattern === "string") return inp.pattern.slice(0, 40);
    if (typeof inp.query === "string") return inp.query.slice(0, 40);
    if (typeof inp.regex === "string") return inp.regex.slice(0, 40);
  }
  return "pattern";
}

// ── Mini Diff (first 10 lines) ──────────────────────────────────────

function MiniDiff({ diff }: { diff: string }) {
  const [expanded, setExpanded] = useState(false);
  const { lines, truncated } = truncateLines(diff, expanded ? Infinity : 10);

  return (
    <div className="rounded border border-border/50 bg-background overflow-hidden">
      <pre className="overflow-x-auto p-2 text-[10px] leading-relaxed font-mono">
        {lines.map((line, i) => (
          <div
            key={i}
            className={clsx(
              line.startsWith("+") && !line.startsWith("+++") && "text-success bg-success/5",
              line.startsWith("-") && !line.startsWith("---") && "text-destructive bg-destructive/5",
              line.startsWith("@@") && "text-info",
            )}
          >
            {line}
          </div>
        ))}
      </pre>
      {truncated && !expanded && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setExpanded(true)}
          className="w-full border-t border-border/50 px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground transition text-center"
        >
          Show full diff…
        </Button>
      )}
    </div>
  );
}

// ── Command Output (last 20 lines, expandable) ──────────────────────

function CommandOutput({ event }: { event: AgentEvent }) {
  const [expanded, setExpanded] = useState(false);
  const output = event.stdout || event.stderr || "";

  if (!output) {
    return event.exitCode != null ? (
      <span className={clsx("text-[10px]", event.exitCode === 0 ? "text-success" : "text-destructive")}>
        Exit code: {event.exitCode}
      </span>
    ) : null;
  }

  const { lines, truncated } = truncateLines(output, expanded ? Infinity : 20, true);

  return (
    <div className="rounded border border-border/50 bg-background overflow-hidden">
      {event.exitCode != null && (
        <div className={clsx(
          "border-b border-border/50 px-2 py-1 text-[10px]",
          event.exitCode === 0 ? "text-success" : "text-destructive",
        )}>
          Exit code: {event.exitCode}
        </div>
      )}
      {truncated && !expanded && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setExpanded(true)}
          className="w-full border-b border-border/50 px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground transition text-center"
        >
          Show full output ({output.split("\n").length} lines)…
        </Button>
      )}
      <pre className="overflow-x-auto p-2 text-[10px] leading-relaxed font-mono text-muted-foreground">
        {lines.join("\n")}
      </pre>
    </div>
  );
}
