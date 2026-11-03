import * as http from "node:http";
import type { AgentWorkflowDefinitionStore, AgentWorkflowTriggerStore } from "./persistence";
import type { AgentWorkflowRunner } from "./runner";

const WEBHOOK_PORT = 17789;

export class AgentWorkflowWebhookServer {
  private server: http.Server | null = null;
  private triggerStore: AgentWorkflowTriggerStore;
  private definitionStore: AgentWorkflowDefinitionStore;
  private runner: AgentWorkflowRunner;

  constructor(
    triggerStore: AgentWorkflowTriggerStore,
    definitionStore: AgentWorkflowDefinitionStore,
    runner: AgentWorkflowRunner,
  ) {
    this.triggerStore = triggerStore;
    this.definitionStore = definitionStore;
    this.runner = runner;
  }

  start(): void {
    if (this.server) return;
    this.server = http.createServer((req, res) => {
      if (req.method !== "POST") {
        res.writeHead(405).end(JSON.stringify({ error: "Method not allowed" }));
        return;
      }
      const token = (req.url ?? "").replace(/^\//, "");
      if (!token) {
        res.writeHead(400).end(JSON.stringify({ error: "Missing webhook token" }));
        return;
      }
      let body = "";
      req.on("data", (chunk: Buffer) => { body += chunk.toString(); });
      req.on("end", async () => {
        try {
          const triggers = await this.triggerStore.list();
          const trigger = triggers.find((t) => t.kind === "webhook" && t.webhookToken === token && t.enabled);
          if (!trigger) {
            res.writeHead(404).end(JSON.stringify({ error: "Webhook not found or disabled" }));
            return;
          }
          let input = trigger.input ?? {};
          if (body) {
            try { input = { ...input, ...JSON.parse(body) as Record<string, unknown> }; } catch { input = { ...input, body }; }
          }
          const definition = await this.definitionStore.get(trigger.repoPath, trigger.workflowDefinitionId);
          if (!definition) {
            res.writeHead(404).end(JSON.stringify({ error: "Workflow definition not found" }));
            return;
          }
          const run = await this.runner.startRun(trigger.repoPath, definition, { input, trigger: { type: "webhook", triggerId: trigger.id } });
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ runId: run.id, status: run.status }));
        } catch (err: unknown) {
          res.writeHead(500).end(JSON.stringify({ error: err instanceof Error ? err.message : "Internal error" }));
        }
      });
    });
    this.server.listen(WEBHOOK_PORT, "127.0.0.1", () => {
      console.log(`[agent-workflow] Webhook server listening on http://127.0.0.1:${WEBHOOK_PORT}`);
    });
  }

  getUrl(token: string): string {
    return `http://127.0.0.1:${WEBHOOK_PORT}/${token}`;
  }

  stop(): void {
    if (this.server) {
      this.server.close();
      this.server = null;
    }
  }
}
