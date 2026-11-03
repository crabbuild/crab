import { describe, it, expect } from "vitest";
import {
  encodeFrame,
  FrameDecoder,
  DaemonProtocolError,
  PROTOCOL_VERSION,
  NOTIFY_PREFIX,
  MAX_FRAME_SIZE,
} from "../electron/daemon/protocol";

describe("protocol constants", () => {
  it("PROTOCOL_VERSION is a positive integer", () => {
    expect(PROTOCOL_VERSION).toBe(2);
    expect(Number.isInteger(PROTOCOL_VERSION)).toBe(true);
  });

  it("NOTIFY_PREFIX is notify_", () => {
    expect(NOTIFY_PREFIX).toBe("notify_");
  });
});

describe("encodeFrame", () => {
  it("produces correct byte layout for JSON-only frame", () => {
    const json = { type: "ping", id: "1" };
    const frame = encodeFrame(json);

    const jsonBytes = Buffer.from(JSON.stringify(json), "utf-8");
    const expectedTotalLen = 4 + jsonBytes.length; // jsonLen field + json bytes

    // First 4 bytes: totalLen (u32 BE)
    expect(frame.readUInt32BE(0)).toBe(expectedTotalLen);
    // Next 4 bytes: jsonLen (u32 BE)
    expect(frame.readUInt32BE(4)).toBe(jsonBytes.length);
    // JSON content
    const extractedJson = frame.subarray(8, 8 + jsonBytes.length).toString("utf-8");
    expect(JSON.parse(extractedJson)).toEqual(json);
    // Total frame size
    expect(frame.length).toBe(8 + jsonBytes.length);
  });

  it("produces correct byte layout with binary payload", () => {
    const json = { type: "event", event: "data", sessionId: "s1" };
    const payload = Buffer.from([0x00, 0x01, 0x02, 0xff, 0xfe]);
    const frame = encodeFrame(json, payload);

    const jsonBytes = Buffer.from(JSON.stringify(json), "utf-8");
    const expectedTotalLen = 4 + jsonBytes.length + payload.length;

    expect(frame.readUInt32BE(0)).toBe(expectedTotalLen);
    expect(frame.readUInt32BE(4)).toBe(jsonBytes.length);

    // Verify payload bytes at the tail
    const extractedPayload = frame.subarray(8 + jsonBytes.length);
    expect(Buffer.compare(extractedPayload, payload)).toBe(0);
  });

  it("handles empty payload (zero-payload frame)", () => {
    const json = { id: "2", type: "listSessions" };
    const frame = encodeFrame(json, Buffer.alloc(0));

    const jsonBytes = Buffer.from(JSON.stringify(json), "utf-8");
    const expectedTotalLen = 4 + jsonBytes.length; // no payload bytes

    expect(frame.readUInt32BE(0)).toBe(expectedTotalLen);
    expect(frame.length).toBe(8 + jsonBytes.length);
  });
});

describe("FrameDecoder", () => {
  it("decodes a single complete frame", () => {
    const decoder = new FrameDecoder();
    const json = { hello: "world" };
    const frame = encodeFrame(json);

    decoder.push(frame);
    const frames = decoder.frames();

    expect(frames).toHaveLength(1);
    expect(frames[0].json).toEqual(json);
    expect(frames[0].payload).toBeNull();
  });

  it("decodes a frame with binary payload", () => {
    const decoder = new FrameDecoder();
    const json = { type: "data" };
    const payload = Buffer.from("hello binary world");
    const frame = encodeFrame(json, payload);

    decoder.push(frame);
    const frames = decoder.frames();

    expect(frames).toHaveLength(1);
    expect(frames[0].json).toEqual(json);
    expect(frames[0].payload).not.toBeNull();
    expect(Buffer.compare(frames[0].payload!, payload)).toBe(0);
  });

  it("handles split chunks (frame arrives in multiple pieces)", () => {
    const decoder = new FrameDecoder();
    const json = { type: "ping", id: "42" };
    const payload = Buffer.from("some data here");
    const frame = encodeFrame(json, payload);

    // Split the frame into 3 arbitrary chunks
    const chunk1 = frame.subarray(0, 5);
    const chunk2 = frame.subarray(5, 20);
    const chunk3 = frame.subarray(20);

    decoder.push(chunk1);
    expect(decoder.frames()).toHaveLength(0);

    decoder.push(chunk2);
    expect(decoder.frames()).toHaveLength(0);

    decoder.push(chunk3);
    const frames = decoder.frames();
    expect(frames).toHaveLength(1);
    expect(frames[0].json).toEqual(json);
    expect(Buffer.compare(frames[0].payload!, payload)).toBe(0);
  });

  it("handles multi-frame buffers (multiple frames in one chunk)", () => {
    const decoder = new FrameDecoder();
    const json1 = { id: "1", type: "ping" };
    const json2 = { id: "2", type: "listSessions" };
    const payload2 = Buffer.from([0xde, 0xad, 0xbe, 0xef]);

    const frame1 = encodeFrame(json1);
    const frame2 = encodeFrame(json2, payload2);
    const combined = Buffer.concat([frame1, frame2]);

    decoder.push(combined);
    const frames = decoder.frames();

    expect(frames).toHaveLength(2);
    expect(frames[0].json).toEqual(json1);
    expect(frames[0].payload).toBeNull();
    expect(frames[1].json).toEqual(json2);
    expect(Buffer.compare(frames[1].payload!, payload2)).toBe(0);
  });

  it("rejects oversized frames (>8MB)", () => {
    const decoder = new FrameDecoder();

    // Craft a header that claims a frame larger than MAX_FRAME_SIZE
    const fakeHeader = Buffer.alloc(8);
    fakeHeader.writeUInt32BE(MAX_FRAME_SIZE + 1, 0); // totalLen > 8MB
    fakeHeader.writeUInt32BE(10, 4); // jsonLen (doesn't matter, will reject first)

    expect(() => decoder.push(fakeHeader)).toThrow(DaemonProtocolError);
    expect(() => decoder.push(fakeHeader)).toThrow(/exceeds maximum/);
  });

  it("handles zero-payload frames correctly", () => {
    const decoder = new FrameDecoder();
    const json = { id: "3", ok: true, payload: null };
    const frame = encodeFrame(json);

    decoder.push(frame);
    const frames = decoder.frames();

    expect(frames).toHaveLength(1);
    expect(frames[0].json).toEqual(json);
    expect(frames[0].payload).toBeNull();
  });

  it("round-trips encode/decode with various JSON shapes", () => {
    const decoder = new FrameDecoder();
    const messages = [
      { type: "hello", version: 1, token: "abc", clientId: "c1", role: "control" },
      { id: "req-1", ok: true, payload: { sessions: [] } },
      { id: "req-2", ok: false, error: "Session not found: xyz" },
      { type: "event", event: "exit", sessionId: "s1", payload: { code: 0 } },
    ];

    for (const msg of messages) {
      const frame = encodeFrame(msg);
      decoder.push(frame);
    }

    const frames = decoder.frames();
    expect(frames).toHaveLength(messages.length);
    for (let i = 0; i < messages.length; i++) {
      expect(frames[i].json).toEqual(messages[i]);
    }
  });

  it("handles byte-by-byte delivery", () => {
    const decoder = new FrameDecoder();
    const json = { test: true };
    const frame = encodeFrame(json);

    // Feed one byte at a time
    for (let i = 0; i < frame.length - 1; i++) {
      decoder.push(frame.subarray(i, i + 1));
      expect(decoder.frames()).toHaveLength(0);
    }

    // Last byte completes the frame
    decoder.push(frame.subarray(frame.length - 1));
    const frames = decoder.frames();
    expect(frames).toHaveLength(1);
    expect(frames[0].json).toEqual(json);
  });

  it("bufferedBytes reflects unconsumed data", () => {
    const decoder = new FrameDecoder();
    const json = { x: 1 };
    const frame = encodeFrame(json);

    // Push partial frame
    const partial = frame.subarray(0, 5);
    decoder.push(partial);
    expect(decoder.bufferedBytes).toBe(5);

    // Complete the frame
    decoder.push(frame.subarray(5));
    decoder.frames(); // consume
    expect(decoder.bufferedBytes).toBe(0);
  });

  it("throws DaemonProtocolError on invalid JSON", () => {
    const decoder = new FrameDecoder();

    // Craft a frame with invalid JSON content
    const invalidJson = Buffer.from("not valid json{{{");
    const jsonLen = invalidJson.length;
    const totalLen = 4 + jsonLen;
    const frame = Buffer.alloc(8 + jsonLen);
    frame.writeUInt32BE(totalLen, 0);
    frame.writeUInt32BE(jsonLen, 4);
    invalidJson.copy(frame, 8);

    expect(() => decoder.push(frame)).toThrow(DaemonProtocolError);
    expect(() => {
      const d = new FrameDecoder();
      d.push(frame);
    }).toThrow(/Invalid JSON/);
  });
});
