import { describe, it, expect, vi } from "vitest";
import { CapabilityNegotiator } from "../electron/ssh-capabilities";
import type { RpcRouter } from "../electron/ssh-router";

const capabilities = { protocol: 1, version: "2.0.0-alpha.1", methods: ["capabilities", "ping", "read_range", "file_materialize"] };

function fixture(result: unknown = capabilities) {
  const sendToRemoteAgent = vi.fn().mockResolvedValue(result);
  const router = { sendToRemoteAgent } as unknown as RpcRouter;
  return { negotiator: new CapabilityNegotiator(router), sendToRemoteAgent };
}

describe("native agent capability negotiation", () => {
  it("queries the attached agent during deployment without repository routing", async () => {
    const { negotiator, sendToRemoteAgent } = fixture();
    await expect(negotiator.negotiate("deploying-profile")).resolves.toEqual(capabilities);
    expect(sendToRemoteAgent).toHaveBeenCalledWith("deploying-profile", "capabilities", {});
    expect(negotiator.isSupported("deploying-profile", "file_materialize")).toBe(true);
    expect(negotiator.isSupported("deploying-profile", "compress_file")).toBe(false);
  });

  it.each([
    null, {}, { ...capabilities, protocol: 2 }, { ...capabilities, version: "" },
    { ...capabilities, methods: ["ping", 1] }, { ...capabilities, methods: ["ping", "ping"] },
    { ...capabilities, methods: [] }, { ...capabilities, methods: ["capabilities", ""] },
  ])("rejects an invalid or incompatible wire contract: %j", async result => {
    const { negotiator } = fixture(result);
    await expect(negotiator.negotiate("profile")).rejects.toThrow("Incompatible Crab agent capabilities");
    expect(negotiator.getCached("profile")).toBeNull();
  });

  it("invalidates stale capabilities before a failed renegotiation", async () => {
    const { negotiator, sendToRemoteAgent } = fixture();
    await negotiator.negotiate("profile");
    sendToRemoteAgent.mockRejectedValueOnce(new Error("Agent exited"));
    await expect(negotiator.negotiate("profile")).rejects.toThrow("Agent exited");
    expect(negotiator.isSupported("profile", "read_range")).toBe(false);
  });

  it("does not resurrect capabilities after the session was cleared", async () => {
    const { negotiator, sendToRemoteAgent } = fixture();
    let complete!: (value: unknown) => void;
    sendToRemoteAgent.mockReturnValueOnce(new Promise(resolve => { complete = resolve; }));
    const pending = negotiator.negotiate("profile");
    negotiator.clearCache("profile");
    complete(capabilities);
    await expect(pending).rejects.toThrow("superseded");
    expect(negotiator.getCached("profile")).toBeNull();
  });

  it("requires every RPC used by an operation", async () => {
    const { negotiator } = fixture();
    await negotiator.negotiate("profile");
    expect(negotiator.isSupported("profile", "read_range", "file_materialize")).toBe(true);
    expect(negotiator.isSupported("profile", "read_range", "file_close")).toBe(false);
  });

  it("keeps capabilities scoped to the connected profile", async () => {
    const { negotiator } = fixture();
    await negotiator.negotiate("profile");
    expect(negotiator.isSupported("another-profile", "read_range")).toBe(false);
    negotiator.clearCache("profile");
    expect(negotiator.getCached("profile")).toBeNull();
  });
});
