/**
 * Delta Sync Engine — computes content-defined chunk signatures and
 * transfers only differing chunks between local and remote files.
 *
 * Uses gearhash-based content-defined chunking (CDC) to split files into
 * variable-size chunks, then compares chunk hashes to identify differences.
 * Only new/changed chunks are transferred, saving bandwidth for large files
 * with small modifications.
 *
 * Parameters match crab's core chunker:
 * - Target chunk size: 64 KiB
 * - Min chunk size: 8 KiB
 * - Max chunk size: 128 KiB
 *
 * Delegates remote-side chunking to the remote agent via RPC
 * (method: "chunk_signatures"). Local chunking uses the same gearhash
 * algorithm for deterministic, content-defined boundaries.
 */

import * as fs from "node:fs";
import { createHash as createBlake3 } from "blake3";
import type { RpcRouter } from "./ssh-router";

// ── Constants ────────────────────────────────────────────────────

/** Files smaller than this are transferred in full (no delta). */
const MIN_FILE_SIZE_FOR_DELTA = 1 * 1024 * 1024; // 1 MB

/** If more than this fraction of chunks differ, fall back to full transfer. */
const CHANGE_THRESHOLD_PERCENT = 80;

/** Maximum time allowed for delta computation before fallback. */
const COMPUTATION_TIMEOUT_MS = 30_000;

/** Target chunk size for gearhash CDC (matches crab's default). */
const TARGET_CHUNK_SIZE = 64 * 1024; // 64 KiB

/** Minimum chunk size (target / 8). */
const MIN_CHUNK_SIZE = TARGET_CHUNK_SIZE / 8; // 8 KiB

/** Maximum chunk size (target × 2). */
const MAX_CHUNK_SIZE = TARGET_CHUNK_SIZE * 2; // 128 KiB

/**
 * Gearhash lookup table — 256 pseudo-random 32-bit values used by the
 * rolling hash to map each byte to a hash contribution. This table is
 * deterministic and matches the standard gearhash implementation.
 */
const GEAR_TABLE: Uint32Array = generateGearTable();

/** Mask for detecting chunk boundaries (targets ~64 KiB average). */
const CHUNK_MASK = (1 << 16) - 1; // 16 bits → avg 64 KiB

// ── Types ────────────────────────────────────────────────────────

export interface ChunkSignature {
  offset: number;
  length: number;
  hash: string;
}

export interface DeltaPlan {
  sourceHash: string;
  targetHash: string;
  matchedChunks: number;
  newChunks: ChunkSignature[];
  totalSavingsPercent: number;
  fallbackToFull: boolean;
}

// ── Gearhash CDC Implementation ──────────────────────────────────

/**
 * Generate the gearhash lookup table using a deterministic PRNG.
 * Uses a simple multiplicative hash (Knuth's) seeded with the byte index.
 */
function generateGearTable(): Uint32Array {
  const table = new Uint32Array(256);
  let state = 0x6b326ac4;
  for (let i = 0; i < 256; i++) {
    state = Math.imul(state, 6364136223846793005 & 0xffffffff) + 1442695040888963407;
    state = (state >>> 0) ^ (i * 2654435761);
    table[i] = state >>> 0;
  }
  return table;
}

/**
 * Split a buffer into content-defined chunks using gearhash rolling hash.
 * Returns chunk boundaries as [offset, length] pairs.
 */
function gearhashChunk(data: Buffer): Array<{ offset: number; length: number }> {
  const chunks: Array<{ offset: number; length: number }> = [];
  let chunkStart = 0;
  let hash = 0;

  for (let i = 0; i < data.length; i++) {
    hash = ((hash << 1) | (hash >>> 31)) ^ GEAR_TABLE[data[i]];
    hash = hash >>> 0;

    const chunkLen = i - chunkStart + 1;

    if (chunkLen >= MIN_CHUNK_SIZE) {
      const isMaxSize = chunkLen >= MAX_CHUNK_SIZE;
      const isBoundary = (hash & CHUNK_MASK) === 0;

      if (isMaxSize || isBoundary) {
        chunks.push({ offset: chunkStart, length: chunkLen });
        chunkStart = i + 1;
        hash = 0;
      }
    }
  }

  // Flush remaining bytes as the final chunk
  if (chunkStart < data.length) {
    chunks.push({ offset: chunkStart, length: data.length - chunkStart });
  }

  return chunks;
}

/**
 * Compute Blake3 hash of a buffer slice, returned as hex string.
 */
function blake3Hash(data: Buffer): string {
  const hasher = createBlake3();
  hasher.update(data);
  const hex = hasher.digest("hex") as string;
  hasher.dispose();
  return hex;
}

/**
 * Compute chunk signatures for a local file: split via gearhash CDC,
 * then Blake3-hash each chunk.
 */
function computeLocalSignatures(fileData: Buffer): {
  signatures: ChunkSignature[];
  fileHash: string;
} {
  const boundaries = gearhashChunk(fileData);
  const signatures: ChunkSignature[] = boundaries.map(({ offset, length }) => ({
    offset,
    length,
    hash: blake3Hash(fileData.subarray(offset, offset + length)),
  }));

  const fileHash = blake3Hash(fileData);
  return { signatures, fileHash };
}

// ── DeltaSyncEngine ──────────────────────────────────────────────

export class DeltaSyncEngine {
  constructor(
    private router: RpcRouter,
  ) {}

  /**
   * Compute a delta plan between a local file and its remote counterpart.
   *
   * Returns a DeltaPlan indicating which chunks differ. If the file is
   * below the 1 MB threshold, or if >80% of chunks differ, or if
   * computation exceeds 30s, the plan indicates fallbackToFull=true.
   */
  async computeDelta(
    profileId: string,
    localPath: string,
    remotePath: string,
  ): Promise<DeltaPlan> {
    // Check file size — skip delta for files < 1 MB
    let stat: fs.Stats;
    try {
      stat = fs.statSync(localPath);
    } catch {
      return this.fullTransferPlan("", "");
    }

    if (stat.size < MIN_FILE_SIZE_FOR_DELTA) {
      return this.fullTransferPlan("", "");
    }

    // Race computation against timeout
    const timeoutPromise = new Promise<DeltaPlan>((resolve) => {
      setTimeout(() => {
        resolve(this.fullTransferPlan("", "", "computation timeout exceeded 30s"));
      }, COMPUTATION_TIMEOUT_MS);
    });

    const computePromise = this.computeDeltaInternal(profileId, localPath, remotePath);

    return Promise.race([computePromise, timeoutPromise]);
  }

  /**
   * Upload only the new/changed chunks to the remote file.
   * Sends chunk data via RPC "write_chunks" method.
   */
  async applyDeltaUpload(
    profileId: string,
    plan: DeltaPlan,
    localPath: string,
    remotePath: string,
  ): Promise<void> {
    if (plan.fallbackToFull || plan.newChunks.length === 0) {
      return;
    }

    const fileData = fs.readFileSync(localPath);

    // Prepare chunks to send: extract data for each new chunk
    const chunksToSend = plan.newChunks.map((chunk) => ({
      offset: chunk.offset,
      data_base64: fileData.subarray(chunk.offset, chunk.offset + chunk.length).toString("base64"),
    }));

    await this.router.sendToRemoteAgent(profileId, "write_chunks", {
      path: remotePath,
      chunks: chunksToSend,
      expected_hash: plan.sourceHash,
    });
  }

  /**
   * Download only the changed chunks from the remote and reconstruct
   * the file locally.
   */
  async applyDeltaDownload(
    profileId: string,
    plan: DeltaPlan,
    localPath: string,
    remotePath: string,
  ): Promise<void> {
    if (plan.fallbackToFull || plan.newChunks.length === 0) {
      return;
    }

    // Request the changed chunks from the remote
    const response = (await this.router.sendToRemoteAgent(profileId, "read_chunks", {
      path: remotePath,
      chunks: plan.newChunks.map((c) => ({ offset: c.offset, length: c.length })),
    })) as { chunks: Array<{ offset: number; data_base64: string }> };

    // Read the existing local file
    const localData = fs.readFileSync(localPath);

    // Determine the target file size from the remote hash context.
    // We reconstruct by overlaying the new chunks onto the local file.
    const targetSize = Math.max(
      localData.length,
      ...plan.newChunks.map((c) => c.offset + c.length),
    );
    const result = Buffer.alloc(targetSize);

    // Copy existing local data as the base
    localData.copy(result, 0, 0, Math.min(localData.length, targetSize));

    // Overlay the downloaded chunks
    for (const chunk of response.chunks) {
      const data = Buffer.from(chunk.data_base64, "base64");
      data.copy(result, chunk.offset);
    }

    // Truncate to actual target size if needed
    const finalSize = Math.max(
      ...response.chunks.map((c) => c.offset + Buffer.from(c.data_base64, "base64").length),
      ...plan.newChunks.map((c) => c.offset + c.length),
    );
    const finalBuffer = result.subarray(0, finalSize > 0 ? finalSize : result.length);

    fs.writeFileSync(localPath, finalBuffer);
  }

  /**
   * Verify that a file on disk matches the expected Blake3 hash.
   */
  async verifyIntegrity(filePath: string, expectedHash: string): Promise<boolean> {
    const hasher = createBlake3();
    const fileStream = fs.createReadStream(filePath);

    for await (const chunk of fileStream) {
      hasher.update(chunk as Buffer);
    }

    const actualHash = hasher.digest("hex") as string;
    hasher.dispose();
    return actualHash === expectedHash.toLowerCase();
  }

  // ── Private helpers ────────────────────────────────────────────

  /**
   * Internal delta computation: reads local file, computes local
   * signatures, requests remote signatures via RPC, and compares.
   */
  private async computeDeltaInternal(
    profileId: string,
    localPath: string,
    remotePath: string,
  ): Promise<DeltaPlan> {
    // Read local file and compute signatures
    const fileData = fs.readFileSync(localPath);
    const local = computeLocalSignatures(fileData);

    // Request remote chunk signatures via RPC
    let remoteResult: { signatures: ChunkSignature[]; file_hash: string };
    try {
      remoteResult = (await this.router.sendToRemoteAgent(profileId, "chunk_signatures", {
        path: remotePath,
        algorithm: "gearhash",
      })) as { signatures: ChunkSignature[]; file_hash: string };
    } catch {
      // RPC failure — fall back to full transfer
      return this.fullTransferPlan(local.fileHash, "", "remote chunk_signatures RPC failed");
    }

    const remoteHash = remoteResult.file_hash;
    const remoteSignatures = remoteResult.signatures;

    // If file hashes match, no transfer needed
    if (local.fileHash === remoteHash) {
      return {
        sourceHash: local.fileHash,
        targetHash: remoteHash,
        matchedChunks: local.signatures.length,
        newChunks: [],
        totalSavingsPercent: 100,
        fallbackToFull: false,
      };
    }

    // Build a set of remote chunk hashes for fast lookup
    const remoteHashSet = new Set(remoteSignatures.map((s) => s.hash));

    // Find local chunks that don't exist on the remote (new/changed)
    const newChunks = local.signatures.filter((s) => !remoteHashSet.has(s.hash));
    const matchedChunks = local.signatures.length - newChunks.length;

    // Check if too many chunks differ (>80% changed)
    const totalChunks = local.signatures.length;
    const changePercent = totalChunks > 0 ? (newChunks.length / totalChunks) * 100 : 100;

    if (changePercent > CHANGE_THRESHOLD_PERCENT) {
      return this.fullTransferPlan(
        local.fileHash,
        remoteHash,
        `${changePercent.toFixed(1)}% of chunks changed, exceeds 80% threshold`,
      );
    }

    // Compute savings: bytes saved = matched chunk bytes / total file size
    const newChunkBytes = newChunks.reduce((sum, c) => sum + c.length, 0);
    const totalSavingsPercent =
      fileData.length > 0
        ? ((fileData.length - newChunkBytes) / fileData.length) * 100
        : 0;

    return {
      sourceHash: local.fileHash,
      targetHash: remoteHash,
      matchedChunks,
      newChunks,
      totalSavingsPercent,
      fallbackToFull: false,
    };
  }

  /**
   * Create a DeltaPlan that indicates full transfer is needed.
   */
  private fullTransferPlan(
    sourceHash: string,
    targetHash: string,
    _reason?: string,
  ): DeltaPlan {
    return {
      sourceHash,
      targetHash,
      matchedChunks: 0,
      newChunks: [],
      totalSavingsPercent: 0,
      fallbackToFull: true,
    };
  }
}
