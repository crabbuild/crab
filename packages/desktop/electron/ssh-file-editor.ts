/**
 * Remote file editing over SSH.
 *
 * Provides IPC handlers for reading and writing files on the remote
 * server via the RPC router. Supports conflict detection via mtime
 * comparison and basic file operations (create, delete, rename).
 */

import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface RemoteFileContent {
  content: string;
  path: string;
  mtime: number;
  size: number;
  language: string;
}

export interface WriteResult {
  success: boolean;
  mtime: number;
  conflict?: boolean;
}

// ── Language Detection ────────────────────────────────────────────

const EXTENSION_LANGUAGE_MAP: Record<string, string> = {
  ".ts": "typescript",
  ".tsx": "typescriptreact",
  ".js": "javascript",
  ".jsx": "javascriptreact",
  ".py": "python",
  ".rs": "rust",
  ".go": "go",
  ".rb": "ruby",
  ".java": "java",
  ".c": "c",
  ".cpp": "cpp",
  ".h": "c",
  ".hpp": "cpp",
  ".cs": "csharp",
  ".swift": "swift",
  ".kt": "kotlin",
  ".scala": "scala",
  ".sh": "shellscript",
  ".bash": "shellscript",
  ".zsh": "shellscript",
  ".fish": "shellscript",
  ".json": "json",
  ".yaml": "yaml",
  ".yml": "yaml",
  ".toml": "toml",
  ".xml": "xml",
  ".html": "html",
  ".css": "css",
  ".scss": "scss",
  ".less": "less",
  ".md": "markdown",
  ".sql": "sql",
  ".dockerfile": "dockerfile",
  ".r": "r",
  ".lua": "lua",
  ".vim": "vim",
  ".ex": "elixir",
  ".exs": "elixir",
  ".erl": "erlang",
  ".hs": "haskell",
  ".ml": "ocaml",
  ".clj": "clojure",
  ".php": "php",
  ".pl": "perl",
  ".dart": "dart",
  ".zig": "zig",
  ".nim": "nim",
  ".tf": "terraform",
  ".proto": "protobuf",
  ".graphql": "graphql",
  ".gql": "graphql",
};

function detectLanguage(filePath: string): string {
  const lastDot = filePath.lastIndexOf(".");
  if (lastDot < 0) return "plaintext";

  const ext = filePath.slice(lastDot).toLowerCase();
  return EXTENSION_LANGUAGE_MAP[ext] ?? "plaintext";
}

// ── Remote File Editor ───────────────────────────────────────────

export class RemoteFileEditor {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * Read a file from the remote server. Returns content, mtime for
   * conflict detection, and detected language for syntax highlighting.
   */
  async readFile(profileId: string, filePath: string): Promise<RemoteFileContent> {
    this.assertConnected(profileId);

    const result = await this.router.sendToRemoteAgent(profileId, "read_text", {
      path: filePath,
    });

    const response = result as {
      content: string;
      mtime?: number;
      size?: number;
    };

    return {
      content: response.content ?? "",
      path: filePath,
      mtime: response.mtime ?? Date.now(),
      size: response.size ?? (response.content?.length ?? 0),
      language: detectLanguage(filePath),
    };
  }

  /**
   * Write file content to the remote server. Checks mtime before
   * writing to detect concurrent modifications.
   */
  async writeFile(
    profileId: string,
    filePath: string,
    content: string,
    expectedMtime?: number,
  ): Promise<WriteResult> {
    this.assertConnected(profileId);

    const result = await this.router.sendToRemoteAgent(profileId, "write_text", {
      path: filePath,
      content,
      expected_mtime: expectedMtime,
    });

    const response = result as {
      success?: boolean;
      mtime?: number;
      conflict?: boolean;
    };

    return {
      success: response.success ?? true,
      mtime: response.mtime ?? Date.now(),
      conflict: response.conflict ?? false,
    };
  }

  /**
   * Create a new empty file on the remote server.
   */
  async createFile(profileId: string, filePath: string): Promise<void> {
    this.assertConnected(profileId);

    await this.router.sendToRemoteAgent(profileId, "write_text", {
      path: filePath,
      content: "",
    });
  }

  /**
   * Delete a file on the remote server.
   */
  async deleteFile(profileId: string, filePath: string): Promise<void> {
    this.assertConnected(profileId);

    await this.router.sendToRemoteAgent(profileId, "delete_file", {
      path: filePath,
    });
  }

  /**
   * Rename or move a file on the remote server.
   */
  async renameFile(profileId: string, oldPath: string, newPath: string): Promise<void> {
    this.assertConnected(profileId);

    await this.router.sendToRemoteAgent(profileId, "rename_file", {
      old_path: oldPath,
      new_path: newPath,
    });
  }

  private assertConnected(profileId: string): void {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }
  }
}
