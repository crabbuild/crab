import type { SymphonyTokenTotals } from "./types";

export const EMPTY_TOKEN_TOTALS: SymphonyTokenTotals = {
  inputTokens: 0,
  outputTokens: 0,
  totalTokens: 0,
};

export function extractAbsoluteTokenTotals(message: unknown): SymphonyTokenTotals | null {
  const method = readString(message, ["method"]) || readString(message, ["message", "method"]);
  if (method === "thread/tokenUsage/updated") {
    const total =
      readObject(message, ["params", "tokenUsage", "total"]) ||
      readObject(message, ["message", "params", "tokenUsage", "total"]);
    return tokenTotalsFromObject(total);
  }

  const eventType = readString(message, ["type"]) || readString(message, ["event"]);
  if (eventType === "codex/event/token_count") {
    const total =
      readObject(message, ["info", "total_token_usage"]) ||
      readObject(message, ["payload", "info", "total_token_usage"]);
    return tokenTotalsFromObject(total);
  }

  const tokenUsageTotal =
    readObject(message, ["tokenUsage", "total"]) ||
    readObject(message, ["token_usage", "total"]) ||
    readObject(message, ["params", "tokenUsage", "total"]);
  return tokenTotalsFromObject(tokenUsageTotal);
}

export function addTokenDelta(
  current: SymphonyTokenTotals,
  previous: SymphonyTokenTotals,
  nextAbsolute: SymphonyTokenTotals,
): SymphonyTokenTotals {
  return {
    inputTokens: current.inputTokens + Math.max(0, nextAbsolute.inputTokens - previous.inputTokens),
    outputTokens: current.outputTokens + Math.max(0, nextAbsolute.outputTokens - previous.outputTokens),
    totalTokens: current.totalTokens + Math.max(0, nextAbsolute.totalTokens - previous.totalTokens),
  };
}

function tokenTotalsFromObject(value: unknown): SymphonyTokenTotals | null {
  if (!value || typeof value !== "object") return null;
  const obj = value as Record<string, unknown>;
  const inputTokens = numberField(obj, "input_tokens") ?? numberField(obj, "inputTokens") ?? 0;
  const outputTokens = numberField(obj, "output_tokens") ?? numberField(obj, "outputTokens") ?? 0;
  const totalTokens =
    numberField(obj, "total_tokens") ??
    numberField(obj, "totalTokens") ??
    inputTokens + outputTokens;

  return { inputTokens, outputTokens, totalTokens };
}

function numberField(obj: Record<string, unknown>, field: string): number | null {
  const value = obj[field];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function readObject(value: unknown, path: string[]): Record<string, unknown> | null {
  const result = readPath(value, path);
  return result && typeof result === "object" && !Array.isArray(result)
    ? result as Record<string, unknown>
    : null;
}

function readString(value: unknown, path: string[]): string | null {
  const result = readPath(value, path);
  return typeof result === "string" ? result : null;
}

function readPath(value: unknown, parts: string[]): unknown {
  let current = value;
  for (const part of parts) {
    if (!current || typeof current !== "object") return undefined;
    current = (current as Record<string, unknown>)[part];
  }
  return current;
}
