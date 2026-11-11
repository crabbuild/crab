import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { AgentEvent } from "../src/ipc";

const fixture = vi.hoisted(() => ({
  receive: (_event: AgentEvent) => {},
  notifications: { doNotDisturb: false, doNotDisturbUntil: null as number | null, muteAgentErrors: false },
  publish: vi.fn(() => "toast"),
  dismiss: vi.fn(),
}));
vi.mock("../src/ipc", () => ({ onAgentEvent: (receive: typeof fixture.receive) => { fixture.receive = receive; return () => {}; } }));
vi.mock("../src/settings", () => ({ getSettings: () => ({ notifications: fixture.notifications }) }));
vi.mock("../src/lib/toasts", () => ({ publishToast: fixture.publish, dismissToast: fixture.dismiss }));
import { setupAgentErrorPublisher, setupJobCompletionPublisher } from "../src/lib/toast-publishers";

let unsubscribe: () => void;
beforeEach(() => {
  vi.clearAllMocks();
  fixture.notifications.doNotDisturb = false;
  fixture.notifications.muteAgentErrors = false;
  unsubscribe = setupAgentErrorPublisher();
});
afterEach(() => unsubscribe());

it("cancellation feedback preserves publication uncertainty for partial batches", () => {
  unsubscribe(); unsubscribe = setupJobCompletionPublisher();
  fixture.receive({ method: "progress/complete", params: { op: "hydrate", job_id: "partial", status: -1, outcome: "cancelled" } });
  expect(fixture.publish).toHaveBeenCalledExactlyOnceWith({ kind: "info", title: "Hydrate cancelled", body: "The operation stopped. Files already published remain in place." });
});

it("negotiated local recovery clears the outage without dismissing unrelated errors", () => {
  fixture.publish.mockReturnValueOnce("outage").mockReturnValueOnce("other-error");
  fixture.receive({ method: "agent/failed", params: { message: "Execution denied" } });
  fixture.receive({ method: "agent/error", params: { message: "Operation failed" } });
  fixture.receive({ method: "agent/ready", params: {} });
  expect(fixture.dismiss.mock.calls).toEqual([["outage"]]);
});

it("recovery retires a visible outage even after notifications are muted", () => {
  fixture.publish.mockReturnValueOnce("outage");
  fixture.receive({ method: "agent/failed", params: { message: "Missing runtime" } });
  fixture.notifications.doNotDisturb = true;
  fixture.notifications.muteAgentErrors = true;
  fixture.receive({ method: "agent/ready", params: {} });
  expect(fixture.dismiss.mock.calls).toEqual([["outage"]]);
});

it("recovery clears a crash notice while preserving unrelated operation errors", () => {
  fixture.publish.mockReturnValueOnce("crash").mockReturnValueOnce("operation");
  fixture.receive({ method: "agent/exit", params: { code: null, signal: "SIGKILL" } });
  fixture.receive({ method: "agent/error", params: { message: "Operation failed" } });
  fixture.receive({ method: "agent/ready", params: {} });
  expect(fixture.dismiss.mock.calls).toEqual([["crash"]]);
});

it("a failed restart replaces its crash notice and recovery clears it while muted", () => {
  fixture.publish.mockReturnValueOnce("crash").mockReturnValueOnce("restart");
  fixture.receive({ method: "agent/exit", params: { code: 1, signal: null } });
  fixture.receive({ method: "agent/failed", params: { message: "Restart failed" } });
  fixture.notifications.muteAgentErrors = true;
  fixture.receive({ method: "agent/ready", params: {} });
  expect(fixture.dismiss.mock.calls).toEqual([["crash"], ["restart"]]);
});
