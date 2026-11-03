/**
 * Webhook Receiver for the Kanban subsystem.
 *
 * Optional HTTP server that listens for real-time event payloads from
 * external trackers (GitHub, Jira). Validates HMAC-SHA256 signatures
 * against a configured secret and parses provider-specific payloads into
 * normalized board update events. Falls back to polling-only mode if the
 * configured port is already in use.
 */

import { EventEmitter } from "node:events";
import * as http from "node:http";
import * as crypto from "node:crypto";
import log from "electron-log";

import type { BoardConfig, TrackerSource } from "./kanban-types";
import type { ExternalIssue } from "./tracker-adapters/types";

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_WEBHOOK_PORT = 9876;
const MIN_PORT = 1024;
const MAX_PORT = 65535;
const MAX_PAYLOAD_BYTES = 5 * 1024 * 1024; // 5 MB

// ── Types ────────────────────────────────────────────────────────

export interface WebhookEvent {
  source: TrackerSource;
  eventType: string;
  issue: ExternalIssue;
  timestamp: number;
}

export interface WebhookReceiverConfig {
  port: number;
  secret?: string;
  enabled: boolean;
}

// ── WebhookReceiver ──────────────────────────────────────────────

export class WebhookReceiver extends EventEmitter {
  private server: http.Server | null = null;
  private config: WebhookReceiverConfig;
  private boardConfig: BoardConfig;
  private running = false;

  constructor(boardConfig: BoardConfig) {
    super();
    this.boardConfig = boardConfig;
    this.config = {
      port: this.clampPort(boardConfig.webhookPort ?? DEFAULT_WEBHOOK_PORT),
      secret: boardConfig.webhookSecret,
      enabled: boardConfig.webhookPort !== undefined,
    };
  }

  /** Start the HTTP server. Falls back to polling-only if port is in use. */
  async start(): Promise<boolean> {
    if (!this.config.enabled) {
      log.info("[webhook-receiver] Webhooks disabled, skipping server start");
      return false;
    }

    if (this.running) return true;

    return new Promise<boolean>((resolve) => {
      this.server = http.createServer((req, res) => {
        this.handleRequest(req, res);
      });

      // Disable keep-alive to ensure clean shutdown
      this.server.keepAliveTimeout = 0;

      this.server.on("error", (err: NodeJS.ErrnoException) => {
        if (err.code === "EADDRINUSE") {
          log.warn(
            `[webhook-receiver] Port ${this.config.port} is already in use. Falling back to polling-only mode.`,
          );
          this.server = null;
          this.running = false;
          this.emit("fallback:polling", { port: this.config.port, reason: "EADDRINUSE" });
          resolve(false);
        } else {
          log.error(
            `[webhook-receiver] Server error: ${err.message}`,
          );
          this.server = null;
          this.running = false;
          resolve(false);
        }
      });

      this.server.listen(this.config.port, () => {
        this.running = true;
        log.info(`[webhook-receiver] Listening on port ${this.config.port}`);
        resolve(true);
      });
    });
  }

  /** Gracefully stop the HTTP server. */
  async stop(): Promise<void> {
    if (!this.server) return;

    return new Promise<void>((resolve) => {
      this.server!.closeAllConnections();
      this.server!.close(() => {
        this.running = false;
        this.server = null;
        log.info("[webhook-receiver] Server stopped");
        resolve();
      });
    });
  }

  /** Whether the server is currently running. */
  isRunning(): boolean {
    return this.running;
  }

  /** Returns the configured port. */
  getPort(): number {
    return this.config.port;
  }

  // ── Request Handling ───────────────────────────────────────────

  private handleRequest(req: http.IncomingMessage, res: http.ServerResponse): void {
    // Disable keep-alive for webhook requests
    res.setHeader("Connection", "close");

    // Only accept POST requests
    if (req.method !== "POST") {
      res.writeHead(405, { "Content-Type": "text/plain" });
      res.end("Method Not Allowed");
      return;
    }

    const chunks: Buffer[] = [];
    let totalBytes = 0;

    req.on("data", (chunk: Buffer) => {
      totalBytes += chunk.length;
      if (totalBytes > MAX_PAYLOAD_BYTES) {
        res.writeHead(413, { "Content-Type": "text/plain" });
        res.end("Payload Too Large");
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });

    req.on("end", () => {
      const body = Buffer.concat(chunks);
      const source = this.detectSource(req);

      // Validate signature if secret is configured
      if (this.config.secret) {
        const valid = this.validateSignature(req, body, source);
        if (!valid) {
          log.warn(
            `[webhook-receiver] Rejected payload: invalid or missing signature (source=${source})`,
          );
          res.writeHead(401, { "Content-Type": "text/plain" });
          res.end("Unauthorized: Invalid signature");
          return;
        }
      }

      // Parse the payload
      let payload: unknown;
      try {
        payload = JSON.parse(body.toString("utf-8"));
      } catch {
        log.warn("[webhook-receiver] Rejected payload: invalid JSON");
        res.writeHead(400, { "Content-Type": "text/plain" });
        res.end("Bad Request: Invalid JSON");
        return;
      }

      // Route to the appropriate parser
      const events = this.parsePayload(source, req, payload);

      if (events.length > 0) {
        for (const event of events) {
          this.emit("webhook:event", event);
        }
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ received: true, events: events.length }));
      } else {
        // Event type not relevant — acknowledge but don't emit
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ received: true, events: 0 }));
      }
    });

    req.on("error", (err) => {
      log.error(`[webhook-receiver] Request error: ${err.message}`);
      if (!res.headersSent) {
        res.writeHead(500, { "Content-Type": "text/plain" });
        res.end("Internal Server Error");
      }
    });
  }

  // ── Source Detection ───────────────────────────────────────────

  /**
   * Detects the webhook source based on request headers.
   * GitHub sends X-GitHub-Event; Jira sends a user-agent containing "Atlassian".
   */
  private detectSource(req: http.IncomingMessage): TrackerSource {
    if (req.headers["x-github-event"]) {
      return "github";
    }

    const userAgent = req.headers["user-agent"] ?? "";
    if (userAgent.toLowerCase().includes("atlassian") || req.headers["x-atlassian-webhook-identifier"]) {
      return "jira";
    }

    // Default to the board's configured tracker type
    return this.boardConfig.trackerType;
  }

  // ── Signature Validation ───────────────────────────────────────

  /**
   * Validates the HMAC-SHA256 signature of the incoming payload.
   * GitHub uses X-Hub-Signature-256 with format "sha256=<hex>".
   * Jira uses X-Hub-Signature (or custom header) with the same format.
   */
  private validateSignature(
    req: http.IncomingMessage,
    body: Buffer,
    source: TrackerSource,
  ): boolean {
    const secret = this.config.secret;
    if (!secret) return true;

    const signatureHeader = this.getSignatureHeader(req, source);
    if (!signatureHeader) {
      return false;
    }

    const expectedSignature = this.computeHmacSha256(secret, body);

    // Both GitHub and Jira use "sha256=<hex>" format
    const providedHex = this.extractHexFromSignature(signatureHeader);
    if (!providedHex) {
      return false;
    }

    // Use timing-safe comparison to prevent timing attacks
    try {
      const expectedBuf = Buffer.from(expectedSignature, "hex");
      const providedBuf = Buffer.from(providedHex, "hex");

      if (expectedBuf.length !== providedBuf.length) {
        return false;
      }

      return crypto.timingSafeEqual(expectedBuf, providedBuf);
    } catch {
      return false;
    }
  }

  /**
   * Retrieves the appropriate signature header based on the source.
   */
  private getSignatureHeader(req: http.IncomingMessage, source: TrackerSource): string | undefined {
    if (source === "github") {
      // GitHub uses X-Hub-Signature-256
      const header = req.headers["x-hub-signature-256"];
      return typeof header === "string" ? header : undefined;
    }

    // Jira uses X-Hub-Signature (standard webhook signature header)
    const jiraHeader = req.headers["x-hub-signature"];
    if (typeof jiraHeader === "string") return jiraHeader;

    // Fallback: check X-Hub-Signature-256 for Jira as well
    const fallbackHeader = req.headers["x-hub-signature-256"];
    return typeof fallbackHeader === "string" ? fallbackHeader : undefined;
  }

  /**
   * Computes HMAC-SHA256 of the body using the configured secret.
   */
  private computeHmacSha256(secret: string, body: Buffer): string {
    return crypto.createHmac("sha256", secret).update(body).digest("hex");
  }

  /**
   * Extracts the hex digest from a signature header value.
   * Supports formats: "sha256=<hex>" or raw hex.
   */
  private extractHexFromSignature(signature: string): string | undefined {
    if (signature.startsWith("sha256=")) {
      return signature.slice(7);
    }
    // If it looks like a raw hex string (64 chars for SHA-256)
    if (/^[0-9a-f]{64}$/i.test(signature)) {
      return signature.toLowerCase();
    }
    return undefined;
  }

  // ── Payload Parsing ────────────────────────────────────────────

  private parsePayload(
    source: TrackerSource,
    req: http.IncomingMessage,
    payload: unknown,
  ): WebhookEvent[] {
    switch (source) {
      case "github":
        return this.parseGitHubPayload(req, payload);
      case "jira":
        return this.parseJiraPayload(payload);
      default:
        log.debug(`[webhook-receiver] Unsupported source: ${source}`);
        return [];
    }
  }

  // ── GitHub Payload Parsing ─────────────────────────────────────

  /**
   * Parses GitHub webhook payloads for supported events:
   * - issues: opened, closed, labeled, assigned
   * - pull_request_review: submitted
   */
  private parseGitHubPayload(
    req: http.IncomingMessage,
    payload: unknown,
  ): WebhookEvent[] {
    const eventType = req.headers["x-github-event"] as string | undefined;
    if (!eventType) return [];

    const data = payload as Record<string, unknown>;

    switch (eventType) {
      case "issues":
        return this.parseGitHubIssueEvent(data);
      case "pull_request_review":
        return this.parseGitHubPullRequestReviewEvent(data);
      default:
        return [];
    }
  }

  private parseGitHubIssueEvent(data: Record<string, unknown>): WebhookEvent[] {
    const action = data.action as string | undefined;
    if (!action) return [];

    const supportedActions = ["opened", "closed", "labeled", "assigned"];
    if (!supportedActions.includes(action)) return [];

    const issue = data.issue as Record<string, unknown> | undefined;
    if (!issue) return [];

    const externalIssue = this.normalizeGitHubIssue(issue);
    if (!externalIssue) return [];

    return [
      {
        source: "github",
        eventType: `issues.${action}`,
        issue: externalIssue,
        timestamp: Date.now(),
      },
    ];
  }

  private parseGitHubPullRequestReviewEvent(data: Record<string, unknown>): WebhookEvent[] {
    const action = data.action as string | undefined;
    if (action !== "submitted") return [];

    const review = data.review as Record<string, unknown> | undefined;
    const pullRequest = data.pull_request as Record<string, unknown> | undefined;
    if (!review || !pullRequest) return [];

    // Extract the linked issue from the PR body or title if possible
    // For now, emit a synthetic issue representing the PR
    const prNumber = pullRequest.number as number | undefined;
    const prTitle = (pullRequest.title as string) ?? "";
    const prUrl = (pullRequest.html_url as string) ?? "";
    const prState = (pullRequest.state as string) ?? "open";

    const externalIssue: ExternalIssue = {
      id: String(prNumber ?? ""),
      title: prTitle,
      description: (pullRequest.body as string) ?? "",
      status: prState,
      assignee: this.extractGitHubAssignee(pullRequest),
      labels: this.extractGitHubLabels(pullRequest),
      priority: "none",
      url: prUrl,
      updatedAt: Date.now(),
    };

    return [
      {
        source: "github",
        eventType: "pull_request_review.submitted",
        issue: externalIssue,
        timestamp: Date.now(),
      },
    ];
  }

  private normalizeGitHubIssue(issue: Record<string, unknown>): ExternalIssue | null {
    const number = issue.number as number | undefined;
    if (number === undefined) return null;

    const state = (issue.state as string) ?? "open";
    const title = (issue.title as string) ?? "";
    const body = (issue.body as string) ?? "";
    const htmlUrl = (issue.html_url as string) ?? "";

    return {
      id: String(number),
      title,
      description: body,
      status: state,
      assignee: this.extractGitHubAssignee(issue),
      labels: this.extractGitHubLabels(issue),
      priority: "none",
      url: htmlUrl,
      updatedAt: Date.now(),
    };
  }

  private extractGitHubAssignee(obj: Record<string, unknown>): string | undefined {
    const assignee = obj.assignee as Record<string, unknown> | null | undefined;
    if (!assignee) return undefined;
    return (assignee.login as string) ?? undefined;
  }

  private extractGitHubLabels(obj: Record<string, unknown>): string[] {
    const labels = obj.labels as Array<Record<string, unknown>> | undefined;
    if (!Array.isArray(labels)) return [];
    return labels
      .map((l) => (l.name as string) ?? "")
      .filter((name) => name.length > 0);
  }

  // ── Jira Payload Parsing ───────────────────────────────────────

  /**
   * Parses Jira webhook payloads for supported events:
   * - jira:issue_updated (status change, assignee change)
   */
  private parseJiraPayload(payload: unknown): WebhookEvent[] {
    const data = payload as Record<string, unknown>;
    const webhookEvent = data.webhookEvent as string | undefined;

    if (!webhookEvent) return [];

    switch (webhookEvent) {
      case "jira:issue_updated":
        return this.parseJiraIssueUpdated(data);
      case "jira:issue_created":
        return this.parseJiraIssueCreated(data);
      default:
        return [];
    }
  }

  private parseJiraIssueUpdated(data: Record<string, unknown>): WebhookEvent[] {
    const issue = data.issue as Record<string, unknown> | undefined;
    if (!issue) return [];

    const changelog = data.changelog as Record<string, unknown> | undefined;
    if (!changelog) return [];

    const items = (changelog.items as Array<Record<string, unknown>>) ?? [];

    // Only process status changes and assignee changes
    const relevantChanges = items.filter((item) => {
      const field = (item.field as string) ?? "";
      return field === "status" || field === "assignee";
    });

    if (relevantChanges.length === 0) return [];

    const externalIssue = this.normalizeJiraIssue(issue);
    if (!externalIssue) return [];

    // Determine the event type based on what changed
    const hasStatusChange = relevantChanges.some((item) => (item.field as string) === "status");
    const hasAssigneeChange = relevantChanges.some((item) => (item.field as string) === "assignee");

    const eventType = hasStatusChange
      ? "jira:issue_updated.status"
      : hasAssigneeChange
        ? "jira:issue_updated.assignee"
        : "jira:issue_updated";

    return [
      {
        source: "jira",
        eventType,
        issue: externalIssue,
        timestamp: Date.now(),
      },
    ];
  }

  private parseJiraIssueCreated(data: Record<string, unknown>): WebhookEvent[] {
    const issue = data.issue as Record<string, unknown> | undefined;
    if (!issue) return [];

    const externalIssue = this.normalizeJiraIssue(issue);
    if (!externalIssue) return [];

    return [
      {
        source: "jira",
        eventType: "jira:issue_created",
        issue: externalIssue,
        timestamp: Date.now(),
      },
    ];
  }

  private normalizeJiraIssue(issue: Record<string, unknown>): ExternalIssue | null {
    const key = issue.key as string | undefined;
    if (!key) return null;

    const fields = issue.fields as Record<string, unknown> | undefined;
    if (!fields) return null;

    const summary = (fields.summary as string) ?? "";
    const description = this.extractJiraDescription(fields.description);
    const status = this.extractJiraStatus(fields.status);
    const assignee = this.extractJiraAssignee(fields.assignee);
    const labels = (fields.labels as string[]) ?? [];
    const priority = this.extractJiraPriority(fields.priority);
    const self = (issue.self as string) ?? "";

    // Construct a browsable URL from the self link
    const url = this.constructJiraBrowseUrl(self, key);

    return {
      id: key,
      title: summary,
      description,
      status,
      assignee,
      labels,
      priority,
      url,
      updatedAt: Date.now(),
    };
  }

  private extractJiraDescription(description: unknown): string {
    if (typeof description === "string") return description;
    // Jira Cloud uses ADF (Atlassian Document Format) — extract text content
    if (description && typeof description === "object") {
      return this.flattenAdfToText(description as Record<string, unknown>);
    }
    return "";
  }

  private flattenAdfToText(node: Record<string, unknown>): string {
    if (node.type === "text") {
      return (node.text as string) ?? "";
    }
    const content = node.content as Array<Record<string, unknown>> | undefined;
    if (!Array.isArray(content)) return "";
    return content.map((child) => this.flattenAdfToText(child)).join(" ");
  }

  private extractJiraStatus(status: unknown): string {
    if (!status || typeof status !== "object") return "unknown";
    return ((status as Record<string, unknown>).name as string) ?? "unknown";
  }

  private extractJiraAssignee(assignee: unknown): string | undefined {
    if (!assignee || typeof assignee !== "object") return undefined;
    const obj = assignee as Record<string, unknown>;
    return (obj.displayName as string) ?? (obj.name as string) ?? undefined;
  }

  private extractJiraPriority(priority: unknown): string {
    if (!priority || typeof priority !== "object") return "none";
    return ((priority as Record<string, unknown>).name as string) ?? "none";
  }

  private constructJiraBrowseUrl(selfUrl: string, key: string): string {
    // self URL looks like: https://instance.atlassian.net/rest/api/3/issue/12345
    // We want: https://instance.atlassian.net/browse/PROJ-123
    try {
      const url = new URL(selfUrl);
      return `${url.origin}/browse/${key}`;
    } catch {
      return selfUrl;
    }
  }

  // ── Helpers ────────────────────────────────────────────────────

  private clampPort(port: number): number {
    if (!Number.isInteger(port) || port < MIN_PORT) return DEFAULT_WEBHOOK_PORT;
    if (port > MAX_PORT) return DEFAULT_WEBHOOK_PORT;
    return port;
  }
}
