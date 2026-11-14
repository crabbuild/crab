import { useMemo, useState, type ReactNode } from "react";
import { ArrowDownToLine, ArrowRight, Bot, Check, ChevronRight, Circle, CircleAlert, Copy, CornerDownRight, GitBranch, Loader2, MessageSquare, Send, Square } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import type { ChatMessage } from "../../../electron/agent-chat-messages";

type Agent = ChatMessage & { depth: number };

function collectAgents(messages: ChatMessage[]): Agent[] {
  const all = new Map<string, ChatMessage>();
  const gather = (entries: ChatMessage[], parentId?: string) => {
    for (const message of entries) {
      if (!message.subagent) continue;
      const subagent = { ...message.subagent, parentId: message.subagent.parentId ?? parentId };
      all.set(subagent.id, { ...message, subagent });
      gather(message.messages ?? [], subagent.id);
    }
  };
  gather(messages);
  const ordered: Agent[] = [];
  const visited = new Set<string>();
  const visit = (message: ChatMessage, depth: number) => {
    const id = message.subagent!.id;
    if (visited.has(id)) return;
    visited.add(id);
    ordered.push({ ...message, depth });
    for (const child of all.values()) if (child.subagent?.parentId === id) visit(child, depth + 1);
  };
  for (const message of all.values()) if (!all.has(message.subagent?.parentId ?? "")) visit(message, 0);
  return ordered;
}

function transcriptText(messages: ChatMessage[]): string {
  const blocks: string[] = [];
  let commandSession: string | undefined;
  for (const message of messages) {
    const event = message.providerEvent;
    if (event?.type === "command_output") {
      if (commandSession && commandSession === event.commandSessionId) blocks[blocks.length - 1] += event.content ?? "";
      else blocks.push(event.content ?? "");
      commandSession = event.commandSessionId;
      continue;
    }
    commandSession = undefined;
    const text = [message.content, event?.agentMessage?.content,
      event?.type === "command_exited" ? `Exit code: ${event.exitCode}` : undefined,
      event?.type === "filesystem_change" ? `${event.fsChangeKind ?? "Changed"}: ${event.path}` : undefined,
      ...(message.events ?? []).map(tool => [tool.tool, JSON.stringify(tool.input, null, 2), tool.detail].filter(Boolean).join("\n")),
      ...(message.subagent ? [message.subagent.name, transcriptText(message.messages ?? [])] : []),
    ].filter(Boolean).join("\n\n");
    if (text) blocks.push(text);
  }
  return blocks.join("\n\n");
}

function AgentStatus({ status }: { status?: string }) {
  const running = status === "running" || status === "starting";
  const failed = status === "failed" || status === "interrupted";
  const Icon = running ? Loader2 : status === "completed" ? Check : failed ? CircleAlert : Circle;
  return <span className={cn("inline-flex shrink-0 items-center gap-1.5 text-xs", failed ? "text-destructive" : "text-muted-foreground")}>
    <Icon size={12} className={cn(running && "motion-safe:animate-spin", status === "completed" && "text-success")} />
    {status === "starting" ? "Starting" : status ? status.charAt(0).toUpperCase() + status.slice(1) : "Waiting"}
  </span>;
}

export function AgentTeamPanel({ messages, taskId, renderTranscript }: {
  messages: ChatMessage[];
  taskId: string;
  renderTranscript: (messages: ChatMessage[], ownerId: string) => ReactNode;
}) {
  const agents = useMemo(() => collectAgents(messages), [messages]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const selected = agents.find(agent => agent.id === selectedId);
  const metadata = selected?.subagent;
  const selectedMessages = selected?.messages ?? [];
  const running = agents.filter(agent => ["starting", "running", "queued"].includes(agent.subagent?.status ?? "")).length;
  const failed = agents.filter(agent => ["failed", "interrupted"].includes(agent.subagent?.status ?? "")).length;
  const communications = useMemo(() => {
    const entries = [...messages, ...agents.flatMap(agent => agent.messages ?? [])];
    const unique = new Map<string, ChatMessage>();
    for (const entry of entries) {
      const message = entry.providerEvent?.agentMessage;
      if (message) unique.set(JSON.stringify([message.from, message.to, entry.providerEvent?.itemId ?? entry.timestamp]), entry);
    }
    return [...unique.values()].sort((a, b) => a.timestamp - b.timestamp);
  }, [messages, agents]);
  if (!agents.length) return null;
  const name = (id: string) => id === taskId ? "Lead agent" : agents.find(agent => agent.subagent?.id === id)?.subagent?.name ?? id;
  const ancestry = (agent: Agent): string => {
    const chain: string[] = [];
    let parent = agents.find(candidate => candidate.subagent?.id === agent.subagent?.parentId);
    while (parent) {
      chain.unshift(parent.subagent?.name ?? "Sub-agent");
      parent = agents.find(candidate => candidate.subagent?.id === parent?.subagent?.parentId);
    }
    return chain.join(" / ");
  };
  const open = (id: string) => { setSelectedId(id); setError(""); setCopied(false); };
  const draft = selectedId ? drafts[selectedId] ?? "" : "";
  const act = async (action: "send" | "stop") => {
    if (!metadata?.taskId || pending || action === "send" && !draft.trim()) return;
    setPending(true);
    setError("");
    try {
      const result = action === "send" ? await window.crab.orchestrator.iterate(metadata.taskId, draft)
        : await window.crab.orchestrator.cancelTask(metadata.taskId);
      if (!result.success) throw new Error(result.error ?? `Could not ${action === "send" ? "send message" : "stop agent"}`);
      if (action === "send" && selectedId) setDrafts(previous => ({ ...previous, [selectedId]: "" }));
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
    finally { setPending(false); }
  };
  const fullText = () => [metadata?.prompt, transcriptText(selectedMessages)].filter(Boolean).join("\n\n");

  return <section aria-label="Agent team" className="mb-5 overflow-hidden rounded-xl border border-border bg-card">
    <div className="flex items-center gap-2 px-3 py-2.5">
      <GitBranch size={14} className="text-muted-foreground" />
      <h3 className="text-xs font-medium">Agent team</h3>
      <span className="text-xs tabular-nums text-muted-foreground">{agents.length} {agents.length === 1 ? "agent" : "agents"}{running ? ` · ${running} working` : failed ? ` · ${failed} need attention` : " · All finished"}</span>
    </div>
    <div className="divide-y divide-border border-t border-border">
      {agents.map(agent => <Button key={agent.id} variant="ghost" size="compact" disabled={pending}
        className="min-h-11 w-full justify-start gap-3 rounded-none px-3 py-2 text-left" onClick={() => open(agent.id)}>
        <Bot size={14} className="shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2">{agent.depth > 0 && <CornerDownRight size={12} className="shrink-0 text-muted-foreground" />}<span className="truncate">{agent.subagent?.name ?? "Sub-agent"}</span>
            <span className="shrink-0 text-xs text-muted-foreground">{agent.subagent?.provider === "claude-code" ? "Claude" : "Codex"}</span></span>
          {agent.depth > 0 && <span className="block truncate text-xs font-normal text-muted-foreground">{ancestry(agent)}</span>}
          {agent.subagent?.prompt && <span className="block truncate text-xs font-normal text-muted-foreground">{agent.subagent.prompt}</span>}
        </span>
        <AgentStatus status={agent.subagent?.status} /><ChevronRight size={12} className="shrink-0 text-muted-foreground" />
      </Button>)}
    </div>
    <Dialog open={Boolean(selected)} onOpenChange={value => { if (!value) setSelectedId(null); }}>
      <DialogContent className="flex h-[90dvh] w-[calc(100vw-24px)] max-w-6xl flex-col gap-0 overflow-hidden rounded-xl p-0">
        <div className="flex shrink-0 items-center gap-3 border-b border-border px-5 py-4 pr-12">
          <GitBranch size={18} className="text-muted-foreground" />
          <div><DialogTitle>Agent team</DialogTitle><DialogDescription>Follow every agent, from instructions to results.</DialogDescription></div>
        </div>
        <div className="flex min-h-0 flex-1 flex-col sm:flex-row">
          <nav aria-label="Team agents" className="flex max-h-36 shrink-0 gap-1 overflow-auto border-b border-border bg-muted/30 p-2 sm:max-h-none sm:w-56 sm:flex-col sm:border-b-0 sm:border-r">
            {agents.map(agent => <Button key={agent.id} variant={agent.id === selectedId ? "secondary" : "ghost"} size="compact" disabled={pending}
              aria-current={agent.id === selectedId ? "true" : undefined}
              className="min-h-14 min-w-40 flex-col items-start gap-1 px-3 py-2 text-left sm:min-w-0" onClick={() => open(agent.id)}>
              <span className="flex w-full items-center gap-2">{agent.depth > 0 ? <CornerDownRight size={13} /> : <Bot size={13} />}<span className="truncate">{agent.subagent?.name ?? "Sub-agent"}</span></span>
              {agent.depth > 0 && <span className="block w-full truncate text-xs font-normal text-muted-foreground">{ancestry(agent)}</span>}
              <AgentStatus status={agent.subagent?.status} />
            </Button>)}
          </nav>
          <div className="flex min-h-0 min-w-0 flex-1 flex-col">
            <div className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-3">
              <h4 className="min-w-0 flex-1 break-words text-sm font-medium">{metadata?.name ?? "Sub-agent"}</h4>
              <Badge variant="outline">{metadata?.provider === "claude-code" ? "Claude Code" : "Codex"}</Badge>
              <Button variant="ghost" size="icon-sm" aria-label="Copy full transcript" title={copied ? "Copied" : "Copy full transcript"} onClick={() => {
                void navigator.clipboard.writeText(fullText()).then(() => setCopied(true), () => setError("Could not copy transcript. Download it instead."));
              }}>{copied ? <Check /> : <Copy />}</Button>
              <Button variant="ghost" size="icon-sm" aria-label="Download full transcript" title="Download full transcript" onClick={() => {
                const url = URL.createObjectURL(new Blob([fullText()], { type: "text/plain;charset=utf-8" }));
                const link = document.createElement("a"); link.href = url; link.download = "agent-transcript.txt"; link.click();
                setTimeout(() => URL.revokeObjectURL(url), 0);
              }}><ArrowDownToLine /></Button>
              {metadata?.taskId && ["running", "queued", "starting"].includes(metadata.status ?? "") &&
                <Button variant="outline" size="sm" disabled={pending} onClick={() => void act("stop")}><Square data-icon="inline-start" />Stop</Button>}
            </div>
            <Tabs defaultValue="transcript" className="flex min-h-0 flex-1 flex-col">
              <TabsList className="mx-4 mt-3 w-fit shrink-0"><TabsTrigger value="transcript">Transcript</TabsTrigger>
                <TabsTrigger value="full-text">Full text</TabsTrigger>
                <TabsTrigger value="messages">Messages{communications.length > 0 ? ` (${communications.length})` : ""}</TabsTrigger></TabsList>
              <TabsContent value="transcript" className="min-h-0 flex-1 overflow-auto px-5 pb-5">
                {metadata?.prompt && <details open className="mb-5 border-b border-border py-3"><summary className="cursor-pointer text-xs font-medium text-muted-foreground">Instructions</summary>
                  <p className="mt-2 whitespace-pre-wrap break-words text-sm leading-relaxed">{metadata.prompt}</p></details>}
                {selectedMessages.length ? renderTranscript(selectedMessages.filter(message => message.kind !== "subagent"), metadata?.taskId ?? taskId)
                  : <div className="flex min-h-40 flex-col items-center justify-center gap-2 text-center text-sm text-muted-foreground"><Bot size={24} /><p>{metadata?.status === "queued" ? "Waiting for an available agent slot." : "The agent has not sent output yet."}</p><p className="text-xs">Its complete output will appear here.</p></div>}
              </TabsContent>
              <TabsContent value="full-text" className="min-h-0 flex-1 overflow-auto px-5 pb-5">
                <pre className="whitespace-pre-wrap break-words py-3 font-mono text-xs leading-relaxed">{fullText() || "The agent has not sent output yet."}</pre>
              </TabsContent>
              <TabsContent value="messages" className="min-h-0 flex-1 overflow-auto px-5 pb-5">
                {communications.length ? <ol className="flex flex-col gap-5 py-3">{communications.map(entry => {
                  const message = entry.providerEvent!.agentMessage!;
                  return <li key={entry.id} className="flex gap-3"><MessageSquare size={15} className="mt-0.5 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground"><span className="break-all font-medium text-foreground">{name(message.from)}</span><ArrowRight size={12} /><span className="break-all">{name(message.to)}</span><span>· {message.delivery === "accepted" ? "Delivered" : message.delivery === "unconfirmed" ? "Delivery unconfirmed" : "Delivery failed"}</span></div>
                    <p className="mt-2 whitespace-pre-wrap break-words text-sm leading-relaxed">{message.content}</p>
                  </div></li>;
                })}</ol> : <p className="py-10 text-center text-sm text-muted-foreground">Agent messages will appear here as the team coordinates.</p>}
              </TabsContent>
            </Tabs>
            {error && <p role="alert" className="px-4 py-2 text-xs text-destructive">{error}</p>}
            {metadata?.taskId && <form className="flex shrink-0 gap-2 border-t border-border p-3" onSubmit={event => { event.preventDefault(); void act("send"); }}>
              <Textarea aria-label="Message this agent" placeholder={`Message ${metadata.name ?? "this agent"}…`} value={draft} className="min-h-16 flex-1 resize-none" disabled={pending}
                onChange={event => selectedId && setDrafts(previous => ({ ...previous, [selectedId]: event.target.value }))} />
              <Button type="submit" variant="default" size="icon-sm" aria-label="Send message to agent" disabled={pending || !draft.trim()}>{pending ? <Loader2 className="motion-safe:animate-spin" /> : <Send />}</Button>
            </form>}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  </section>;
}
