import type { RpcRouter } from "./ssh-router";
import type { AgentCapabilities } from "./ssh-types";

export class CapabilityNegotiator {
  private cache = new Map<string, { capabilities?: AgentCapabilities }>();

  constructor(private readonly router: RpcRouter) {}

  async negotiate(profileId: string): Promise<AgentCapabilities> {
    // Clearing or replacing the entry revokes an in-flight negotiation; an
    // older connection must not restore capabilities after disconnect.
    const entry: { capabilities?: AgentCapabilities } = {};
    this.cache.set(profileId, entry);
    // Bootstrap runs while the session is deploying, before repository routing
    // is available. The attached channel owns this control request.
    const result: unknown = await this.router.sendToRemoteAgent(profileId, "capabilities", {});
    if (!result || typeof result !== "object") throw new Error("Incompatible Crab agent capabilities");
    const { protocol, version, methods } = result as Record<string, unknown>;
    if (protocol !== 1 || typeof version !== "string" || !version.trim() ||
        !Array.isArray(methods) || !methods.every((method): method is string => typeof method === "string" && !!method) ||
        !methods.includes("capabilities") || new Set(methods).size !== methods.length) {
      throw new Error("Incompatible Crab agent capabilities");
    }
    if (this.cache.get(profileId) !== entry) throw new Error("Agent capability negotiation superseded");
    const capabilities: AgentCapabilities = { protocol, version, methods };
    entry.capabilities = capabilities;
    return capabilities;
  }

  isSupported(profileId: string, ...methods: string[]): boolean {
    const capabilities = this.getCached(profileId);
    return !!capabilities && methods.length > 0 && methods.every(method => capabilities.methods.includes(method));
  }

  getCached(profileId: string): AgentCapabilities | null {
    return this.cache.get(profileId)?.capabilities ?? null;
  }

  clearCache(profileId: string): void {
    this.cache.delete(profileId);
  }
}
