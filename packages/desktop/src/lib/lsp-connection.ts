/**
 * Renderer-side LSP connection that wraps the IPC bridge.
 *
 * Correlates request/response by ID, routes notifications to handlers,
 * and implements request timeouts. One instance per language server.
 */

import type { LspMessage, LspNotificationMessage, LspResponseMessage } from "./lsp-types";

interface PendingRequest {
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export class LspConnection {
  private requestId = 0;
  private pending = new Map<number, PendingRequest>();
  private notificationHandlers = new Map<string, Array<(params: unknown) => void>>();
  private unsubscribeMessage: (() => void) | null = null;
  private disposed = false;

  constructor(private language: string) {
    this.unsubscribeMessage = window.crab.lsp.onMessage(({ language: lang, message }) => {
      if (lang !== this.language) return;
      try {
        const parsed = JSON.parse(message) as LspMessage;
        this.handleMessage(parsed);
      } catch {
        // Malformed message — ignore
      }
    });
  }

  /**
   * Send a request and await the response.
   * Rejects on timeout or if the server returns an error.
   */
  request(method: string, params: unknown, timeout = 5000): Promise<unknown> {
    if (this.disposed) {
      return Promise.reject(new Error("LspConnection disposed"));
    }

    const id = ++this.requestId;
    const message = JSON.stringify({
      jsonrpc: "2.0",
      id,
      method,
      params,
    });

    return new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`LSP request '${method}' timed out after ${timeout}ms`));
      }, timeout);

      this.pending.set(id, { resolve, reject, timer });
      window.crab.lsp.send(this.language, message);
    });
  }

  /**
   * Send a notification (fire-and-forget, no response expected).
   */
  notify(method: string, params: unknown): void {
    if (this.disposed) return;

    const message = JSON.stringify({
      jsonrpc: "2.0",
      method,
      params,
    });
    window.crab.lsp.send(this.language, message);
  }

  /**
   * Register a handler for server-initiated notifications.
   * Returns an unsubscribe function.
   */
  onNotification(method: string, handler: (params: unknown) => void): () => void {
    const handlers = this.notificationHandlers.get(method) ?? [];
    handlers.push(handler);
    this.notificationHandlers.set(method, handlers);

    return () => {
      const current = this.notificationHandlers.get(method);
      if (current) {
        const idx = current.indexOf(handler);
        if (idx >= 0) current.splice(idx, 1);
      }
    };
  }

  /**
   * Clean up IPC listener and reject all pending requests.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    // Unsubscribe from IPC
    if (this.unsubscribeMessage) {
      this.unsubscribeMessage();
      this.unsubscribeMessage = null;
    }

    // Reject all pending requests
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(new Error("LspConnection disposed"));
      this.pending.delete(id);
    }

    // Clear notification handlers
    this.notificationHandlers.clear();
  }

  private handleMessage(msg: LspMessage): void {
    // Response to a request (has id + result/error)
    if ("id" in msg && msg.id != null) {
      const response = msg as LspResponseMessage;
      const pending = this.pending.get(response.id);
      if (!pending) return;

      clearTimeout(pending.timer);
      this.pending.delete(response.id);

      if (response.error) {
        pending.reject(new Error(response.error.message));
      } else {
        pending.resolve(response.result);
      }
      return;
    }

    // Notification from server (has method, no id)
    if ("method" in msg) {
      const notification = msg as LspNotificationMessage;
      const handlers = this.notificationHandlers.get(notification.method);
      if (handlers) {
        for (const handler of handlers) {
          try {
            handler(notification.params);
          } catch {
            // Don't let a handler error break the message pump
          }
        }
      }
    }
  }
}
