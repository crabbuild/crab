import type Database from "better-sqlite3";
import { OP_CANCELLED_RPC_CODE } from "./rpc-transport";
import { FileTransferOwner } from "./file-transfer-owner";
import { nativeSessionLost } from "./repo-capabilities";

type Verb = "hydrate" | "dehydrate" | "restore";
type Params = Record<string, unknown>;

export interface ProgressTransfer {
  op: string;
  job_id: string;
  repo_id: string;
  written_bytes: number;
  received_bytes?: number;
  checked_bytes?: number | null;
  total_bytes: number | null;
  files_published?: number | null;
  files_total?: number | null;
}

export interface FileTransferIntent {
  repository: string;
  storageConnectionId: string | null;
  verb: Verb;
  paths: string[];
}

interface FileTransferVersion {
  kind: "git" | "crab" | "lfs";
  hash: string;
  size: number;
  mode: string | null;
}

export interface FileTransferSource extends FileTransferVersion {
  position: number;
  path: string;
}

export interface FileTransferJob {
  job_id: string;
  repoId: string;
  op: Verb;
  intent: FileTransferIntent;
  revision: number;
  nativeSessionId?: string;
  cancelRequested?: boolean;
  status: "running" | "done" | "failed" | "cancelled" | "interrupted";
  transfer?: ProgressTransfer;
  interruption?: string;
  exitCode?: number;
  startedAt: number;
  updatedAt: number;
}

export interface FileTransferActivity {
  version: number;
  jobs: FileTransferJob[];
  error: string | null;
}

export interface FileTransferChange {
  version: number;
  job?: FileTransferJob;
  removed?: string[];
  error: string | null;
}

export interface FileTransferRepository {
  repository: string;
  storageConnectionId: string | null;
  nativeSessionId: string | null;
}

const METHODS: Record<string, Verb> = {
  file_materialize: "hydrate", files_materialize: "hydrate", files_dehydrate: "dehydrate", files_restore: "restore",
};
const HISTORY_LIMIT = 200;

export function isFileTransfer(method: string): boolean { return Object.hasOwn(METHODS, method); }

function fields(value: unknown): Params | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Params : null;
}

function number(value: unknown): value is number { return Number.isSafeInteger(value) && Number(value) >= 0; }

function sourceVersion(value: unknown): FileTransferVersion | null {
  const source = fields(value);
  if (!source || !number(source.size) || typeof source.hash !== "string") return null;
  const kind = source.kind;
  if (kind === "git") {
    if (!/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(source.hash) || typeof source.mode !== "string" || !["100644", "100755"].includes(source.mode)) return null;
  } else if ((kind !== "crab" && kind !== "lfs") || !/^[0-9a-f]{64}$/.test(source.hash) || source.mode !== null) return null;
  return { kind, hash: source.hash, size: source.size, mode: kind === "git" ? String(source.mode) : null };
}

class SourceChanged extends Error {
  constructor() { super("A transfer cannot replace an already captured source. Start a new attempt."); }
}

function sameSource(left: FileTransferSource, right: FileTransferSource): boolean {
  return left.path === right.path && left.kind === right.kind && left.hash === right.hash && left.size === right.size && left.mode === right.mode;
}

function cancelledBeforeAdmission(): Error {
  return Object.assign(new Error("File transfer cancelled before native admission"), { code: OP_CANCELLED_RPC_CODE, data: { kind: "op_cancelled" } });
}

/** Own native transfer admission, receipts and repository drain in the main process. */
export class FileTransfers {
  private jobs = new Map<string, FileTransferJob>();
  private captures = new Map<string, { repoId: string; session: string; path: string; fileId: string }>();
  private pendingSources = new Map<string, Map<number, FileTransferSource>>();
  private active = new Map<string, { repoId: string; session: string; admitted: boolean; sent: boolean; cancelAttempted: boolean; cancelRequested: boolean; cancelBeforeSend: () => void; cancellation?: Promise<unknown> }>();
  private owner: FileTransferOwner;
  private version = 0;
  private failure: string | null = null;
  private stopping = false;

  constructor(
    private db: Database.Database,
    closeRepo: (repoId: string) => Promise<unknown>,
    private cancel: (repoId: string, jobId: string, session: string) => Promise<unknown>,
    private changed: (change: FileTransferChange) => void,
    private redact: (message: string) => string,
  ) {
    this.owner = new FileTransferOwner(closeRepo);
    db.exec(`CREATE TABLE IF NOT EXISTS file_transfer_jobs (
      job_id TEXT PRIMARY KEY, intent TEXT NOT NULL, state TEXT NOT NULL,
      status TEXT NOT NULL, updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS file_transfer_sources (
      job_id TEXT NOT NULL REFERENCES file_transfer_jobs(job_id) ON DELETE CASCADE,
      position INTEGER NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL,
      hash TEXT NOT NULL, size INTEGER NOT NULL, mode TEXT,
      PRIMARY KEY(job_id,position)
    )`);
    const rows = db.prepare("SELECT job_id, intent, state FROM file_transfer_jobs ORDER BY updated_at DESC").all() as Array<{ job_id: string; intent: string; state: string }>;
    db.transaction(() => {
      for (const row of rows) {
        const intent = JSON.parse(row.intent) as FileTransferIntent;
        const state = JSON.parse(row.state) as Omit<FileTransferJob, "intent" | "nativeSessionId">;
        if (typeof intent.repository !== "string" || !intent.repository ||
            !(intent.storageConnectionId === null || typeof intent.storageConnectionId === "string") || state.op !== intent.verb || !Array.isArray(intent.paths) || !intent.paths.every(path => typeof path === "string") ||
            !["hydrate", "dehydrate", "restore"].includes(intent.verb) || state.job_id !== row.job_id || typeof state.repoId !== "string" ||
            !["running", "done", "failed", "cancelled", "interrupted"].includes(state.status) || !number(state.revision)) {
          throw new Error("Activity contains an invalid file-transfer record.");
        }
        let job: FileTransferJob = { ...state, intent, nativeSessionId: undefined };
        if (job.status === "running") {
          // A lost process proves neither rollback nor completed cleanup. Never
          // replay its native identity or turn its last counters into success.
          job = { ...job, status: "interrupted", interruption: "The application stopped before a terminal receipt. Check working files before retrying.", revision: job.revision + 1, updatedAt: Date.now() };
          this.persist(job);
        }
        this.jobs.set(job.job_id, job);
      }
      this.prune();
    })();
  }

  snapshot(): FileTransferActivity {
    return { version: this.version, jobs: [...this.jobs.values()].map(job => structuredClone(job)), error: this.failure };
  }

  sources(jobId: string, start: number, limit: number): FileTransferSource[] {
    const job = this.jobs.get(jobId);
    if (!job) throw new Error("This transfer is no longer in Activity history.");
    if (!number(start) || !number(limit) || !number(start + limit) || limit < 1 || limit > 50) throw new Error("Source pages require a nonnegative start and 1–50 entries.");
    const rows = this.db.prepare("SELECT position,path,kind,hash,size,mode FROM file_transfer_sources WHERE job_id=? AND position>=? AND position<? ORDER BY position").all(jobId, start, start + limit) as FileTransferSource[];
    const sources = new Map(rows.map(row => {
      if (!number(row.position) || job.intent.paths[row.position] !== row.path || !sourceVersion(row)) throw new Error("Activity contains an invalid captured source.");
      return [row.position, row];
    }));
    for (const [position, source] of this.pendingSources.get(jobId) ?? []) if (position >= start && position < start + limit) sources.set(position, source);
    return [...sources.values()].sort((a, b) => a.position - b.position);
  }

  capture(repoId: string, session: string, path: string, result: unknown): void {
    const file = fields(result);
    if (file?.can_materialize && typeof file.file_id === "string") this.captures.set(JSON.stringify([repoId, session, file.file_id]), { repoId, session, path, fileId: file.file_id });
  }

  forget(repoId: string, fileId?: string): void {
    for (const [id, source] of this.captures) if (source.repoId === repoId && (!fileId || source.fileId === fileId)) this.captures.delete(id);
  }

  run(method: string, params: Params, repository: FileTransferRepository, work: () => Promise<unknown>): Promise<unknown> {
    return this.owner.run(String(params.repo_id), async () => {
      if (this.failure) throw new Error(this.failure);
      const op = METHODS[method];
      const jobId = params.op_id;
      const repoId = String(params.repo_id);
      const session = params.native_session_id ?? repository.nativeSessionId;
      if (!op || typeof jobId !== "string" || !jobId || typeof session !== "string" || !session || !repository.repository) throw new Error("A file transfer requires an opened repository and operation identity.");
      // Validate ownership before capture lookup or durable intent. A replacement
      // has already retired old captures; that must remain a session-loss error.
      if (session !== repository.nativeSessionId) throw nativeSessionLost();
      if (this.jobs.has(jobId)) throw new Error("This transfer already has an Activity record. Start a new attempt.");
      let paths: string[];
      if (method === "file_materialize") {
        const captured = this.captures.get(JSON.stringify([repoId, session, params.file_id]));
        if (!captured || captured.repoId !== repoId || captured.session !== session) throw new Error("Reopen file details before materializing this file.");
        paths = [captured.path];
      } else {
        if (!Array.isArray(params.paths) || !params.paths.length || !params.paths.every(path => typeof path === "string" && path)) throw new Error("Select at least one literal file path.");
        paths = [...new Set(params.paths as string[])];
      }
      const now = Date.now();
      const job: FileTransferJob = {
        job_id: jobId, repoId, op, revision: 0, nativeSessionId: session, status: "running", startedAt: now, updatedAt: now,
        intent: { repository: repository.repository, storageConnectionId: repository.storageConnectionId, verb: op, paths },
      };
      // Commit intent before dispatching authentication, setup or native work.
      // If admission cannot be recorded, no publication is allowed to start.
      try { this.insert(job); }
      catch (error) { this.failedPersistence(error); throw new Error(this.failure!, { cause: error }); }
      this.jobs.set(jobId, job);
      let cancelBeforeSend!: () => void;
      const cancelled = new Promise<never>((_resolve, reject) => { cancelBeforeSend = () => reject(cancelledBeforeAdmission()); });
      const active = { repoId, session, admitted: false, sent: false, cancelAttempted: false, cancelRequested: false, cancelBeforeSend, cancellation: undefined as Promise<unknown> | undefined };
      this.active.set(jobId, active);
      this.publish(job);
      let prepared = false, returned = false;
      const cleanup = () => {
        if (!prepared || !returned) return;
        this.active.delete(jobId);
        if (!this.stopping) try { this.prune(); } catch (error) { this.failedPersistence(error); }
      };
      const workPending = Promise.resolve().then(() => {
        if (active.cancelRequested && !active.sent) throw cancelledBeforeAdmission();
        return work();
      });
      // A cancelled credential/setup wait owns no native writer. Retain its
      // wire guard until preparation settles, so it cannot publish after the
      // visible operation or repository has closed.
      void workPending.then(() => { prepared = true; cleanup(); }, () => { prepared = true; cleanup(); });
      try {
        const result = await Promise.race([workPending, cancelled]);
        const resultFields = fields(result);
        const progress = this.jobs.get(jobId)!.transfer;
        const transfer = resultFields && number(resultFields.written_bytes) ? {
          ...progress, op, job_id: jobId, repo_id: repoId, total_bytes: progress?.total_bytes ?? null,
          written_bytes: resultFields.written_bytes,
          files_published: number(resultFields.files) ? resultFields.files : paths.length,
          files_total: paths.length,
        } : progress;
        this.update(jobId, { status: "done", exitCode: 0, transfer });
        if (this.failure) throw new Error(this.failure);
        return result;
      } catch (error) {
        const detail = fields(error);
        const cancelled = detail?.code === OP_CANCELLED_RPC_CODE || fields(detail?.data)?.kind === "op_cancelled";
        const current = this.jobs.get(jobId)!;
        // A delivered native terminal receipt wins loss of the RPC reply.
        const status = current.status !== "running" ? current.status : cancelled ? "cancelled" : typeof detail?.code === "number" ? "failed" : "interrupted";
        this.update(jobId, { status, interruption: this.redact(error instanceof Error ? error.message : String(error)) });
        throw error;
      } finally {
        await active.cancellation?.catch(() => {});
        returned = true; cleanup();
      }
    });
  }

  observe(method: string, value: unknown): void {
    const params = fields(value);
    const jobId = params?.job_id;
    if (!params || typeof jobId !== "string" || !this.active.has(jobId)) return;
    const current = this.jobs.get(jobId)!;
    const active = this.active.get(jobId)!;
    if (params.repo_id !== undefined && params.repo_id !== active.repoId) return;
    if (method === "progress/source" && current.status === "running") {
      active.admitted = true; active.sent = true;
      const version = sourceVersion(params.source);
      const position = params.position;
      if (!version || !number(position) || position >= current.intent.paths.length) {
        this.failedPersistence(new Error("Native transfer supplied an invalid captured source."));
        return;
      }
      const source = { ...version, position, path: current.intent.paths[position] };
      const previous = this.pendingSources.get(jobId)?.get(position);
      if (previous && !sameSource(previous, source)) {
        this.failedPersistence(new SourceChanged());
        return;
      }
      try {
        this.recordSource(jobId, source);
        const pending = this.pendingSources.get(jobId);
        pending?.delete(position);
        if (!pending?.size) this.pendingSources.delete(jobId);
      } catch (error) {
        if (!(error instanceof SourceChanged)) {
          let pending = this.pendingSources.get(jobId);
          if (!pending) { pending = new Map(); this.pendingSources.set(jobId, pending); }
          pending.set(position, source);
        }
        this.failedPersistence(error);
      }
      if (this.failure || active.cancelRequested) this.cancelUnrecorded();
    } else if (method === "progress/read" && current.status === "running") {
      if (!number(params.received_bytes) || params.received_bytes < (current.transfer?.received_bytes ?? 0)) return;
      active.admitted = true; active.sent = true;
      const transfer: ProgressTransfer = current.transfer ?? { op: current.op, job_id: jobId, repo_id: active.repoId, written_bytes: 0, total_bytes: null };
      this.update(jobId, { transfer: { ...transfer, received_bytes: params.received_bytes } });
      if (this.failure || active.cancelRequested) this.cancelUnrecorded();
    } else if (method === "progress/transfer" && current.status === "running") {
      if (!number(params.written_bytes) || !(params.total_bytes === null || number(params.total_bytes))) return;
      const progress: ProgressTransfer = {
        op: current.op, job_id: jobId, repo_id: active.repoId,
        written_bytes: params.written_bytes, total_bytes: params.total_bytes,
        received_bytes: current.transfer?.received_bytes,
      };
      for (const key of ["checked_bytes", "files_published", "files_total"] as const) if (params[key] === null || number(params[key])) progress[key] = params[key] as number | null;
      active.admitted = true; active.sent = true;
      this.update(jobId, { transfer: progress });
      if (this.failure || active.cancelRequested) this.cancelUnrecorded();
    } else if (method === "progress/complete" && typeof params.status === "number" && Number.isSafeInteger(params.status)) {
      const status = params.outcome === "cancelled" || params.status === -1 ? "cancelled" : params.status === 0 ? "done" : "failed";
      this.update(jobId, { status, exitCode: Number(params.status) });
    }
  }

  owns(jobId: string): boolean { return this.active.has(jobId); }

  requestCancel(repoId: string, jobId: string, session: string): Promise<unknown> {
    const active = this.active.get(jobId);
    if (!active || active.repoId !== repoId || active.session !== session) return Promise.reject(new Error("The transfer's native session ended. Check working files before retrying."));
    active.cancelRequested = true;
    this.update(jobId, { cancelRequested: true });
    if (!active.sent) active.cancelBeforeSend();
    return active.admitted ? this.sendCancellation(jobId, active) : Promise.resolve({ ok: true });
  }

  fence(method: string, value: unknown): void {
    if (!isFileTransfer(method)) return;
    const params = fields(value);
    const active = typeof params?.op_id === "string" ? this.active.get(params.op_id) : undefined;
    if (!active || active.repoId !== params?.repo_id) throw new Error("File transfers require Activity admission.");
    // Preparation may finish after cancellation. Check at the actual native
    // write boundary; an early cancel RPC cannot cancel an unregistered job.
    if (this.stopping || (active.cancelRequested && !active.sent)) throw cancelledBeforeAdmission();
    if (this.failure) throw new Error(this.failure);
    if (active.sent) throw new Error("A file transfer cannot be dispatched twice.");
    active.sent = true;
  }

  close(repoId: string): Promise<unknown> { return this.owner.close(repoId); }
  stop(): Promise<void> {
    this.stopping = true;
    for (const [id, active] of this.active) if (!active.sent) {
      active.cancelRequested = true; active.cancelBeforeSend();
      this.update(id, { cancelRequested: true });
    }
    return this.owner.stop();
  }

  recover(): FileTransferActivity {
    this.db.transaction(() => {
      for (const job of this.jobs.values()) this.insert(job, true);
      for (const [jobId, sources] of this.pendingSources) for (const source of sources.values()) this.recordSource(jobId, source);
      this.prune();
    })();
    this.pendingSources.clear();
    this.failure = null;
    this.changed({ version: ++this.version, error: null });
    return this.snapshot();
  }

  private publish(job: FileTransferJob): void { this.changed({ version: ++this.version, job: structuredClone(job), error: this.failure }); }

  private update(id: string, patch: Partial<FileTransferJob>): void {
    const previous = this.jobs.get(id)!;
    const job = { ...previous, ...patch, revision: previous.revision + 1, updatedAt: Date.now() };
    this.jobs.set(id, job);
    try { this.persist(job); }
    catch (error) { this.failedPersistence(error); }
    this.publish(job);
  }

  private state(job: FileTransferJob): string {
    const { nativeSessionId: _session, intent: _intent, ...state } = job;
    return JSON.stringify(state);
  }

  private insert(job: FileTransferJob, replace = false): void {
    const conflict = replace ? "ON CONFLICT(job_id) DO UPDATE SET state=excluded.state, status=excluded.status, updated_at=excluded.updated_at" : "";
    this.db.prepare(`INSERT INTO file_transfer_jobs(job_id,intent,state,status,updated_at) VALUES(?,?,?,?,?) ${conflict}`)
      .run(job.job_id, JSON.stringify(job.intent), this.state(job), job.status, job.updatedAt);
  }

  private persist(job: FileTransferJob): void {
    this.db.prepare("UPDATE file_transfer_jobs SET state=?,status=?,updated_at=? WHERE job_id=?").run(this.state(job), job.status, job.updatedAt, job.job_id);
  }

  private recordSource(jobId: string, source: FileTransferSource): void {
    const previous = this.db.prepare("SELECT position,path,kind,hash,size,mode FROM file_transfer_sources WHERE job_id=? AND position=?").get(jobId, source.position) as FileTransferSource | undefined;
    // Recovery may retry an interrupted receipt write. A recorded version is
    // immutable: another source belongs to a new attempt, never an overwrite.
    if (previous) {
      if (!sameSource(previous, source)) throw new SourceChanged();
      return;
    }
    this.db.prepare("INSERT INTO file_transfer_sources(job_id,position,path,kind,hash,size,mode) VALUES(?,?,?,?,?,?,?)").run(jobId, source.position, source.path, source.kind, source.hash, source.size, source.mode);
  }

  private prune(): void {
    const finished = this.db.prepare("SELECT job_id FROM file_transfer_jobs WHERE status != 'running' ORDER BY updated_at DESC,job_id").all() as Array<{ job_id: string }>;
    const removed = finished.filter(row => !this.active.has(row.job_id)).slice(HISTORY_LIMIT);
    this.db.transaction(() => {
      for (const row of removed) this.db.prepare("DELETE FROM file_transfer_jobs WHERE job_id=?").run(row.job_id);
    })();
    for (const row of removed) { this.jobs.delete(row.job_id); this.pendingSources.delete(row.job_id); }
    if (removed.length) this.changed({ version: ++this.version, removed: removed.map(row => row.job_id), error: this.failure });
  }

  private failedPersistence(error: unknown): void {
    this.failure = `Activity could not save file-transfer receipts. Check working files and restore writable storage, then reload Activity. ${this.redact(error instanceof Error ? error.message : String(error))}`;
    this.changed({ version: ++this.version, error: this.failure });
    this.cancelUnrecorded();
  }

  private cancelUnrecorded(): void {
    for (const [id, active] of this.active) {
      if (!active.admitted || active.cancelAttempted || (!this.failure && !active.cancelRequested)) continue;
      void this.sendCancellation(id, active).catch(() => {});
    }
  }

  private sendCancellation(id: string, active: { repoId: string; session: string; cancelAttempted: boolean; cancelRequested: boolean; cancellation?: Promise<unknown> }): Promise<unknown> {
    if (!active.cancellation) {
      active.cancelAttempted = true;
      active.cancellation = this.cancel(active.repoId, id, active.session).catch(error => {
        active.cancellation = undefined;
        active.cancelRequested = false;
        this.update(id, { cancelRequested: false, interruption: this.redact(error instanceof Error ? error.message : String(error)) });
        throw error;
      });
    }
    return active.cancellation;
  }
}
