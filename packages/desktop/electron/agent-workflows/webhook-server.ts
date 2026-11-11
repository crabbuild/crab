import * as http from "node:http";
import type { AgentWorkflowDefinitionStore, AgentWorkflowTriggerStore } from "./persistence";
import type { AgentWorkflowRunner } from "./runner";

const WEBHOOK_PORT = 17789;

export class AgentWorkflowWebhookServer {
  private server: http.Server | null = null;
  private ready?: Promise<void>;

  constructor(
    private readonly triggerStore: Pick<AgentWorkflowTriggerStore, "list">,
    private readonly definitionStore: Pick<AgentWorkflowDefinitionStore, "get">,
    private readonly runner: Pick<AgentWorkflowRunner, "startRun">,
  ) {}

  start(): Promise<void> {
    if (this.ready) return this.ready;
    const server = http.createServer((req, res) => {
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
    this.server = server;
    // Binding is asynchronous. Its caller must see refusal before publishing
    // an enabled endpoint; a failed attempt must permit an explicit retry.
    this.ready = new Promise((resolve, reject) => {
      server.once("error", error => {
        this.server = null;
        this.ready = undefined;
        reject(new Error(`Workflow webhooks could not listen on 127.0.0.1:${WEBHOOK_PORT}: ${error.message}`, { cause: error }));
      });
      server.listen(WEBHOOK_PORT, "127.0.0.1", resolve);
    });
    return this.ready;
  }

  getUrl(token: string): string {
    return `http://127.0.0.1:${WEBHOOK_PORT}/${token}`;
  }

  async stop(): Promise<void> {
    const server = this.server;
    if (!server) return;
    await this.ready;
    await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
    this.server = null;
    this.ready = undefined;
  }
}
