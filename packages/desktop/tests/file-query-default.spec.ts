import { beforeEach, expect, it, vi } from "vitest";
import type { FileQueryResult } from "../src/ipc";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));
vi.mock("../src/ipc", () => ({ rpc: { fileQuery: request }, isCancelledError: () => false }));
import { FileQuery } from "../src/lib/file-query";

beforeEach(() => {
  request.mockReset();
  request.mockReturnValue({
    promise: Promise.resolve({ kind: "file_query", columns: [], rows: [], row_count: 0, truncated: false, max_rows: 1000 } satisfies FileQueryResult),
    cancel: vi.fn(),
  });
});

it("waits for a resolved default before saving or running a query", async () => {
  const query = new FileQuery("repository", "data.sqlite", null);
  query.startDefault("native", "");
  expect({ draft: query.snapshot().sql, requests: request.mock.calls.length }).toEqual({ draft: null, requests: 0 });
  query.startDefault("native", 'SELECT * FROM "data" LIMIT 100;');
  await vi.waitFor(() => expect(query.snapshot().status).toBe("idle"));
  expect(request).toHaveBeenCalledWith("repository", "data.sqlite", 'SELECT * FROM "data" LIMIT 100;', { rev: undefined, native_session_id: "native", max_rows: 1000 });
});

it("preserves an explicit draft when table metadata arrives later", () => {
  const query = new FileQuery("repository", "data.sqlite", null);
  query.startDefault("native", "");
  query.setSql("SELECT 7 AS chosen;");
  query.startDefault("native", 'SELECT * FROM "data" LIMIT 100;');
  expect({ draft: query.snapshot().sql, requests: request.mock.calls.length }).toEqual({ draft: "SELECT 7 AS chosen;", requests: 0 });
});
