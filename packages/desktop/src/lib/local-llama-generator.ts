/**
 * Local-llama commit message generator.
 *
 * Calls a user-configured HTTP endpoint (e.g., Ollama at
 * http://localhost:11434/api/generate) with the staged patch and
 * recent commit subjects as context.
 */

import type { CommitMessageContext, CommitMessageGenerator } from "./commit-message-generator";
import { registerGenerator } from "./commit-message-generator";

const SYSTEM_PROMPT = [
  "You are a commit message generator.",
  "Given a git diff and recent commit subjects, write a single concise commit message subject line.",
  "Follow conventional commit style if the project uses it.",
  "Output ONLY the subject line — no body, no explanation, no quotes.",
].join(" ");

function buildPrompt(ctx: CommitMessageContext): string {
  const parts: string[] = [];
  if (ctx.recentSubjects.length > 0) {
    parts.push("Recent commits:\n" + ctx.recentSubjects.map((s) => `- ${s}`).join("\n"));
  }
  parts.push("Staged diff:\n```\n" + ctx.stagedPatch.slice(0, 8000) + "\n```");
  parts.push("Write a concise commit message subject line for this change:");
  return parts.join("\n\n");
}

export class LocalLlamaGenerator implements CommitMessageGenerator {
  readonly id = "local-llama";
  readonly name = "Local LLM (Ollama / llama.cpp)";

  async suggest(ctx: CommitMessageContext): Promise<string> {
    const settings = await this.loadSettings();
    if (!settings.endpoint) {
      throw new Error("Local LLM endpoint not configured. Set it in Settings → AI Provider.");
    }

    const prompt = buildPrompt(ctx);
    const body = JSON.stringify({
      model: settings.model || "llama3",
      prompt: `${SYSTEM_PROMPT}\n\n${prompt}`,
      stream: false,
    });

    const response = await fetch(settings.endpoint, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
      signal: AbortSignal.timeout(30_000),
    });

    if (!response.ok) {
      const text = await response.text().catch(() => "");
      throw new Error(
        `Local LLM request failed (${response.status}): ${text.slice(0, 200)}`,
      );
    }

    const json = (await response.json()) as { response?: string; message?: { content?: string } };
    // Ollama returns { response: "..." }, llama.cpp server may differ
    const raw = json.response ?? json.message?.content ?? "";
    return raw.trim().split("\n")[0].trim();
  }

  private async loadSettings(): Promise<{ endpoint: string | null; model: string | null }> {
    // Read from localStorage (renderer settings)
    try {
      const raw = localStorage.getItem("crab.settings.v1");
      if (raw) {
        const parsed = JSON.parse(raw) as Record<string, unknown>;
        return {
          endpoint: (parsed.aiEndpoint as string) || null,
          model: (parsed.aiModel as string) || null,
        };
      }
    } catch {
      // Fall through
    }
    return { endpoint: null, model: null };
  }
}

// Auto-register on import
registerGenerator(new LocalLlamaGenerator());
