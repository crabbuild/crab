import { afterEach, describe, expect, it, vi } from "vitest";
import { rpcReply } from "../electron/rpc-transport";
import { call } from "../src/ipc";

afterEach(() => vi.unstubAllGlobals());

describe("RPC error transport", () => {
  it("carries native classifications as cloneable data and restores renderer Error", async () => {
    const detail = { code: 1012, data: { kind: "content_read", code: "integrity" } };
    const native = Object.assign(new Error("Content hash differs"), detail);
    const reply = structuredClone(await rpcReply(async () => { throw native; }));
    expect(reply).toEqual({ ok: false, error: { message: native.message, ...detail } });
    if (reply.ok) throw new Error("Expected failure reply");
    vi.stubGlobal("window", { crab: { rpc: vi.fn().mockRejectedValue(reply.error) } });
    const result = call("preview_safetensors", {});
    await expect(result).rejects.toBeInstanceOf(Error);
    await expect(result).rejects.toMatchObject({ message: native.message, ...detail });
  });

  it("preserves successful payloads without interpreting their own error fields", async () => {
    const value = { ok: false, error: "A domain value", data: [1, 2] };
    expect(await rpcReply(async () => value)).toEqual({ ok: true, value });
  });

  it("preserves ordinary thrown messages", async () => {
    expect(await rpcReply(async () => { throw new Error("Agent unavailable"); })).toEqual({ ok: false, error: { message: "Agent unavailable" } });
  });
});
