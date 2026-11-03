export interface ChatHistoryTaskPrompt {
  id: string;
  prompt?: string;
  createdAt?: number;
}

export interface ChatHistoryMessage {
  id: string;
  role: "user" | "agent";
  kind?: string;
  content: string;
  events?: unknown[];
  timestamp: number;
}

export function initialPromptMessageForTask(task: ChatHistoryTaskPrompt | null | undefined): ChatHistoryMessage | null {
  const prompt = task?.prompt?.trim();
  if (!task || !prompt) return null;
  return {
    id: `${task.id}-init`,
    role: "user",
    content: task.prompt ?? prompt,
    timestamp: task.createdAt ?? Date.now(),
  };
}

export function ensureInitialPromptMessage(
  messages: ChatHistoryMessage[],
  task: ChatHistoryTaskPrompt | null | undefined,
): ChatHistoryMessage[] {
  if (messages.some((message) => message.role === "user")) return messages;
  const initialMessage = initialPromptMessageForTask(task);
  if (!initialMessage) return messages;
  if (messages.some((message) => message.id === initialMessage.id)) return messages;
  return [initialMessage, ...messages];
}
