import { useEffect, useRef, useState } from "react";
import { applyChatUpdate, type ChatMessage, type ChatUpdate } from "../../electron/agent-chat-messages";
import { publishToast } from "./toasts";

/** Keeps a renderer replica current across snapshot loads and concurrent updates. */
export function useAgentChat(taskId: string | undefined) {
  const [messages, setMessages] = useState<Record<string, ChatMessage[]>>({});
  const pendingChatLoads = useRef(new Map<string, ChatUpdate[]>());
  const chatRevisions = useRef(new Map<string, number>());

  useEffect(() => {
    const offUpdate = window.crab.orchestrator.onChatUpdate(update => {
      pendingChatLoads.current.get(update.taskId)?.push(update);
      if (update.revision <= (chatRevisions.current.get(update.taskId) ?? -1)) return;
      chatRevisions.current.set(update.taskId, update.revision);
      setMessages(previous => ({ ...previous, [update.taskId]: applyChatUpdate(previous[update.taskId] ?? [], update) }));
    });
    const offError = window.crab.orchestrator.onError(error => {
      publishToast({ kind: "error", title: "Agent error", body: error.message });
    });
    return () => { offUpdate(); offError(); };
  }, []);

  useEffect(() => {
    if (!taskId) return;
    const updates: ChatUpdate[] = [];
    pendingChatLoads.current.set(taskId, updates);
    let current = true;
    window.crab.orchestrator.loadChat(taskId).then(result => {
      if (!current) return;
      if (!result.success || !result.messages || result.revision === undefined) throw new Error(result.error ?? "Could not load chat");
      const latest = updates.filter(update => update.revision > result.revision!).sort((a, b) => a.revision - b.revision);
      const transcript = latest.reduce(applyChatUpdate, result.messages);
      chatRevisions.current.set(taskId, latest.at(-1)?.revision ?? result.revision);
      setMessages(previous => ({ ...previous, [taskId]: transcript }));
      if (result.persistenceError) publishToast({ kind: "error", title: "Chat history incomplete", body: result.persistenceError });
    }).catch((error: unknown) => {
      if (current) publishToast({ kind: "error", title: "Could not load chat", body: error instanceof Error ? error.message : String(error) });
    }).finally(() => {
      if (pendingChatLoads.current.get(taskId) === updates) pendingChatLoads.current.delete(taskId);
    });
    return () => {
      current = false;
      if (pendingChatLoads.current.get(taskId) === updates) pendingChatLoads.current.delete(taskId);
    };
  }, [taskId]);

  return [messages, setMessages] as const;
}
