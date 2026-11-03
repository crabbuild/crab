/**
 * Large-File Awareness for the Crab Desktop Orchestrator.
 *
 * Detects Crab-tracked large files in workspaces, injects context into
 * agent prompts (so agents don't corrupt pointer blobs), validates pointer
 * integrity before merge, and provides hydrate/dehydrate actions.
 */

import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as path from "node:path";

import type { Task } from "./orchestrator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ValidationResult {
  file: string;
  status: "valid" | "corrupted";
  message: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

/** Crab pointer files are always small — under 1 KB. */
const MAX_POINTER_SIZE = 1024;

/** The version line that starts every valid Crab pointer blob. */
const POINTER_VERSION_PREFIX = "version https://crab.dev/pointer/";

// ─── LargeFileAwareness ─────────────────────────────────────────────────────

export class LargeFileAwareness {
  // ─── Detection (30.1 + 30.2) ───────────────────────────────────────────

  /**
   * Detect Crab-tracked files in a worktree by parsing `.gitattributes`
   * for `filter=crab` patterns and expanding them via `git ls-files`.
   */
  async getTrackedFiles(worktreePath: string): Promise<string[]> {
    const patterns = await this.parseGitattributes(worktreePath);
    if (patterns.length === 0) return [];
    return this.expandPatterns(worktreePath, patterns);
  }

  /**
   * Parse `.gitattributes` for lines containing `filter=crab` and
   * extract the glob pattern (first whitespace-delimited token).
   */
  private async parseGitattributes(worktreePath: string): Promise<string[]> {
    const attrsPath = path.join(worktreePath, ".gitattributes");
    let content: string;
    try {
      content = await fs.promises.readFile(attrsPath, "utf-8");
    } catch {
      return [];
    }

    const patterns: string[] = [];
    for (const line of content.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith("#")) continue;
      if (trimmed.includes("filter=crab")) {
        const pattern = trimmed.split(/\s+/)[0];
        if (pattern) {
          patterns.push(pattern);
        }
      }
    }
    return patterns;
  }

  /**
   * Expand glob patterns to actual file paths using `git ls-files`.
   * Falls back to returning patterns as-is if git is unavailable.
   */
  private async expandPatterns(
    worktreePath: string,
    patterns: string[],
  ): Promise<string[]> {
    const allFiles = new Set<string>();

    for (const pattern of patterns) {
      try {
        const { stdout } = await execFile(
          "git",
          ["ls-files", "--", pattern],
          { cwd: worktreePath },
        );
        for (const file of stdout.split("\n")) {
          const trimmed = file.trim();
          if (trimmed) allFiles.add(trimmed);
        }
      } catch {
        // If git ls-files fails for a pattern, skip it.
      }
    }

    return Array.from(allFiles).sort();
  }

  // ─── Context Injection (30.3) ──────────────────────────────────────────

  /**
   * Build context injection string to prepend to agent prompts.
   * Lists tracked files and instructs the agent not to modify pointer
   * content directly.
   */
  buildContextInjection(trackedFiles: string[]): string {
    if (trackedFiles.length === 0) return "";
    return [
      "## Large-File Tracked Files",
      "",
      "The following files are tracked by Crab's large-file system.",
      "They appear as pointer blobs in the working tree unless hydrated.",
      "Do NOT modify the pointer content directly. If you need to read",
      "or modify these files, ask the user to hydrate them first.",
      "",
      ...trackedFiles.map((f) => `- ${f}`),
    ].join("\n");
  }

  // ─── Pointer Validation (30.4) ─────────────────────────────────────────

  /**
   * Validate that all tracked files in a task's worktree are either
   * valid pointer blobs or legitimately hydrated content.
   *
   * A file is considered a pointer blob if it is small (< 1 KB) and
   * starts with the Crab pointer version prefix. If it looks like a
   * pointer but has an invalid format, it's marked as corrupted.
   */
  async validatePointers(task: Task): Promise<ValidationResult[]> {
    const trackedFiles = await this.getTrackedFiles(task.worktreePath);
    const results: ValidationResult[] = [];

    for (const file of trackedFiles) {
      const filePath = path.join(task.worktreePath, file);
      try {
        const stat = await fs.promises.stat(filePath);

        // Large files are hydrated content — valid by definition.
        if (stat.size >= MAX_POINTER_SIZE) {
          results.push({
            file,
            status: "valid",
            message: "File is hydrated content",
          });
          continue;
        }

        // Small file — check if it's a valid pointer blob.
        const content = await fs.promises.readFile(filePath, "utf-8");

        if (this.looksLikePointer(content)) {
          if (this.isValidPointer(content)) {
            results.push({
              file,
              status: "valid",
              message: "Valid pointer blob",
            });
          } else {
            results.push({
              file,
              status: "corrupted",
              message: "Pointer blob has invalid format",
            });
          }
        } else {
          // Small file that doesn't look like a pointer — could be
          // a legitimately small tracked file that's hydrated.
          results.push({
            file,
            status: "valid",
            message: "Small hydrated file",
          });
        }
      } catch {
        // File doesn't exist or can't be read — skip.
      }
    }

    return results;
  }

  /**
   * Check if content looks like it could be a pointer blob
   * (starts with the version prefix).
   */
  private looksLikePointer(content: string): boolean {
    return content.startsWith(POINTER_VERSION_PREFIX);
  }

  /**
   * Validate the full structure of a Crab pointer blob.
   *
   * Expected format:
   * ```
   * version https://crab.dev/pointer/v1
   * oid blake3:<64-char-hex-hash>
   * size <positive-integer>
   * ```
   */
  private isValidPointer(content: string): boolean {
    const lines = content.trim().split("\n");
    if (lines.length < 3) return false;

    // Line 1: version
    if (!lines[0].startsWith(POINTER_VERSION_PREFIX)) return false;

    // Line 2: oid blake3:<hash>
    const oidMatch = lines[1].match(/^oid blake3:([0-9a-f]{64})$/);
    if (!oidMatch) return false;

    // Line 3: size <number>
    const sizeMatch = lines[2].match(/^size (\d+)$/);
    if (!sizeMatch) return false;

    const size = parseInt(sizeMatch[1], 10);
    if (size <= 0 || !Number.isFinite(size)) return false;

    return true;
  }

  // ─── Hydrate / Dehydrate Actions (30.7, 30.8, 30.9) ───────────────────

  /**
   * Hydrate specific files in the worktree so the agent can read their
   * full content. Calls `crab hydrate` with the given patterns.
   */
  async hydrateFiles(worktreePath: string, patterns: string[]): Promise<void> {
    if (patterns.length === 0) return;
    await execFile(process.env.CRAB_BIN || "crab", ["hydrate", "--", ...patterns], {
      cwd: worktreePath,
    });
  }

  /**
   * Dehydrate all tracked files in the worktree back to pointer form.
   * Should be called before merging to ensure pointer blobs are intact.
   */
  async dehydrateFiles(worktreePath: string): Promise<void> {
    await execFile(process.env.CRAB_BIN || "crab", ["dehydrate", "--all"], {
      cwd: worktreePath,
    });
  }
}
