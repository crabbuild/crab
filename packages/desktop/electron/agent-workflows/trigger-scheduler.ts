import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";

import type { AgentWorkflowEvent, AgentWorkflowTrigger } from "./types";
import {
  AgentWorkflowDefinitionStore,
  AgentWorkflowRunStore,
  AgentWorkflowTriggerStore,
} from "./persistence";
import type { AgentWorkflowRunner } from "./runner";

interface TriggerSchedulerEvents {
  "trigger-event": (event: AgentWorkflowEvent) => void;
}

export class AgentWorkflowTriggerScheduler extends EventEmitter {
  private timer: ReturnType<typeof setInterval> | null = null;
  private ticking = false;

  constructor(
    private readonly definitionStore: AgentWorkflowDefinitionStore,
    private readonly triggerStore: AgentWorkflowTriggerStore,
    private readonly runStore: AgentWorkflowRunStore,
    private readonly runner: AgentWorkflowRunner,
    private readonly intervalMs = 30_000,
  ) {
    super();
  }

  override on<K extends keyof TriggerSchedulerEvents>(eventName: K, listener: TriggerSchedulerEvents[K]): this {
    return super.on(eventName, listener);
  }

  start(): void {
    if (this.timer) return;
    this.timer = setInterval(() => {
      this.tick().catch((err) => {
        this.emitTriggerEvent("unknown", "trigger_failed", `Trigger scheduler failed: ${errorMessage(err)}`);
      });
    }, this.intervalMs);
    this.tick().catch(() => {});
  }

  stop(): void {
    if (!this.timer) return;
    clearInterval(this.timer);
    this.timer = null;
  }

  async tick(now = Date.now()): Promise<void> {
    if (this.ticking) return;
    this.ticking = true;
    try {
      const triggers = await this.triggerStore.list();
      for (const trigger of triggers) {
        if (!trigger.enabled) continue;
        await this.fireDueTrigger(trigger, now);
      }
    } finally {
      this.ticking = false;
    }
  }

  private async fireDueTrigger(trigger: AgentWorkflowTrigger, now: number): Promise<void> {
    const nextScheduledAt = nextDueOccurrence(trigger, now);
    if (nextScheduledAt === null) return;

    const duplicate = (await this.runStore.list(trigger.repoPath)).some((run) =>
      run.trigger?.type === "schedule"
      && run.trigger.triggerId === trigger.id
      && run.trigger.scheduledAt === nextScheduledAt,
    );
    if (duplicate) {
      await this.triggerStore.markFired(trigger.id, nextScheduledAt, trigger.lastRunId ?? "");
      this.emitTriggerEvent(trigger.id, "trigger_skipped", `${trigger.name} skipped duplicate scheduled occurrence.`, {
        scheduledAt: nextScheduledAt,
      });
      return;
    }

    const definition = await this.definitionStore.get(trigger.repoPath, trigger.workflowDefinitionId);
    if (!definition) {
      this.emitTriggerEvent(trigger.id, "trigger_failed", `${trigger.name} references a missing workflow definition.`, {
        workflowDefinitionId: trigger.workflowDefinitionId,
      });
      return;
    }

    const run = await this.runner.startRun(trigger.repoPath, definition, {
      input: trigger.input,
      trigger: {
        type: "schedule",
        triggerId: trigger.id,
        scheduledAt: nextScheduledAt,
      },
    });
    await this.triggerStore.markFired(trigger.id, nextScheduledAt, run.id);
    this.emitTriggerEvent(trigger.id, "trigger_fired", `${trigger.name} started a scheduled workflow run.`, {
      runId: run.id,
      scheduledAt: nextScheduledAt,
    });
  }

  private emitTriggerEvent(
    triggerId: string,
    type: AgentWorkflowEvent["type"],
    message: string,
    payload?: Record<string, unknown>,
  ): void {
    this.emit("trigger-event", {
      id: randomUUID(),
      runId: triggerId,
      type,
      message,
      timestamp: Date.now(),
      payload,
    });
  }
}

export function nextDueOccurrence(trigger: AgentWorkflowTrigger, now: number): number | null {
  const intervalMs = intervalMsFromRrule(trigger.rrule);
  if (!intervalMs) return null;
  const base = trigger.lastScheduledAt ?? trigger.createdAt;
  const next = base + intervalMs;
  return next <= now ? next : null;
}

export function intervalMsFromRrule(rrule: string): number | null {
  const parts = new Map(
    rrule
      .split(";")
      .map((part) => part.split("="))
      .filter((pair): pair is [string, string] => pair.length === 2)
      .map(([key, value]) => [key.trim().toUpperCase(), value.trim().toUpperCase()]),
  );
  const interval = Math.max(1, Number(parts.get("INTERVAL") ?? "1") || 1);
  switch (parts.get("FREQ")) {
    case "MINUTELY":
      return interval * 60_000;
    case "HOURLY":
      return interval * 60 * 60_000;
    case "DAILY":
      return interval * 24 * 60 * 60_000;
    case "WEEKLY":
      return interval * 7 * 24 * 60 * 60_000;
    default:
      return null;
  }
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
