/**
 * Downloads prebuilt LSP server binaries to `~/.crab/lsp/<language>/`
 * on demand. Lets the app work out of the box when system installs
 * aren't present, without bloating the app bundle with per-platform
 * binaries for every language.
 *
 * Strategy:
 * - typescript-language-server: bundled as an npm dep (no download).
 * - rust-analyzer: downloaded from the official GitHub release.
 * - gopls: downloaded from the official Google releases.
 * - pylsp: not supported (needs full Python runtime — user must install).
 */

import { createWriteStream, promises as fsp } from "node:fs";
import { EventEmitter } from "node:events";
import * as https from "node:https";
import * as os from "node:os";
import * as path from "node:path";
import { pipeline } from "node:stream/promises";
import { createGunzip } from "node:zlib";

// ── Types ───────────────────────────────────────────────────────────

export interface InstallProgress {
  language: string;
  phase: "resolving" | "downloading" | "extracting" | "done" | "error";
  /** Fraction 0..1 during download phase; undefined otherwise. */
  percent?: number;
  /** Bytes downloaded so far (download phase). */
  downloaded?: number;
  /** Total bytes (download phase). */
  total?: number;
  /** Error message when phase === "error". */
  message?: string;
  /** Final binary path when phase === "done". */
  installedPath?: string;
}

export type InstallEventListener = (progress: InstallProgress) => void;

// ── Paths ───────────────────────────────────────────────────────────

/** Root directory for downloaded LSP servers: `~/.crab/lsp/`. */
export function installRoot(): string {
  return path.join(os.homedir(), ".crab", "lsp");
}

/** Per-language install dir: `~/.crab/lsp/<language>/`. */
export function languageDir(language: string): string {
  return path.join(installRoot(), language);
}

/**
 * Resolve the final binary path for a language if it's been installed
 * to `~/.crab/lsp/`. Returns null when not installed.
 */
export async function findInstalledBinary(language: string): Promise<string | null> {
  const dir = languageDir(language);
  const binName = binaryName(language);
  if (!binName) return null;
  const full = path.join(dir, binName);
  try {
    await fsp.access(full, fsp.constants.X_OK);
    return full;
  } catch {
    return null;
  }
}

function binaryName(language: string): string | null {
  const ext = process.platform === "win32" ? ".exe" : "";
  switch (language) {
    case "rust":
      return `rust-analyzer${ext}`;
    case "go":
      return `gopls${ext}`;
    default:
      return null;
  }
}

// ── Release URL resolution ──────────────────────────────────────────

/**
 * Map (platform, arch) to the suffix used in release asset names.
 * Returns null when the platform/arch combo isn't supported.
 */
function rustAnalyzerAsset(): string | null {
  const arch = process.arch;
  if (process.platform === "darwin") {
    if (arch === "arm64") return "rust-analyzer-aarch64-apple-darwin.gz";
    if (arch === "x64") return "rust-analyzer-x86_64-apple-darwin.gz";
  } else if (process.platform === "linux") {
    if (arch === "arm64") return "rust-analyzer-aarch64-unknown-linux-gnu.gz";
    if (arch === "x64") return "rust-analyzer-x86_64-unknown-linux-gnu.gz";
  } else if (process.platform === "win32") {
    if (arch === "x64") return "rust-analyzer-x86_64-pc-windows-msvc.gz";
    if (arch === "arm64") return "rust-analyzer-aarch64-pc-windows-msvc.gz";
  }
  return null;
}

// ── Install API ─────────────────────────────────────────────────────

export class LspInstaller extends EventEmitter {
  private inflight = new Map<string, Promise<string>>();

  /**
   * Install the LSP server for `language` to `~/.crab/lsp/<language>/`.
   * Returns the absolute path to the installed binary. Concurrent calls
   * for the same language share the same download.
   */
  async install(language: string): Promise<string> {
    const existing = this.inflight.get(language);
    if (existing) return existing;

    const promise = this.doInstall(language).finally(() => {
      this.inflight.delete(language);
    });
    this.inflight.set(language, promise);
    return promise;
  }

  /** Returns true when the language has an auto-install path. */
  isSupported(language: string): boolean {
    switch (language) {
      case "rust":
        return rustAnalyzerAsset() !== null;
      default:
        return false;
    }
  }

  private emitProgress(progress: InstallProgress): void {
    this.emit("progress", progress);
  }

  private async doInstall(language: string): Promise<string> {
    this.emitProgress({ language, phase: "resolving" });

    try {
      let installed: string;
      switch (language) {
        case "rust":
          installed = await this.installRustAnalyzer();
          break;
        case "go":
          throw new Error("gopls auto-install unsupported — install Go toolchain and run `go install golang.org/x/tools/gopls@latest`");
        default:
          throw new Error(`No installer available for language: ${language}`);
      }
      this.emitProgress({ language, phase: "done", installedPath: installed });
      return installed;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emitProgress({ language, phase: "error", message });
      throw err;
    }
  }

  private async installRustAnalyzer(): Promise<string> {
    const asset = rustAnalyzerAsset();
    if (!asset) {
      throw new Error(`rust-analyzer unavailable for platform ${process.platform}/${process.arch}`);
    }

    // Use "latest" release — rust-analyzer ships nightly but the `latest`
    // tag is kept fresh and stable enough for general use.
    const url = `https://github.com/rust-lang/rust-analyzer/releases/latest/download/${asset}`;
    const dir = languageDir("rust");
    await fsp.mkdir(dir, { recursive: true });

    const finalPath = path.join(dir, binaryName("rust") ?? "rust-analyzer");
    const tmpPath = `${finalPath}.part`;

    await downloadAndExtractGz(url, tmpPath, (downloaded, total) => {
      this.emitProgress({
        language: "rust",
        phase: "downloading",
        downloaded,
        total,
        percent: total > 0 ? downloaded / total : undefined,
      });
    });

    this.emitProgress({ language: "rust", phase: "extracting" });
    await fsp.chmod(tmpPath, 0o755);
    await fsp.rename(tmpPath, finalPath);
    return finalPath;
  }
}

// ── HTTPS download helper with redirect support ─────────────────────

/**
 * Download a `.gz` file and write the decompressed contents to `destPath`.
 * Follows HTTP redirects. Reports progress based on the final download size.
 */
async function downloadAndExtractGz(
  url: string,
  destPath: string,
  onProgress: (downloaded: number, total: number) => void,
): Promise<void> {
  const response = await httpGetFollowRedirects(url);
  const total = parseInt(response.headers["content-length"] ?? "0", 10);
  let downloaded = 0;

  response.on("data", (chunk: Buffer) => {
    downloaded += chunk.length;
    onProgress(downloaded, total);
  });

  // Decompress on the fly and write to disk.
  await pipeline(response, createGunzip(), createWriteStream(destPath));
}

function httpGetFollowRedirects(url: string, maxRedirects = 5): Promise<import("node:http").IncomingMessage> {
  return new Promise((resolve, reject) => {
    const visit = (currentUrl: string, remaining: number) => {
      const req = https.get(currentUrl, (res) => {
        const status = res.statusCode ?? 0;
        if (status >= 300 && status < 400 && res.headers.location) {
          if (remaining <= 0) {
            reject(new Error("Too many redirects"));
            return;
          }
          // Drain so the socket can be reused.
          res.resume();
          const nextUrl = new URL(res.headers.location, currentUrl).toString();
          visit(nextUrl, remaining - 1);
          return;
        }
        if (status !== 200) {
          reject(new Error(`Download failed: HTTP ${status}`));
          res.resume();
          return;
        }
        resolve(res);
      });
      req.on("error", reject);
    };
    visit(url, maxRedirects);
  });
}

// ── Singleton ───────────────────────────────────────────────────────

export const lspInstaller = new LspInstaller();
