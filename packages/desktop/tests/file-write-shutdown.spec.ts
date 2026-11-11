import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";

let root: string;
beforeEach(async () => {
  vi.resetModules();
  root = await fs.promises.mkdtemp(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "file-write-shutdown-"));
});
afterEach(async () => { vi.restoreAllMocks(); await fs.promises.rm(root, { recursive: true, force: true }); });

for (const boundary of ["preparation", "publication", "cleanup"] as const) it(`shutdown joins admitted ${boundary} and rejects late saves`, async () => {
  const { writeTextAtomically, stopFileWrites } = await import("../electron/file-write");
  const file = path.join(root, "note.txt"), late = path.join(root, "late.txt");
  await fs.promises.writeFile(file, "ORIGINAL");
  let entered!: () => void, release!: () => void;
  const held = new Promise<void>(resolve => { entered = resolve; }), gate = new Promise<void>(resolve => { release = resolve; });
  const realpath = fs.promises.realpath.bind(fs.promises);
  const rename = fs.promises.rename.bind(fs.promises), unlink = fs.promises.unlink.bind(fs.promises);
  const failure = new Error("Fixture publication failed");
  if (boundary === "preparation") vi.spyOn(fs.promises, "realpath").mockImplementation(async (...args) => {
    entered(); await gate; return realpath(...args);
  });
  vi.spyOn(fs.promises, "rename").mockImplementation(async (...args) => {
    if (boundary === "cleanup") throw failure;
    if (boundary === "publication") { entered(); await gate; }
    return rename(...args);
  });
  vi.spyOn(fs.promises, "unlink").mockImplementation(async (...args) => {
    if (boundary === "cleanup") { entered(); await gate; }
    return unlink(...args);
  });
  const saving = writeTextAtomically(file, "ACKNOWLEDGED_UTF8_🦀");
  const result = Promise.allSettled([saving]);
  let drained = false, stopping: Promise<void> | undefined;
  try {
    await held;
    stopping = stopFileWrites().then(() => { drained = true; });
    await expect(writeTextAtomically(late, "TOO_LATE")).rejects.toMatchObject({ code: "ESHUTDOWN" });
    expect(drained).toBe(false);
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
  } finally { release(); await stopping; }
  expect((await result)[0]).toEqual(boundary === "cleanup"
    ? { status: "rejected", reason: failure }
    : { status: "fulfilled", value: Buffer.byteLength("ACKNOWLEDGED_UTF8_🦀") });
  expect(await fs.promises.readFile(file, "utf8")).toBe(boundary === "cleanup" ? "ORIGINAL" : "ACKNOWLEDGED_UTF8_🦀");
  expect(await fs.promises.readdir(root)).toEqual(["note.txt"]);
});
