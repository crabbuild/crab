/**
 * OpenAI-compatible commit message generator.
 *
 * Calls POST /v1/chat/completions at a configurable base URL.
 * Works with OpenAI, Anthropic (via proxy), local models with
 * OpenAI-compatible APIs (LM Studio, vLLM, etc.).
 *
 * API key is stored in the OS keychain via the window.crab.keychain
 * bridge.
 */

import type { CommitMessageContext, CommitMessageGenerator } from "./commit-message-generator";
import { registerGenerator } from "./commit-message-generator";

const KEYCHAIN_KEY = "ai-provider-api-key";

const SYSTEM_MESSAGE = [
  "You are a commit message generator.",
  "Given a git diff and recent commit subjects from the same project,",
  "write a single concise commit message subject line (max 72 characters).",
  "Follow the project's existing commit style.",
  "Output ONLY the subject line — no body, no explanation, no quotes, no prefix like 'commit:'.",
].join(" ");

function buildUserMessage(ctx: CommitMessageContext): string {
  const parts: string[] = [];
  if (ctx.recentSubjects.length > 0) {
    parts.push("Recent commits in this project:\n" + ctx.recentSubjects.map((s) => `- ${s}`).join("\n"));
  }
  parts.push("Staged diff:\n```\n" + ctx.stagedPatch.slice(0, 12000) + "\n```");
  return parts.join("\n\n");
}

export class OpenAICompatibleGenerator implements CommitMessageGenerator {
  readonly id = "openai-compatible";
  readonly name = "OpenAI-compatible API";

  async suggest(ctx: CommitMessageContext): Promise<string> {
    const settings = await this.loadSettings();
    if (!settings.endpoint) {
      throw new Error(
        "OpenAI-compatible endpoint not configured. Set it in Settings → AI Provider.",
      );
    }

    const apiKey = await this.getApiKey();
    if (!apiKey) {
      throw new Error(
        "API key not found. Set your API key in Settings → AI Provider.",
      );
    }

    const url = settings.endpoint.replace(/\/+$/, "") + "/v1/chat/completions";
    const body = JSON.stringify({
      model: settings.model || "gpt-4",
      messages: [
        { role: "system", content: SYSTEM_MESSAGE },
        { role: "user", content: buildUserMessage(ctx) },
      ],
      max_tokens: 100,
      temperature: 0.3,
    });

    const response = await fetch(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${apiKey}`,
      },
      body,
      signal: AbortSignal.timeout(30_000),
    });

    if (!response.ok) {
      const text = await response.text().catch(() => "");
      throw new Error(
        `OpenAI-compatible request failed (${response.status}): ${text.slice(0, 200)}`,
      );
    }

    const json = (await response.json()) as {
      choices?: Array<{ message?: { content?: string } }>;
    };
    const raw = json.choices?.[0]?.message?.content ?? "";
    return raw.trim().split("\n")[0].trim();
  }

  private async getApiKey(): Promise<string | null> {
    try {
      return await window.crab.keychain.get(KEYCHAIN_KEY);
    } catch {
      return null;
    }
  }

  private async loadSettings(): Promise<{ endpoint: string | null; model: string | null }> {
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

/** Store the API key in the OS keychain. */
export async function setOpenAIApiKey(key: string): Promise<void> {
  await window.crab.keychain.set(KEYCHAIN_KEY, key);
}

/** Remove the API key from the OS keychain. */
export async function deleteOpenAIApiKey(): Promise<void> {
  await window.crab.keychain.delete(KEYCHAIN_KEY);
}

/** Check if an API key is stored. */
export async function hasOpenAIApiKey(): Promise<boolean> {
  return window.crab.keychain.has(KEYCHAIN_KEY);
}

// Auto-register on import
registerGenerator(new OpenAICompatibleGenerator());
