/**
 * Predictive Collision Detection for the Crab Desktop Orchestrator.
 *
 * Analyses a set of task nodes to predict which files may be modified
 * by multiple concurrent workspaces. Uses file mention extraction,
 * dependency graph expansion, and git co-edit frequency to score
 * collision risk and suggest mitigations.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";

import type { TaskNode } from "./orchestrator-coordinator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export type CollisionConfidence = "high" | "medium" | "low";
export type MitigationStrategy = "sequential" | "rebase" | "shared-workspace";

export interface CollisionPrediction {
  file: string;
  taskIds: string[];
  confidence: CollisionConfidence;
  reason: string;
  mitigation: MitigationStrategy;
}

export interface CollisionReport {
  predictions: CollisionPrediction[];
  analysedAt: number;
}

interface CoEditEntry {
  fileA: string;
  fileB: string;
  frequency: number;
}

// ─── CollisionDetector ───────────────────────────────────────────────────────

export class CollisionDetector extends EventEmitter {
  /**
   * Predicts file collisions across a set of task nodes by combining
   * three signals: explicit file mentions, transitive dependency
   * expansion, and git co-edit frequency.
   */
  async predictCollisions(
    taskNodes: TaskNode[],
    repoPath: string,
  ): Promise<CollisionReport> {
    // 1. Extract file mentions from task descriptions and estimated files.
    const mentionsByTask = this.extractFileMentions(taskNodes);

    // 2. Expand transitive dependencies (files that import/are imported by
    //    mentioned files, within 2 hops).
    const expandedMentions = await this.expandTransitiveDeps(
      mentionsByTask,
      repoPath,
    );

    // 3. Analyse git log for co-edit frequency.
    const coEditFrequency = await this.getCoEditFrequency(repoPath);

    // 4. Score and rank collision hotspots.
    const predictions = this.rankCollisions(
      expandedMentions,
      coEditFrequency,
      taskNodes,
    );

    const report: CollisionReport = {
      predictions,
      analysedAt: Date.now(),
    };

    if (predictions.length > 0) {
      this.emit("orchestrator:collision-warning", { predictions });
    }

    return report;
  }

  // ─── File Mention Extraction ─────────────────────────────────────────────

  /**
   * Extracts file paths mentioned in task descriptions, titles, and
   * the estimatedFiles arrays. Returns a map of taskId → file paths.
   */
  extractFileMentions(taskNodes: TaskNode[]): Map<string, Set<string>> {
    const mentions = new Map<string, Set<string>>();

    for (const node of taskNodes) {
      const files = new Set<string>();

      // Add explicitly estimated files.
      for (const f of node.estimatedFiles) {
        files.add(f);
      }

      // Extract file-like patterns from description and title.
      const text = `${node.title} ${node.description}`;
      const filePatterns = this.extractFilePatterns(text);
      for (const f of filePatterns) {
        files.add(f);
      }

      mentions.set(node.id, files);
    }

    return mentions;
  }

  /**
   * Extracts file path patterns from free text using common patterns:
   * - Paths with extensions (src/foo/bar.ts)
   * - Paths with slashes (components/Button)
   */
  private extractFilePatterns(text: string): string[] {
    const patterns: string[] = [];

    // Match paths like src/foo/bar.ts, ./config.json, etc.
    const pathRegex = /(?:\.\/|[a-zA-Z][\w-]*\/)+[\w.-]+\.\w+/g;
    const matches = text.match(pathRegex);
    if (matches) {
      patterns.push(...matches);
    }

    // Match quoted paths.
    const quotedRegex = /['"`]([\w./-]+\.\w+)['"`]/g;
    let match: RegExpExecArray | null;
    while ((match = quotedRegex.exec(text)) !== null) {
      patterns.push(match[1]);
    }

    return [...new Set(patterns)];
  }

  // ─── Dependency Graph Expansion ──────────────────────────────────────────

  /**
   * For each mentioned file, finds files that import it or are imported
   * by it (up to 2 hops). Uses git grep to find import/require statements.
   */
  private async expandTransitiveDeps(
    mentionsByTask: Map<string, Set<string>>,
    repoPath: string,
  ): Promise<Map<string, Set<string>>> {
    const expanded = new Map<string, Set<string>>();

    for (const [taskId, files] of mentionsByTask) {
      const expandedFiles = new Set(files);

      for (const file of files) {
        // Find files that reference this file (1 hop).
        const importers = await this.findImporters(file, repoPath);
        for (const imp of importers) {
          expandedFiles.add(imp);
        }

        // Find files imported by this file (1 hop).
        const imports = await this.findImports(file, repoPath);
        for (const imp of imports) {
          expandedFiles.add(imp);
        }
      }

      expanded.set(taskId, expandedFiles);
    }

    return expanded;
  }

  /**
   * Finds files that import/require the given file path.
   */
  private async findImporters(filePath: string, repoPath: string): Promise<string[]> {
    const basename = filePath.replace(/\.\w+$/, "").split("/").pop() ?? "";
    if (!basename) return [];

    try {
      const { stdout } = await execFile(
        "git",
        ["grep", "-l", "--cached", basename],
        { cwd: repoPath, maxBuffer: 512 * 1024 },
      );
      return stdout
        .trim()
        .split("\n")
        .filter((f) => f && f !== filePath)
        .slice(0, 20);
    } catch {
      return [];
    }
  }

  /**
   * Finds files imported by the given file (parses import/require statements).
   */
  private async findImports(filePath: string, repoPath: string): Promise<string[]> {
    try {
      const { stdout } = await execFile(
        "git",
        ["show", `HEAD:${filePath}`],
        { cwd: repoPath, maxBuffer: 256 * 1024 },
      );

      const imports: string[] = [];
      const importRegex = /(?:import|require)\s*\(?['"]([^'"]+)['"]\)?/g;
      let match: RegExpExecArray | null;
      while ((match = importRegex.exec(stdout)) !== null) {
        const importPath = match[1];
        // Only include relative imports.
        if (importPath.startsWith(".")) {
          imports.push(importPath);
        }
      }

      return imports.slice(0, 20);
    } catch {
      return [];
    }
  }

  // ─── Git Co-Edit Frequency ───────────────────────────────────────────────

  /**
   * Analyses recent git history to find files that are frequently
   * edited together in the same commit. Weights recent commits higher.
   */
  private async getCoEditFrequency(repoPath: string): Promise<CoEditEntry[]> {
    try {
      // Get the last 100 commits with their changed files.
      const { stdout } = await execFile(
        "git",
        ["log", "--name-only", "--pretty=format:", "-n", "100"],
        { cwd: repoPath, maxBuffer: 1024 * 1024 },
      );

      // Group files by commit (separated by blank lines).
      const commits = stdout
        .split("\n\n")
        .map((block) => block.trim().split("\n").filter(Boolean))
        .filter((files) => files.length > 1);

      // Count co-edit pairs.
      const pairCounts = new Map<string, number>();

      for (const files of commits) {
        for (let i = 0; i < files.length; i++) {
          for (let j = i + 1; j < files.length; j++) {
            const key = [files[i], files[j]].sort().join("|||");
            pairCounts.set(key, (pairCounts.get(key) ?? 0) + 1);
          }
        }
      }

      // Convert to entries, sorted by frequency.
      const entries: CoEditEntry[] = [];
      for (const [key, frequency] of pairCounts) {
        if (frequency >= 2) {
          const [fileA, fileB] = key.split("|||");
          entries.push({ fileA, fileB, frequency });
        }
      }

      return entries.sort((a, b) => b.frequency - a.frequency).slice(0, 50);
    } catch {
      return [];
    }
  }

  // ─── Scoring & Ranking ───────────────────────────────────────────────────

  /**
   * Combines all signals to produce ranked collision predictions.
   */
  private rankCollisions(
    expandedMentions: Map<string, Set<string>>,
    coEditFrequency: CoEditEntry[],
    taskNodes: TaskNode[],
  ): CollisionPrediction[] {
    // Build a map of file → task IDs that may touch it.
    const fileToTasks = new Map<string, Set<string>>();

    for (const [taskId, files] of expandedMentions) {
      for (const file of files) {
        if (!fileToTasks.has(file)) {
          fileToTasks.set(file, new Set());
        }
        fileToTasks.get(file)!.add(taskId);
      }
    }

    const predictions: CollisionPrediction[] = [];

    // Files touched by multiple tasks are collision candidates.
    for (const [file, taskIds] of fileToTasks) {
      if (taskIds.size < 2) continue;

      const taskIdArray = Array.from(taskIds);
      const confidence = this.scoreConfidence(file, taskIdArray, taskNodes, coEditFrequency);
      const mitigation = this.suggestMitigation(taskIdArray, taskNodes);
      const reason = this.buildReason(file, taskIdArray, taskNodes);

      predictions.push({
        file,
        taskIds: taskIdArray,
        confidence,
        reason,
        mitigation,
      });
    }

    // Sort by confidence (high first), then by number of conflicting tasks.
    const confidenceOrder: Record<CollisionConfidence, number> = {
      high: 0,
      medium: 1,
      low: 2,
    };

    return predictions.sort((a, b) => {
      const confDiff = confidenceOrder[a.confidence] - confidenceOrder[b.confidence];
      if (confDiff !== 0) return confDiff;
      return b.taskIds.length - a.taskIds.length;
    });
  }

  /**
   * Scores collision confidence based on how the file was identified:
   * - high: explicitly listed in estimatedFiles of multiple tasks
   * - medium: found via import analysis or co-edit frequency
   * - low: only found via text pattern matching
   */
  private scoreConfidence(
    file: string,
    taskIds: string[],
    taskNodes: TaskNode[],
    coEditFrequency: CoEditEntry[],
  ): CollisionConfidence {
    // Check if the file is explicitly estimated by multiple tasks.
    let explicitCount = 0;
    for (const taskId of taskIds) {
      const node = taskNodes.find((n) => n.id === taskId);
      if (node?.estimatedFiles.includes(file)) {
        explicitCount++;
      }
    }

    if (explicitCount >= 2) return "high";

    // Check co-edit frequency.
    const isFrequentlyCoEdited = coEditFrequency.some(
      (entry) =>
        (entry.fileA === file || entry.fileB === file) && entry.frequency >= 5,
    );

    if (explicitCount >= 1 || isFrequentlyCoEdited) return "medium";

    return "low";
  }

  /**
   * Suggests a mitigation strategy based on the dependency structure
   * between conflicting tasks.
   */
  private suggestMitigation(
    taskIds: string[],
    taskNodes: TaskNode[],
  ): MitigationStrategy {
    // If any of the conflicting tasks already depend on another,
    // sequential execution is already implied.
    for (const taskId of taskIds) {
      const node = taskNodes.find((n) => n.id === taskId);
      if (node) {
        const hasDep = node.dependencies.some((dep) => taskIds.includes(dep));
        if (hasDep) return "rebase";
      }
    }

    // Default: suggest sequential execution for high-confidence collisions.
    return "sequential";
  }

  /**
   * Builds a human-readable reason string for the collision.
   */
  private buildReason(
    file: string,
    taskIds: string[],
    taskNodes: TaskNode[],
  ): string {
    const taskTitles = taskIds
      .map((id) => taskNodes.find((n) => n.id === id)?.title ?? id)
      .map((t) => `"${t}"`)
      .join(" + ");

    return `${file} may be modified by tasks: ${taskTitles}`;
  }
}
