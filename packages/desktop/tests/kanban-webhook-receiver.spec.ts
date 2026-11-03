import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as http from "node:http";
import * as crypto from "node:crypto";

import { WebhookReceiver } from "../electron/kanban/webhook-receiver";
import type { WebhookEvent } from "../electron/kanban/webhook-receiver";
import type { BoardConfig, KanbanColumn } from "../electron/kanban/kanban-types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

// ── Test Helpers ─────────────────────────────────────────────────

const DEFAULT_COLUMNS: KanbanColumn[] = [
  { id: "backlog", name: "Backlog", trackerStatusMapping: ["backlog", "open"] },
  { id: "ready", name: "Ready", trackerStatusMapping: ["ready", "todo"] },
  { id: "in-progress", name: "In Progress", trackerStatusMapping: ["in progress"] },
  { id: "in-review", name: "In Review", trackerStatusMapping: ["in review"] },
  { id: "done", name: "Done", trackerStatusMapping: ["done", "closed"] },
];

function createBoardConfig(overrides: Partial<BoardConfig> = {}): BoardConfig {
  return {
    id: "board-1",
    repoPath: "/test/repo",
    columns: DEFAULT_COLUMNS,
    trackerType: "github",
    trackerConfig: { owner: "test", repo: "repo" },
    autonomyLevel: "moderate",
    pollIntervalMs: 60000,
    ciPollIntervalMs: 30000,
    maxIterations: 3,
    filterCriteria: {},
    webhookPort: 0, // Use port 0 for random available port in tests
    ...overrides,
  };
}

function computeGitHubSignature(secret: string, body: string): string {
  const hmac = crypto.createHmac("sha256", secret).update(body).digest("hex");
  return `sha256=${hmac}`;
}

function computeJiraSignature(secret: string, body: string): string {
  const hmac = crypto.createHmac("sha256", secret).update(body).digest("hex");
  return `sha256=${hmac}`;
}

function sendWebhook(
  port: number,
  payload: unknown,
  headers: Record<string, string> = {},
): Promise<{ statusCode: number; body: string }> {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify(payload);
    const options: http.RequestOptions = {
      hostname: "127.0.0.1",
      port,
      path: "/",
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Content-Length": Buffer.byteLength(body),
        ...headers,
      },
    };

    const req = http.request(options, (res) => {
      let data = "";
      res.on("data", (chunk) => { data += chunk; });
      res.on("end", () => {
        resolve({ statusCode: res.statusCode ?? 0, body: data });
      });
    });

    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

/** Get the actual listening port (useful when port 0 is used). */
function getListeningPort(receiver: WebhookReceiver): number {
  const server = (receiver as any).server as http.Server | null;
  if (!server) return 0;
  const addr = server.address();
  if (addr && typeof addr === "object") return addr.port;
  return 0;
}

// ── Tests ────────────────────────────────────────────────────────

describe("WebhookReceiver", () => {
  let receiver: WebhookReceiver;

  afterEach(async () => {
    if (receiver) {
      await receiver.stop();
    }
  });

  describe("server lifecycle", () => {
    it("starts and stops the HTTP server", async () => {
      const config = createBoardConfig({ webhookPort: 0 });
      receiver = new WebhookReceiver(config);

      const started = await receiver.start();

      expect(started).toBe(true);
      expect(receiver.isRunning()).toBe(true);

      await receiver.stop();

      expect(receiver.isRunning()).toBe(false);
    });

    it("does not start when webhooks are disabled (no webhookPort)", async () => {
      const config = createBoardConfig({ webhookPort: undefined });
      receiver = new WebhookReceiver(config);

      const started = await receiver.start();

      expect(started).toBe(false);
      expect(receiver.isRunning()).toBe(false);
    });

    it("falls back to polling-only mode when port is in use", async () => {
      // Start a server on a specific port first
      const blockingServer = http.createServer();
      const port = await new Promise<number>((resolve) => {
        blockingServer.listen(0, () => {
          const addr = blockingServer.address();
          resolve(typeof addr === "object" && addr ? addr.port : 0);
        });
      });

      try {
        const config = createBoardConfig({ webhookPort: port });
        receiver = new WebhookReceiver(config);

        const fallbackHandler = vi.fn();
        receiver.on("fallback:polling", fallbackHandler);

        const started = await receiver.start();

        expect(started).toBe(false);
        expect(receiver.isRunning()).toBe(false);
        expect(fallbackHandler).toHaveBeenCalledWith({
          port,
          reason: "EADDRINUSE",
        });
      } finally {
        blockingServer.close();
      }
    });

    it("clamps port to valid range (1024-65535)", () => {
      const config = createBoardConfig({ webhookPort: 500 });
      receiver = new WebhookReceiver(config);

      // Should fall back to default port (9876)
      expect(receiver.getPort()).toBe(9876);
    });

    it("clamps port above 65535 to default", () => {
      const config = createBoardConfig({ webhookPort: 70000 });
      receiver = new WebhookReceiver(config);

      expect(receiver.getPort()).toBe(9876);
    });

    it("accepts valid port in range", () => {
      const config = createBoardConfig({ webhookPort: 8080 });
      receiver = new WebhookReceiver(config);

      expect(receiver.getPort()).toBe(8080);
    });
  });

  describe("signature validation", () => {
    it("accepts valid GitHub HMAC-SHA256 signature", async () => {
      const secret = "test-secret-123";
      const config = createBoardConfig({ webhookPort: 0, webhookSecret: secret });
      receiver = new WebhookReceiver(config);
      await receiver.start();

      const port = getListeningPort(receiver);
      const payload = {
        action: "opened",
        issue: {
          number: 1,
          title: "Test Issue",
          body: "Description",
          state: "open",
          html_url: "https://github.com/test/repo/issues/1",
          assignee: null,
          labels: [],
        },
      };

      const body = JSON.stringify(payload);
      const signature = computeGitHubSignature(secret, body);

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issues",
        "X-Hub-Signature-256": signature,
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).toHaveBeenCalledTimes(1);
    });

    it("rejects payload with invalid GitHub signature", async () => {
      const secret = "test-secret-123";
      const config = createBoardConfig({ webhookPort: 0, webhookSecret: secret });
      receiver = new WebhookReceiver(config);
      await receiver.start();

      const port = getListeningPort(receiver);
      const payload = { action: "opened", issue: { number: 1, title: "Test", body: "", state: "open", html_url: "", assignee: null, labels: [] } };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issues",
        "X-Hub-Signature-256": "sha256=0000000000000000000000000000000000000000000000000000000000000000",
      });

      expect(response.statusCode).toBe(401);
      expect(eventHandler).not.toHaveBeenCalled();
    });

    it("rejects payload with missing signature when secret is configured", async () => {
      const secret = "test-secret-123";
      const config = createBoardConfig({ webhookPort: 0, webhookSecret: secret });
      receiver = new WebhookReceiver(config);
      await receiver.start();

      const port = getListeningPort(receiver);
      const payload = { action: "opened", issue: { number: 1, title: "Test", body: "", state: "open", html_url: "", assignee: null, labels: [] } };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issues",
      });

      expect(response.statusCode).toBe(401);
      expect(eventHandler).not.toHaveBeenCalled();
    });

    it("accepts payload without signature when no secret is configured", async () => {
      const config = createBoardConfig({ webhookPort: 0, webhookSecret: undefined });
      receiver = new WebhookReceiver(config);
      await receiver.start();

      const port = getListeningPort(receiver);
      const payload = {
        action: "opened",
        issue: {
          number: 42,
          title: "No Secret Issue",
          body: "Body",
          state: "open",
          html_url: "https://github.com/test/repo/issues/42",
          assignee: null,
          labels: [],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issues",
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).toHaveBeenCalledTimes(1);
    });

    it("accepts valid Jira HMAC-SHA256 signature", async () => {
      const secret = "jira-secret";
      const config = createBoardConfig({
        webhookPort: 0,
        webhookSecret: secret,
        trackerType: "jira",
      });
      receiver = new WebhookReceiver(config);
      await receiver.start();

      const port = getListeningPort(receiver);
      const payload = {
        webhookEvent: "jira:issue_updated",
        issue: {
          key: "PROJ-1",
          self: "https://test.atlassian.net/rest/api/3/issue/10001",
          fields: {
            summary: "Jira Issue",
            description: "Desc",
            status: { name: "In Progress" },
            assignee: { displayName: "User" },
            labels: [],
            priority: { name: "High" },
          },
        },
        changelog: {
          items: [{ field: "status", fromString: "Open", toString: "In Progress" }],
        },
      };

      const body = JSON.stringify(payload);
      const signature = computeJiraSignature(secret, body);

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "User-Agent": "Atlassian Webhook",
        "X-Hub-Signature": signature,
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).toHaveBeenCalledTimes(1);
    });
  });

  describe("GitHub event parsing", () => {
    beforeEach(async () => {
      const config = createBoardConfig({ webhookPort: 0 });
      receiver = new WebhookReceiver(config);
      await receiver.start();
    });

    it("parses issues.opened event", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "opened",
        issue: {
          number: 5,
          title: "New Feature Request",
          body: "Please add this feature",
          state: "open",
          html_url: "https://github.com/test/repo/issues/5",
          assignee: { login: "dev1" },
          labels: [{ name: "enhancement" }, { name: "priority:high" }],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, { "X-GitHub-Event": "issues" });

      expect(eventHandler).toHaveBeenCalledTimes(1);
      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.source).toBe("github");
      expect(event.eventType).toBe("issues.opened");
      expect(event.issue.id).toBe("5");
      expect(event.issue.title).toBe("New Feature Request");
      expect(event.issue.description).toBe("Please add this feature");
      expect(event.issue.assignee).toBe("dev1");
      expect(event.issue.labels).toEqual(["enhancement", "priority:high"]);
    });

    it("parses issues.closed event", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "closed",
        issue: {
          number: 3,
          title: "Bug Fix",
          body: "",
          state: "closed",
          html_url: "https://github.com/test/repo/issues/3",
          assignee: null,
          labels: [],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, { "X-GitHub-Event": "issues" });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.eventType).toBe("issues.closed");
      expect(event.issue.status).toBe("closed");
    });

    it("parses issues.labeled event", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "labeled",
        issue: {
          number: 7,
          title: "Labeled Issue",
          body: "Body",
          state: "open",
          html_url: "https://github.com/test/repo/issues/7",
          assignee: null,
          labels: [{ name: "bug" }, { name: "urgent" }],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, { "X-GitHub-Event": "issues" });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.eventType).toBe("issues.labeled");
      expect(event.issue.labels).toEqual(["bug", "urgent"]);
    });

    it("parses issues.assigned event", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "assigned",
        issue: {
          number: 10,
          title: "Assigned Issue",
          body: "",
          state: "open",
          html_url: "https://github.com/test/repo/issues/10",
          assignee: { login: "assignee-user" },
          labels: [],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, { "X-GitHub-Event": "issues" });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.eventType).toBe("issues.assigned");
      expect(event.issue.assignee).toBe("assignee-user");
    });

    it("parses pull_request_review.submitted event", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "submitted",
        review: {
          state: "approved",
          body: "LGTM",
        },
        pull_request: {
          number: 15,
          title: "Fix login bug",
          body: "Fixes #5",
          state: "open",
          html_url: "https://github.com/test/repo/pull/15",
          assignee: { login: "author" },
          labels: [{ name: "fix" }],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, { "X-GitHub-Event": "pull_request_review" });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.source).toBe("github");
      expect(event.eventType).toBe("pull_request_review.submitted");
      expect(event.issue.id).toBe("15");
      expect(event.issue.title).toBe("Fix login bug");
    });

    it("ignores unsupported GitHub event types", async () => {
      const port = getListeningPort(receiver);
      const payload = { action: "created", comment: { body: "Hello" } };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issue_comment",
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).not.toHaveBeenCalled();
    });

    it("ignores unsupported issue actions", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        action: "pinned",
        issue: { number: 1, title: "T", body: "", state: "open", html_url: "", assignee: null, labels: [] },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "issues",
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).not.toHaveBeenCalled();
    });
  });

  describe("Jira event parsing", () => {
    beforeEach(async () => {
      const config = createBoardConfig({ webhookPort: 0, trackerType: "jira" });
      receiver = new WebhookReceiver(config);
      await receiver.start();
    });

    it("parses jira:issue_updated with status change", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        webhookEvent: "jira:issue_updated",
        issue: {
          key: "PROJ-42",
          self: "https://myteam.atlassian.net/rest/api/3/issue/10042",
          fields: {
            summary: "Implement login page",
            description: "Build the login page with OAuth",
            status: { name: "In Progress" },
            assignee: { displayName: "Jane Doe" },
            labels: ["frontend", "auth"],
            priority: { name: "High" },
          },
        },
        changelog: {
          items: [
            { field: "status", fromString: "To Do", toString: "In Progress" },
          ],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, {
        "User-Agent": "Atlassian Webhook",
      });

      expect(eventHandler).toHaveBeenCalledTimes(1);
      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.source).toBe("jira");
      expect(event.eventType).toBe("jira:issue_updated.status");
      expect(event.issue.id).toBe("PROJ-42");
      expect(event.issue.title).toBe("Implement login page");
      expect(event.issue.status).toBe("In Progress");
      expect(event.issue.assignee).toBe("Jane Doe");
      expect(event.issue.labels).toEqual(["frontend", "auth"]);
      expect(event.issue.priority).toBe("High");
      expect(event.issue.url).toBe("https://myteam.atlassian.net/browse/PROJ-42");
    });

    it("parses jira:issue_updated with assignee change", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        webhookEvent: "jira:issue_updated",
        issue: {
          key: "PROJ-10",
          self: "https://myteam.atlassian.net/rest/api/3/issue/10010",
          fields: {
            summary: "Fix bug",
            description: null,
            status: { name: "Open" },
            assignee: { displayName: "John Smith" },
            labels: [],
            priority: { name: "Medium" },
          },
        },
        changelog: {
          items: [
            { field: "assignee", from: null, to: "john.smith" },
          ],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, {
        "User-Agent": "Atlassian Webhook",
      });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.eventType).toBe("jira:issue_updated.assignee");
      expect(event.issue.assignee).toBe("John Smith");
    });

    it("ignores jira:issue_updated with irrelevant field changes", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        webhookEvent: "jira:issue_updated",
        issue: {
          key: "PROJ-5",
          self: "https://myteam.atlassian.net/rest/api/3/issue/10005",
          fields: {
            summary: "Some issue",
            description: "",
            status: { name: "Open" },
            assignee: null,
            labels: [],
            priority: { name: "Low" },
          },
        },
        changelog: {
          items: [
            { field: "description", fromString: "old", toString: "new" },
          ],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      const response = await sendWebhook(port, payload, {
        "User-Agent": "Atlassian Webhook",
      });

      expect(response.statusCode).toBe(200);
      expect(eventHandler).not.toHaveBeenCalled();
    });

    it("handles Jira ADF description format", async () => {
      const port = getListeningPort(receiver);
      const payload = {
        webhookEvent: "jira:issue_updated",
        issue: {
          key: "PROJ-99",
          self: "https://myteam.atlassian.net/rest/api/3/issue/10099",
          fields: {
            summary: "ADF Issue",
            description: {
              type: "doc",
              content: [
                {
                  type: "paragraph",
                  content: [
                    { type: "text", text: "Hello" },
                    { type: "text", text: " world" },
                  ],
                },
              ],
            },
            status: { name: "Done" },
            assignee: null,
            labels: [],
            priority: { name: "None" },
          },
        },
        changelog: {
          items: [{ field: "status", fromString: "In Progress", toString: "Done" }],
        },
      };

      const eventHandler = vi.fn();
      receiver.on("webhook:event", eventHandler);

      await sendWebhook(port, payload, {
        "User-Agent": "Atlassian Webhook",
      });

      const event: WebhookEvent = eventHandler.mock.calls[0][0];
      expect(event.issue.description).toContain("Hello");
      expect(event.issue.description).toContain("world");
    });
  });

  describe("HTTP handling", () => {
    beforeEach(async () => {
      const config = createBoardConfig({ webhookPort: 0 });
      receiver = new WebhookReceiver(config);
      await receiver.start();
    });

    it("rejects non-POST requests with 405", async () => {
      const port = getListeningPort(receiver);

      const response = await new Promise<{ statusCode: number }>((resolve, reject) => {
        const req = http.request(
          { hostname: "127.0.0.1", port, path: "/", method: "GET" },
          (res) => { resolve({ statusCode: res.statusCode ?? 0 }); },
        );
        req.on("error", reject);
        req.end();
      });

      expect(response.statusCode).toBe(405);
    });

    it("rejects invalid JSON with 400", async () => {
      const port = getListeningPort(receiver);

      const response = await new Promise<{ statusCode: number; body: string }>((resolve, reject) => {
        const body = "not valid json {{{";
        const req = http.request(
          {
            hostname: "127.0.0.1",
            port,
            path: "/",
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "Content-Length": Buffer.byteLength(body),
              "X-GitHub-Event": "issues",
            },
          },
          (res) => {
            let data = "";
            res.on("data", (chunk) => { data += chunk; });
            res.on("end", () => { resolve({ statusCode: res.statusCode ?? 0, body: data }); });
          },
        );
        req.on("error", reject);
        req.write(body);
        req.end();
      });

      expect(response.statusCode).toBe(400);
    });

    it("responds 200 for valid but unrecognized events", async () => {
      const port = getListeningPort(receiver);
      const payload = { action: "something_else" };

      const response = await sendWebhook(port, payload, {
        "X-GitHub-Event": "unknown_event_type",
      });

      expect(response.statusCode).toBe(200);
      const body = JSON.parse(response.body);
      expect(body.events).toBe(0);
    });
  });
});
