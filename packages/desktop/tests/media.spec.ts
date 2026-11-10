import { describe, expect, it, vi } from "vitest";
import { MediaResources } from "../electron/media";

const WINDOW = 1024 * 1024;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function fixture(size = 31) {
  const request = vi.fn(async (method: string, params: Record<string, unknown>): Promise<unknown> => {
    if (method === "file_open") return { file_id: "captured", size, kind: "crab" };
    if (method === "read_range") {
      const offset = params.offset as number;
      const length = params.length as number;
      const bytes = Buffer.from(Array.from({ length }, (_, index) => (offset + index) % 251));
      return { offset, length, file_size: size, bytes_b64: bytes.toString("base64") };
    }
    return { closed: true };
  });
  const notify = vi.fn();
  const media = new MediaResources(request, notify);
  const open = () => media.open(1, { repo_id: "repo", path: "movie.webm", native_session_id: "native:1" });
  return { request, notify, media, open };
}

describe("captured media", () => {
  it.each([
    ["bytes=5-8", "bytes 5-8/31", 5, 4],
    ["bytes=-3", "bytes 28-30/31", 28, 3],
    ["bytes=29-", "bytes 29-30/31", 29, 2],
    ["bytes=30-100", "bytes 30-30/31", 30, 1],
  ])("serves %s from the retained source", async (range, contentRange, offset, length) => {
    const { media, request, open } = fixture();
    const source = await open();
    try {
      const response = await media.fetch(new Request(source.url, { headers: { Range: range } }));
      expect(response.status).toBe(206);
      expect(response.headers.get("Content-Range")).toBe(contentRange);
      const bytes = Buffer.from(await response.arrayBuffer());
      expect(bytes).toEqual(Buffer.from(Array.from({ length }, (_, index) => offset + index)));
      expect(request).toHaveBeenCalledWith("read_range", expect.objectContaining({ repo_id: "repo", file_id: "captured", offset, length }));
      expect(request.mock.calls.filter(([method]) => method === "file_open")).toHaveLength(1);
    } finally { await media.closeAll(); }
  });

  it("answers metadata and invalid ranges without reading payload bytes", async () => {
    const { media, request, open } = fixture();
    const source = await open();
    try {
      const head = await media.fetch(new Request(source.url, { method: "HEAD", headers: { Range: "bytes=5-8" } }));
      expect([head.status, head.headers.get("Content-Length"), await head.text()]).toEqual([200, "31", ""]);
      for (const range of ["bytes=31-", "bytes=3-2", "bytes=-0", "bytes=1-2,4-5", "bytes=9007199254740992-"]) {
        const response = await media.fetch(new Request(source.url, { headers: { Range: range } }));
        expect([response.status, response.headers.get("Content-Range")]).toEqual([416, "bytes */31"]);
      }
      expect(request.mock.calls.filter(([method]) => method === "read_range")).toHaveLength(0);
    } finally { await media.closeAll(); }
  });

  it("ignores unknown range units and unprovable If-Range conditions", async () => {
    const { media, open } = fixture();
    const source = await open();
    try {
      for (const headers of [{ Range: "items=1-2" }, { Range: "bytes=1-2", "If-Range": '"different"' }]) {
        const response = await media.fetch(new Request(source.url, { headers }));
        expect([response.status, (await response.arrayBuffer()).byteLength]).toEqual([200, 31]);
      }
    } finally { await media.closeAll(); }
  });

  it("streams with backpressure and bounded reads instead of loading the full object", async () => {
    const { media, request, open } = fixture(3 * 1024 ** 3);
    const source = await open();
    try {
      const response = await media.fetch(new Request(source.url));
      await new Promise(resolve => setTimeout(resolve, 0));
      expect(request.mock.calls.filter(([method]) => method === "read_range")).toHaveLength(1);
      const reader = response.body!.getReader();
      expect((await reader.read()).value?.byteLength).toBe(WINDOW);
      await reader.cancel();
      const reads = request.mock.calls.filter(([method]) => method === "read_range");
      expect(reads.length).toBeLessThanOrEqual(2);
      expect(reads.every(([, params]) => params.length === WINDOW)).toBe(true);
    } finally { await media.closeAll(); }
  });

  it("keeps a seek failure structured and visible to the owning window", async () => {
    const { media, request, notify, open } = fixture(2 * WINDOW);
    const source = await open();
    const native = request.getMockImplementation()!;
    const failure = { code: 1012, message: "Content hash differs", data: { kind: "content_read" } };
    request.mockImplementation(async (method, params) => {
      if (method === "read_range" && params.offset === WINDOW) throw failure;
      return native(method, params);
    });
    try {
      const response = await media.fetch(new Request(source.url));
      const reader = response.body!.getReader();
      await reader.read();
      await expect(reader.read()).rejects.toMatchObject(failure);
      expect(notify).toHaveBeenCalledWith(1, source.media_id, failure);
    } finally { await media.closeAll(); }
  });

  it("acknowledges a stopped response after its native query drains and permits another seek", async () => {
    const { media, request, open } = fixture(2 * WINDOW);
    const source = await open();
    const native = request.getMockImplementation()!;
    const pending = deferred<unknown>();
    request.mockImplementation((method, params) => method === "read_range" && params.offset === WINDOW ? pending.promise : native(method, params));
    try {
      const response = await media.fetch(new Request(source.url));
      const reader = response.body!.getReader();
      await reader.read();
      await vi.waitFor(() => expect(request).toHaveBeenCalledWith("read_range", expect.objectContaining({ offset: WINDOW })));
      let stopped = false;
      const stopping = reader.cancel().then(() => { stopped = true; });
      await Promise.resolve();
      expect(stopped).toBe(false);
      pending.reject({ code: 1010, message: "Cancelled" });
      await stopping;
      const next = await media.fetch(new Request(source.url, { headers: { Range: "bytes=0-3" } }));
      expect((await next.arrayBuffer()).byteLength).toBe(4);
    } finally { await media.closeAll(); }
  });

  it("cancels and drains an in-flight range before acknowledging duplicate closes", async () => {
    const { media, request, open } = fixture();
    const source = await open();
    const pending = deferred<unknown>();
    const native = request.getMockImplementation()!;
    request.mockImplementation((method, params) => method === "read_range" ? pending.promise : native(method, params));
    const fetching = media.fetch(new Request(source.url));
    let closed = false;
    const closing = media.close(1, source.media_id).then(() => { closed = true; });
    const duplicate = media.close(1, source.media_id);
    await vi.waitFor(() => expect(request).toHaveBeenCalledWith("rpc/cancel", expect.objectContaining({ repo_id: "repo", op_id: expect.any(String) })));
    expect(closed).toBe(false);
    expect(request.mock.calls.some(([method]) => method === "file_close")).toBe(false);
    pending.reject({ code: 1010, message: "Cancelled" });
    await Promise.all([closing, duplicate, fetching]);
    expect(request.mock.calls.filter(([method]) => method === "file_close")).toHaveLength(1);
    expect((await media.fetch(new Request(source.url))).status).toBe(410);
  });

  it("closes a late file capture before repository cleanup returns", async () => {
    const { media, request, open } = fixture();
    const captured = deferred<unknown>();
    const native = request.getMockImplementation()!;
    request.mockImplementation((method, params) => method === "file_open" ? captured.promise : native(method, params));
    const opening = open();
    const failure = expect(opening).rejects.toThrow("Media preview closed");
    const closing = media.closeRepo("repo");
    await vi.waitFor(() => expect(request).toHaveBeenCalledWith("rpc/cancel", expect.objectContaining({ repo_id: "repo" })));
    captured.resolve({ file_id: "late", size: 31, kind: "crab" });
    await Promise.all([closing, failure]);
    expect(request).toHaveBeenCalledWith("file_close", { repo_id: "repo", file_id: "late", native_session_id: "native:1" });
  });

  it("drains all captures even when cancellation or native cleanup fails", async () => {
    const { media, request, open } = fixture();
    const captured = deferred<unknown>();
    request.mockImplementation(async (method) => {
      if (method === "file_open") return captured.promise;
      if (method === "file_close") throw new Error("Reader cleanup failed");
      throw new Error("Cancel control failed");
    });
    const opening = open();
    const failedOpening = expect(opening).rejects.toThrow("Reader cleanup failed");
    const closing = media.closeOwner(1);
    const failedClose = expect(closing).rejects.toThrow("Reader cleanup failed");
    captured.resolve({ file_id: "late", size: 31, kind: "crab" });
    await Promise.all([failedClose, failedOpening]);
    expect(request).toHaveBeenCalledWith("file_close", { repo_id: "repo", file_id: "late", native_session_id: "native:1" });
  });

  it("isolates owners and repositories and refuses new work after shutdown", async () => {
    const { media, open } = fixture();
    const first = await open();
    const other = await media.open(2, { repo_id: "other", path: "audio.wav", native_session_id: "native:2" });
    await expect(media.close(2, first.media_id)).rejects.toThrow("another window");
    await media.closeRepo("repo");
    expect((await media.fetch(new Request(first.url))).status).toBe(410);
    expect((await media.fetch(new Request(other.url, { method: "HEAD" }))).status).toBe(200);
    await media.closeAll();
    expect((await media.fetch(new Request(other.url))).status).toBe(410);
    await expect(open()).rejects.toThrow("closed");
  });

  it("revokes a lost session without sending its handles to a replacement", async () => {
    const { media, request, notify, open } = fixture();
    const source = await open();
    const sibling = await media.open(2, { repo_id: "other", path: "audio.wav", native_session_id: "native:2" });
    request.mockClear();
    await media.invalidateRepo("repo");
    await media.close(1, source.media_id);
    expect((await media.fetch(new Request(source.url))).status).toBe(410);
    expect((await media.fetch(new Request(sibling.url, { method: "HEAD" }))).status).toBe(200);
    expect(request).not.toHaveBeenCalled();
    expect(notify).toHaveBeenCalledWith(1, source.media_id, expect.objectContaining({ code: "NATIVE_SESSION_LOST" }));
    await media.closeAll();
  });

  it("discards a late capture after session loss and drains it without stale cleanup", async () => {
    const { media, request, open } = fixture();
    const captured = deferred<unknown>();
    request.mockImplementation(async () => captured.promise);
    const opening = open();
    const failure = expect(opening).rejects.toThrow("Media preview closed");
    let drained = false;
    const invalidating = media.invalidateRepo("repo").then(() => { drained = true; });
    await Promise.resolve();
    expect(drained).toBe(false);
    captured.resolve({ file_id: "old", size: 31, kind: "crab" });
    await Promise.all([invalidating, failure]);
    expect(request.mock.calls.map(([method]) => method)).toEqual(["file_open"]);
  });

  it("binds reads and cleanup to the capture's native session", async () => {
    const { media, request, open } = fixture();
    const source = await open();
    const response = await media.fetch(new Request(source.url, { headers: { Range: "bytes=0-3" } }));
    await response.arrayBuffer();
    await media.closeAll();
    expect(request.mock.calls.every(([, params]) => params.native_session_id === "native:1")).toBe(true);
  });
});
