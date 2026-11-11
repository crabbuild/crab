import Database from "better-sqlite3";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FileTransfers, type FileTransferActivity } from "../electron/file-transfers";
import { OP_CANCELLED_RPC_CODE } from "../electron/rpc-transport";

const databases: Database.Database[] = [];
const repository = { repository: "/work/repository", storageConnectionId: "saved-alpha", nativeSessionId: "native:captured" };
const params = { repo_id: "logical", op_id: "attempt", native_session_id: "native:captured", paths: ["model[1].bin", "model1.bin"] };
function setup(db = new Database(":memory:")) {
  if (!databases.includes(db)) databases.push(db);
  const close = vi.fn().mockResolvedValue({}), cancel = vi.fn().mockResolvedValue({}), changed = vi.fn();
  const jobs = new FileTransfers(db, close, cancel, changed, message => message.replaceAll("FIXTURE_SECRET", "[redacted]"));
  return { db, jobs, close, cancel, changed };
}
function deferred() {
  let resolve!: (value: unknown) => void, reject!: (error: Error) => void;
  return { promise: new Promise<unknown>((yes, no) => { resolve = yes; reject = no; }), resolve: (value: unknown) => resolve(value), reject: (error: Error) => reject(error) };
}
function progress(jobs: FileTransfers, published = 1) {
  jobs.observe("progress/transfer", { job_id: "attempt", repo_id: "logical", written_bytes: 4096, total_bytes: 8192, files_published: published, files_total: 2 });
}
afterEach(() => { for (const db of databases.splice(0)) if (db.open) db.close(); });

describe("durable native file-transfer Activity", () => {
  it("commits literal intent before dispatch and never persists native IDs, captures or credentials", async () => {
    const { db, jobs } = setup();
    await jobs.run("files_materialize", { ...params, paths: [...params.paths, params.paths[0]], __storage_auth: "FIXTURE_SECRET" }, repository, async () => {
      const row = db.prepare("SELECT intent,state FROM file_transfer_jobs").get() as { intent: string; state: string };
      expect(JSON.parse(row.intent)).toEqual({ repository: "/work/repository", storageConnectionId: "saved-alpha", verb: "hydrate", paths: params.paths });
      expect(row.state).not.toContain("native:captured");
      expect(JSON.stringify(row)).not.toContain("FIXTURE_SECRET");
      return { written_bytes: 8192, files: 2 };
    });
    expect(jobs.snapshot().jobs[0]).toMatchObject({ status: "done", transfer: { written_bytes: 8192, files_published: 2 } });
  });

  it("retains actual partial publication and redacted errors across restart", async () => {
    const { db, jobs } = setup();
    await expect(jobs.run("files_materialize", params, repository, async () => {
      progress(jobs);
      throw Object.assign(new Error("Unavailable content FIXTURE_SECRET"), { code: 1011 });
    })).rejects.toThrow("Unavailable content");
    const restored = setup(db).jobs.snapshot().jobs[0];
    expect(restored).toMatchObject({ status: "failed", transfer: { files_published: 1 }, interruption: "Unavailable content [redacted]", intent: { paths: params.paths } });
    expect(restored.nativeSessionId).toBeUndefined();
  });

  it("recovers an unacknowledged admission as unknown without replaying it", async () => {
    const { db, jobs } = setup();
    const work = deferred();
    const pending = jobs.run("files_restore", params, repository, () => work.promise);
    const receipt = db.prepare("SELECT * FROM file_transfer_jobs").get() as Record<string, unknown>;
    const crash = setup();
    crash.db.prepare("INSERT INTO file_transfer_jobs(job_id,intent,state,status,updated_at) VALUES(@job_id,@intent,@state,@status,@updated_at)").run(receipt);
    const recovered = setup(crash.db).jobs;
    expect(recovered.snapshot().jobs[0]).toMatchObject({ status: "interrupted", intent: { verb: "restore", paths: params.paths }, nativeSessionId: undefined });
    recovered.observe("progress/complete", { job_id: "attempt", status: 0 });
    expect(recovered.snapshot().jobs[0].status).toBe("interrupted");
    work.resolve({ written_bytes: 8192 }); await pending;
  });

  it("requires a native cancellation receipt and preserves already-published files", async () => {
    const { jobs } = setup();
    await expect(jobs.run("files_materialize", params, repository, async () => {
      progress(jobs);
      throw Object.assign(new Error("Cancelled"), { code: OP_CANCELLED_RPC_CODE });
    })).rejects.toThrow("Cancelled");
    expect(jobs.snapshot().jobs[0]).toMatchObject({ status: "cancelled", transfer: { files_published: 1 } });
  });

  it("keeps a delivered native completion when its RPC reply is lost", async () => {
    const { jobs } = setup();
    await expect(jobs.run("files_materialize", params, repository, async () => {
      progress(jobs, 2);
      jobs.observe("progress/complete", { job_id: "attempt", status: 0 });
      throw new Error("Connection ended before the reply");
    })).rejects.toThrow("Connection ended");
    expect(jobs.snapshot().jobs[0]).toMatchObject({ status: "done", transfer: { files_published: 2 } });
  });

  it("isolates capture IDs across repository sessions and retires them on rebind", async () => {
    const { jobs } = setup();
    jobs.capture("logical", "native:captured", "model[1].bin", { file_id: "capture", can_materialize: true });
    jobs.capture("sibling", "native:other", "other.bin", { file_id: "capture", can_materialize: true });
    await jobs.run("file_materialize", { ...params, file_id: "capture" }, repository, async () => ({ written_bytes: 10 }));
    expect(jobs.snapshot().jobs[0].intent.paths).toEqual(["model[1].bin"]);
    jobs.forget("logical");
    const run = vi.fn();
    await expect(jobs.run("file_materialize", { ...params, op_id: "later", file_id: "capture" }, repository, run)).rejects.toThrow("Reopen file details");
    expect(run).not.toHaveBeenCalled();
  });

  it("refuses dispatch on failed durable admission and can recover writable storage", async () => {
    const { db, jobs } = setup();
    db.exec("CREATE TRIGGER deny_admission BEFORE INSERT ON file_transfer_jobs BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END");
    const work = vi.fn().mockResolvedValue({ written_bytes: 8192 });
    await expect(jobs.run("files_materialize", params, repository, work)).rejects.toThrow("Activity could not save");
    expect(work).not.toHaveBeenCalled();
    db.exec("DROP TRIGGER deny_admission"); jobs.recover();
    await jobs.run("files_materialize", params, repository, work);
    expect(jobs.snapshot().jobs[0].status).toBe("done");
  });

  it("cancels admitted work when receipts fail and retains the known publication count", async () => {
    const { db, jobs, cancel } = setup();
    await expect(jobs.run("files_materialize", params, repository, async () => {
      db.exec("CREATE TRIGGER deny_receipt BEFORE UPDATE ON file_transfer_jobs BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END");
      progress(jobs);
      throw Object.assign(new Error("Cancelled"), { code: OP_CANCELLED_RPC_CODE });
    })).rejects.toThrow("Cancelled");
    expect(cancel).toHaveBeenCalledExactlyOnceWith("logical", "attempt", "native:captured");
    expect(jobs.snapshot()).toMatchObject({ error: expect.stringContaining("Activity could not save"), jobs: [expect.objectContaining({ transfer: expect.objectContaining({ files_published: 1 }) })] });
    db.exec("DROP TRIGGER deny_receipt"); jobs.recover();
    expect(setup(db).jobs.snapshot().jobs[0]).toMatchObject({ status: "cancelled", transfer: { files_published: 1 } });
  });

  it("joins admitted work before repository close, including a reloaded subscriber", async () => {
    const { jobs, close } = setup(); const work = deferred();
    const pending = jobs.run("files_materialize", params, repository, () => work.promise);
    const reload: FileTransferActivity = jobs.snapshot();
    expect(reload.jobs[0]).toMatchObject({ status: "running", nativeSessionId: "native:captured" });
    const closed = jobs.close("logical");
    expect(close).not.toHaveBeenCalled();
    work.resolve({ written_bytes: 8192 }); await pending; await closed;
    expect(close).toHaveBeenCalledExactlyOnceWith("logical");
  });
  it("cancels a blocked preparation promptly and fences its eventual native write", async () => {
    const { jobs } = setup(); const gate = deferred(); const send = vi.fn();
    const pending = jobs.run("files_materialize", params, repository, async () => {
      await gate.promise;
      jobs.fence("files_materialize", params);
      send(); return { written_bytes: 8192 };
    });
    const cancelled = expect(pending).rejects.toMatchObject({ code: OP_CANCELLED_RPC_CODE });
    await jobs.requestCancel("logical", "attempt", "native:captured");
    await cancelled;
    expect(jobs.snapshot().jobs[0].status).toBe("cancelled");
    gate.resolve(undefined);
    await vi.waitFor(() => expect(jobs.owns("attempt")).toBe(false));
    expect(send).not.toHaveBeenCalled();
  });

  it("waits for native acknowledgement when cancellation races its first progress frame", async () => {
    const { jobs, cancel } = setup(); const reply = deferred();
    const pending = jobs.run("files_materialize", params, repository, async () => {
      jobs.fence("files_materialize", params);
      return reply.promise;
    });
    const outcome = expect(pending).rejects.toMatchObject({ code: OP_CANCELLED_RPC_CODE });
    await vi.waitFor(() => expect(jobs.owns("attempt")).toBe(true));
    await Promise.resolve();
    await jobs.requestCancel("logical", "attempt", "native:captured");
    expect(jobs.snapshot().jobs[0].status).toBe("running");
    expect(cancel).not.toHaveBeenCalled();
    progress(jobs);
    expect(cancel).toHaveBeenCalledExactlyOnceWith("logical", "attempt", "native:captured");
    reply.reject(Object.assign(new Error("Cancelled"), { code: OP_CANCELLED_RPC_CODE }));
    await outcome;
    expect(jobs.snapshot().jobs[0]).toMatchObject({ status: "cancelled", transfer: { files_published: 1 } });
  });

  it("does not retry failed cancellation in a receipt-failure loop", async () => {
    const { db, jobs, cancel } = setup(); const reply = deferred();
    cancel.mockRejectedValue(new Error("Connection unavailable"));
    const pending = jobs.run("files_materialize", params, repository, () => reply.promise);
    const outcome = expect(pending).rejects.toThrow("Connection unavailable");
    db.exec("CREATE TRIGGER deny_receipt BEFORE UPDATE ON file_transfer_jobs BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END");
    progress(jobs); await Promise.resolve();
    progress(jobs); await Promise.resolve();
    expect(cancel).toHaveBeenCalledTimes(1);
    reply.reject(new Error("Connection unavailable")); await outcome;
  });

  it("allows another cancel after a queued control request fails asynchronously", async () => {
    const { jobs, cancel } = setup(); const reply = deferred();
    cancel.mockRejectedValueOnce(new Error("Control channel unavailable"));
    const pending = jobs.run("files_materialize", params, repository, async () => {
      jobs.fence("files_materialize", params); return reply.promise;
    });
    const outcome = expect(pending).rejects.toMatchObject({ code: OP_CANCELLED_RPC_CODE });
    await Promise.resolve();
    await jobs.requestCancel("logical", "attempt", "native:captured");
    progress(jobs);
    await vi.waitFor(() => expect(jobs.snapshot().jobs[0]).toMatchObject({ status: "running", cancelRequested: false, interruption: "Control channel unavailable" }));
    await jobs.requestCancel("logical", "attempt", "native:captured");
    expect(cancel).toHaveBeenCalledTimes(2);
    reply.reject(Object.assign(new Error("Cancelled"), { code: OP_CANCELLED_RPC_CODE })); await outcome;
  });

  it("bounds finished history while retaining every active admission", async () => {
    const { jobs } = setup(); const reply = deferred();
    const pending = jobs.run("files_materialize", { ...params, op_id: "still-running" }, repository, () => reply.promise);
    for (let index = 0; index < 205; index++) await jobs.run("files_materialize", { ...params, op_id: `finished-${index}` }, repository, async () => ({ written_bytes: 8192 }));
    expect(jobs.snapshot().jobs).toHaveLength(201);
    expect(jobs.snapshot().jobs.find(job => job.job_id === "still-running")?.status).toBe("running");
    reply.resolve({ written_bytes: 8192 }); await pending;
    expect(jobs.snapshot().jobs).toHaveLength(200);
  });

  it("rejects corrupt history without deleting it or silently resetting outcomes", async () => {
    const { db, jobs } = setup();
    await jobs.run("files_materialize", params, repository, async () => ({ written_bytes: 8192 }));
    db.prepare("UPDATE file_transfer_jobs SET state=?").run('{"status":"running"}');
    expect(() => setup(db)).toThrow("invalid file-transfer record");
    expect(db.prepare("SELECT COUNT(*) AS count FROM file_transfer_jobs").get()).toEqual({ count: 1 });
  });

  it("fences prepared work if a sibling's durable receipt becomes unwritable", async () => {
    const { db, jobs } = setup(); const gate = deferred(); const send = vi.fn();
    const pending = jobs.run("files_materialize", params, repository, async () => {
      await gate.promise; jobs.fence("files_materialize", params); send();
    });
    const outcome = expect(pending).rejects.toThrow("Activity could not save");
    db.exec("CREATE TRIGGER deny_admission BEFORE INSERT ON file_transfer_jobs BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END");
    await expect(jobs.run("files_materialize", { ...params, op_id: "sibling" }, repository, vi.fn())).rejects.toThrow("Activity could not save");
    gate.resolve(undefined); await outcome;
    expect(send).not.toHaveBeenCalled();
  });

});
