import { EventEmitter } from "node:events";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import type { Client, ClientChannel, SFTPWrapper } from "ssh2";

// ── Deploy progress ──────────────────────────────────────────────

export interface DeployProgress {
  stage: "detecting" | "uploading" | "verifying" | "starting";
  percent: number;
  speed?: number; // bytes/sec during upload
  eta?: number; // seconds remaining
}

// ── Deployment errors ────────────────────────────────────────────

export type DeployErrorCode =
  | "DISK_FULL"
  | "PERMISSION_DENIED"
  | "BINARY_RUNNING"
  | "WRONG_ARCH"
  | "UPLOAD_FAILED"
  | "START_FAILED"
  | "ARCH_DETECT_FAILED";

export class DeployError extends Error {
  constructor(
    public readonly code: DeployErrorCode,
    message: string,
    public readonly suggestion?: string,
  ) {
    super(message);
    this.name = "DeployError";
  }
}

// ── Supported architectures ──────────────────────────────────────

const ARCH_MAP: Record<string, Record<string, string>> = {
  Linux: {
    x86_64: "crab-desktop-agent-x86_64-linux",
    aarch64: "crab-desktop-agent-aarch64-linux",
    arm64: "crab-desktop-agent-aarch64-linux",
  },
  Darwin: {
    arm64: "crab-desktop-agent-aarch64-darwin",
    aarch64: "crab-desktop-agent-aarch64-darwin",
  },
};

const REMOTE_AGENT_DIR = "~/.crab/bin";
const REMOTE_AGENT_PATH = "~/.crab/bin/crab-desktop-agent";

// ── Agent deployer ───────────────────────────────────────────────

/**
 * Manages uploading and running the crab-desktop-agent binary on
 * remote servers over SSH. Handles architecture detection, version
 * comparison, hash-based skip, and graceful error handling.
 *
 * Events:
 * - "ssh:deploy-progress" (DeployProgress)
 */
export class AgentDeployer extends EventEmitter {
  private localAgentVersion: string | null = null;

  /**
   * Detect the remote server's OS and architecture by running
   * `uname -s` and `uname -m` over the SSH exec channel.
   *
   * Returns the binary name to deploy (e.g., "crab-desktop-agent-x86_64-linux").
   */
  async detectRemoteArch(client: Client): Promise<string> {
    this.emitProgress("detecting", 0);

    const output = await this.execCommand(client, "uname -s && uname -m");
    const lines = output.trim().split("\n").map((l) => l.trim());

    if (lines.length < 2) {
      throw new DeployError(
        "ARCH_DETECT_FAILED",
        `Failed to detect remote architecture: unexpected output "${output.trim()}"`,
        "Ensure the remote server supports `uname -s` and `uname -m` commands.",
      );
    }

    const osName = lines[0]; // e.g., "Linux" or "Darwin"
    const archName = lines[1]; // e.g., "x86_64" or "aarch64"

    const osMap = ARCH_MAP[osName];
    if (!osMap) {
      throw new DeployError(
        "WRONG_ARCH",
        `Unsupported remote OS: "${osName}". Supported: Linux, Darwin.`,
        "The crab-desktop-agent is only available for Linux and macOS.",
      );
    }

    const binaryName = osMap[archName];
    if (!binaryName) {
      throw new DeployError(
        "WRONG_ARCH",
        `Unsupported architecture: "${osName} ${archName}". Supported: x86_64, aarch64/arm64.`,
        `The remote server reports architecture "${archName}" which is not supported.`,
      );
    }

    this.emitProgress("detecting", 100);
    return binaryName;
  }

  /**
   * Check if the remote agent is already installed and return its
   * version string, or null if not found / not executable.
   */
  async checkRemoteAgent(client: Client): Promise<string | null> {
    try {
      const output = await this.execCommand(
        client,
        `${REMOTE_AGENT_PATH} --version`,
        { ignoreExitCode: true },
      );

      const trimmed = output.trim();
      if (!trimmed || trimmed.includes("not found") || trimmed.includes("No such file")) {
        return null;
      }

      // Expected format: "crab-desktop-agent X.Y.Z" or just "X.Y.Z"
      const versionMatch = trimmed.match(/(\d+\.\d+\.\d+)/);
      return versionMatch ? versionMatch[1] : null;
    } catch {
      return null;
    }
  }

  /**
   * Deploy the agent binary to the remote server via SFTP.
   * Creates the target directory, uploads the binary, and sets
   * executable permissions. Emits progress events throughout.
   */
  async deploy(client: Client, arch: string): Promise<void> {
    const localPath = this.getLocalBinaryPath(arch);

    if (!fs.existsSync(localPath)) {
      throw new DeployError(
        "UPLOAD_FAILED",
        `Local agent binary not found at "${localPath}".`,
        "The agent binary may not be bundled with this build. Try reinstalling the application.",
      );
    }

    // Check if the binary is already running on the remote
    await this.checkBinaryNotRunning(client);

    // Check remote disk space before uploading
    await this.checkDiskSpace(client, localPath);

    this.emitProgress("uploading", 0);

    const sftp = await this.openSftp(client);

    try {
      // Create the target directory
      await this.mkdirRemote(sftp, REMOTE_AGENT_DIR);

      // Upload the binary with progress tracking
      await this.uploadWithProgress(sftp, localPath, REMOTE_AGENT_PATH);

      // Set executable permissions
      await this.chmodRemote(sftp, REMOTE_AGENT_PATH, 0o755);
    } finally {
      sftp.end();
    }

    this.emitProgress("uploading", 100);
  }

  /**
   * Compare local and remote agent versions using semver.
   * Returns:
   *  - "upgrade" if remote is older than local
   *  - "downgrade" if remote is newer than local
   *  - "current" if versions match
   */
  compareVersions(local: string, remote: string): "upgrade" | "downgrade" | "current" {
    const localParts = this.parseSemver(local);
    const remoteParts = this.parseSemver(remote);

    for (let i = 0; i < 3; i++) {
      if (localParts[i] > remoteParts[i]) return "upgrade";
      if (localParts[i] < remoteParts[i]) return "downgrade";
    }

    return "current";
  }

  /**
   * Compute the SHA-256 hash of the local agent binary.
   */
  computeLocalHash(binaryPath: string): string {
    const content = fs.readFileSync(binaryPath);
    return crypto.createHash("sha256").update(content).digest("hex");
  }

  /**
   * Get the SHA-256 hash of the remote agent binary by running
   * `sha256sum` on the remote server.
   *
   * Returns the hex hash string, or null if the file doesn't exist
   * or the command fails.
   */
  async checkRemoteHash(client: Client): Promise<string | null> {
    try {
      const output = await this.execCommand(
        client,
        `sha256sum ${REMOTE_AGENT_PATH} 2>/dev/null || shasum -a 256 ${REMOTE_AGENT_PATH} 2>/dev/null`,
        { ignoreExitCode: true },
      );

      const trimmed = output.trim();
      if (!trimmed || trimmed.includes("No such file")) {
        return null;
      }

      // sha256sum output format: "<hash>  <filename>"
      const hashMatch = trimmed.match(/^([a-f0-9]{64})/);
      return hashMatch ? hashMatch[1] : null;
    } catch {
      return null;
    }
  }

  /**
   * Start the remote agent process over an SSH exec channel.
   * Wires stdin/stdout as the JSON-RPC (NDJSON) transport.
   * Verifies the agent is responsive with a ping RPC call.
   *
   * Returns the SSH channel for ongoing JSON-RPC communication.
   */
  async startRemoteAgent(client: Client): Promise<ClientChannel> {
    this.emitProgress("starting", 0);

    const channel = await new Promise<ClientChannel>((resolve, reject) => {
      client.exec(
        `RUST_LOG=info ${REMOTE_AGENT_PATH}`,
        { env: { RUST_LOG: "info" } },
        (err, stream) => {
          if (err) {
            reject(
              new DeployError(
                "START_FAILED",
                `Failed to start remote agent: ${err.message}`,
                "Check that the agent binary is executable and the correct architecture.",
              ),
            );
            return;
          }
          resolve(stream);
        },
      );
    });

    this.emitProgress("starting", 50);

    // Verify the agent is responsive with a ping RPC
    await this.verifyAgentPing(channel);

    this.emitProgress("starting", 100);
    return channel;
  }

  /**
   * Resolve the path to the bundled agent binary for the given
   * architecture name. Checks both the Electron resources path
   * (packaged app) and the local resources directory (development).
   */
  getLocalBinaryPath(arch: string): string {
    // In a packaged Electron app, process.resourcesPath points to
    // the app's Resources directory.
    const resourcesPath = this.getResourcesPath();
    const bundledPath = path.join(resourcesPath, "agents", arch);

    if (fs.existsSync(bundledPath)) {
      return bundledPath;
    }

    // Fallback for development: check the project's resources/agents/ directory
    const devPath = path.join(__dirname, "..", "resources", "agents", arch);
    if (fs.existsSync(devPath)) {
      return devPath;
    }

    return bundledPath;
  }

  /**
   * Get the local agent version from the bundled binary's version
   * output or from the Cargo.toml version field.
   */
  getLocalAgentVersion(): string {
    if (this.localAgentVersion) {
      return this.localAgentVersion;
    }

    // Read version from package.json as a fallback (agent version
    // should match the app version in a release build).
    let version = "0.1.0";
    try {
      const pkgPath = path.join(__dirname, "..", "package.json");
      const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf-8"));
      version = pkg.version ?? "0.1.0";
    } catch {
      // Use default version
    }

    this.localAgentVersion = version;
    return version;
  }

  /**
   * Full deployment flow: detect arch, check version, compare hashes,
   * deploy if needed, start the agent.
   *
   * Returns the SSH channel wired as the JSON-RPC transport.
   */
  async ensureAgent(client: Client): Promise<ClientChannel> {
    // Step 1: Detect remote architecture
    const arch = await this.detectRemoteArch(client);

    // Step 2: Check if agent is already installed
    const remoteVersion = await this.checkRemoteAgent(client);
    const localVersion = this.getLocalAgentVersion();
    const localBinaryPath = this.getLocalBinaryPath(arch);

    let needsDeploy = false;

    if (!remoteVersion) {
      // Agent not installed
      needsDeploy = true;
    } else {
      // Compare versions
      const comparison = this.compareVersions(localVersion, remoteVersion);
      if (comparison === "upgrade") {
        needsDeploy = true;
      } else if (comparison === "current") {
        // Same version — check hash to see if binary is identical
        this.emitProgress("verifying", 0);
        const localHash = this.computeLocalHash(localBinaryPath);
        const remoteHash = await this.checkRemoteHash(client);
        this.emitProgress("verifying", 100);

        if (remoteHash && localHash === remoteHash) {
          needsDeploy = false;
        } else {
          needsDeploy = true;
        }
      }
      // If "downgrade" (remote is newer), don't deploy — warn but proceed
    }

    if (needsDeploy) {
      await this.deploy(client, arch);
    }

    // Step 3: Start the agent
    return this.startRemoteAgent(client);
  }

  // ── Private helpers ────────────────────────────────────────────

  private emitProgress(stage: DeployProgress["stage"], percent: number, speed?: number, eta?: number): void {
    const progress: DeployProgress = { stage, percent, speed, eta };
    this.emit("ssh:deploy-progress", progress);
  }

  /**
   * Execute a command on the remote server and return its stdout output.
   */
  private execCommand(
    client: Client,
    command: string,
    options?: { ignoreExitCode?: boolean; timeout?: number },
  ): Promise<string> {
    const timeout = options?.timeout ?? 15000;

    return new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`Command timed out after ${timeout}ms: ${command}`));
      }, timeout);

      client.exec(command, (err, stream) => {
        if (err) {
          clearTimeout(timer);
          reject(err);
          return;
        }

        let stdout = "";
        let stderr = "";

        stream.on("data", (data: Buffer) => {
          stdout += data.toString();
        });

        stream.stderr.on("data", (data: Buffer) => {
          stderr += data.toString();
        });

        stream.on("close", (code: number) => {
          clearTimeout(timer);
          if (code !== 0 && !options?.ignoreExitCode) {
            reject(new Error(`Command failed (exit ${code}): ${command}\n${stderr}`));
          } else {
            resolve(stdout);
          }
        });

        stream.on("error", (streamErr: Error) => {
          clearTimeout(timer);
          reject(streamErr);
        });
      });
    });
  }

  /**
   * Open an SFTP session on the SSH connection.
   */
  private openSftp(client: Client): Promise<SFTPWrapper> {
    return new Promise<SFTPWrapper>((resolve, reject) => {
      client.sftp((err, sftp) => {
        if (err) {
          reject(
            new DeployError(
              "UPLOAD_FAILED",
              `Failed to open SFTP channel: ${err.message}`,
              "The remote server may not support SFTP. Check SSH server configuration.",
            ),
          );
          return;
        }
        resolve(sftp);
      });
    });
  }

  /**
   * Create a directory on the remote server, including parent directories.
   * Expands `~` to the remote home directory.
   */
  private async mkdirRemote(sftp: SFTPWrapper, remotePath: string): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      // Use realpath to resolve the home directory
      sftp.realpath(".", (err, homePath) => {
        if (err) {
          reject(new DeployError("PERMISSION_DENIED", `Cannot resolve home directory: ${err.message}`));
          return;
        }

        const fullPath = remotePath.startsWith("~")
          ? remotePath.replace("~", homePath)
          : remotePath;

        // Create parent directories recursively
        const parts = fullPath.split("/").filter(Boolean);
        let current = fullPath.startsWith("/") ? "/" : "";

        const createNext = (index: number) => {
          if (index >= parts.length) {
            resolve();
            return;
          }

          current = current ? `${current}/${parts[index]}` : parts[index];

          sftp.mkdir(current, (mkdirErr) => {
            // Ignore errors when directory already exists
            if (mkdirErr) {
              // Try stat to check if it exists
              sftp.stat(current, (statErr) => {
                if (statErr) {
                  reject(
                    new DeployError(
                      "PERMISSION_DENIED",
                      `Failed to create directory "${current}": ${mkdirErr.message}`,
                      "Check that you have write permissions to the target directory.",
                    ),
                  );
                } else {
                  createNext(index + 1);
                }
              });
            } else {
              createNext(index + 1);
            }
          });
        };

        createNext(0);
      });
    });
  }

  /**
   * Upload a local file to the remote server via SFTP with progress tracking.
   * Emits progress events with transfer speed and ETA.
   */
  private async uploadWithProgress(
    sftp: SFTPWrapper,
    localPath: string,
    remotePath: string,
  ): Promise<void> {
    const stat = fs.statSync(localPath);
    const totalBytes = stat.size;

    // Resolve the remote path (expand ~)
    const resolvedRemotePath = await new Promise<string>((resolve, reject) => {
      sftp.realpath(".", (err, homePath) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(remotePath.startsWith("~") ? remotePath.replace("~", homePath) : remotePath);
      });
    });

    return new Promise<void>((resolve, reject) => {
      const readStream = fs.createReadStream(localPath);
      const writeStream = sftp.createWriteStream(resolvedRemotePath);

      let bytesTransferred = 0;
      const startTime = Date.now();

      readStream.on("data", (chunk: Buffer | string) => {
        const len = typeof chunk === "string" ? Buffer.byteLength(chunk) : chunk.length;
        bytesTransferred += len;
        const elapsed = (Date.now() - startTime) / 1000;
        const speed = elapsed > 0 ? bytesTransferred / elapsed : 0;
        const remaining = totalBytes - bytesTransferred;
        const eta = speed > 0 ? remaining / speed : 0;
        const percent = Math.round((bytesTransferred / totalBytes) * 100);

        this.emitProgress("uploading", percent, speed, eta);
      });

      writeStream.on("close", () => {
        resolve();
      });

      writeStream.on("error", (err: Error) => {
        readStream.destroy();
        reject(
          new DeployError(
            "UPLOAD_FAILED",
            `Upload failed: ${err.message}`,
            "Check network connectivity and remote disk space.",
          ),
        );
      });

      readStream.on("error", (err: Error) => {
        writeStream.destroy();
        reject(
          new DeployError(
            "UPLOAD_FAILED",
            `Failed to read local binary: ${err.message}`,
          ),
        );
      });

      readStream.pipe(writeStream);
    });
  }

  /**
   * Set file permissions on the remote server via SFTP.
   */
  private async chmodRemote(sftp: SFTPWrapper, remotePath: string, mode: number): Promise<void> {
    const resolvedPath = await new Promise<string>((resolve, reject) => {
      sftp.realpath(".", (err, homePath) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(remotePath.startsWith("~") ? remotePath.replace("~", homePath) : remotePath);
      });
    });

    return new Promise<void>((resolve, reject) => {
      sftp.chmod(resolvedPath, mode, (err) => {
        if (err) {
          reject(
            new DeployError(
              "PERMISSION_DENIED",
              `Failed to set executable permissions: ${err.message}`,
              "Try running: chmod 755 ~/.crab/bin/crab-desktop-agent",
            ),
          );
          return;
        }
        resolve();
      });
    });
  }

  /**
   * Check that the agent binary is not currently running on the remote.
   * If it is, throw a descriptive error with a suggestion to kill it.
   */
  private async checkBinaryNotRunning(client: Client): Promise<void> {
    try {
      const output = await this.execCommand(
        client,
        "pgrep -f crab-desktop-agent 2>/dev/null || true",
        { ignoreExitCode: true },
      );

      const pids = output.trim().split("\n").filter((l) => l.trim().length > 0);
      if (pids.length > 0) {
        throw new DeployError(
          "BINARY_RUNNING",
          `The agent binary is already running on the remote server (PID: ${pids[0]}).`,
          "Stop the running agent before deploying a new version. " +
          `You can kill it with: kill ${pids[0]}`,
        );
      }
    } catch (err) {
      if (err instanceof DeployError) throw err;
      // pgrep not available or other error — proceed with deployment
    }
  }

  /**
   * Check that the remote server has enough disk space for the upload.
   * Requires at least 2x the binary size to account for the upload
   * temporary file.
   */
  private async checkDiskSpace(client: Client, localPath: string): Promise<void> {
    const stat = fs.statSync(localPath);
    const requiredBytes = stat.size * 2;

    try {
      const output = await this.execCommand(
        client,
        "df -k ~ 2>/dev/null | tail -1",
        { ignoreExitCode: true },
      );

      const parts = output.trim().split(/\s+/);
      // df -k output: Filesystem 1K-blocks Used Available Use% Mounted
      if (parts.length >= 4) {
        const availableKb = parseInt(parts[3], 10);
        if (!isNaN(availableKb) && availableKb * 1024 < requiredBytes) {
          throw new DeployError(
            "DISK_FULL",
            `Insufficient disk space on remote server. Need ${Math.ceil(requiredBytes / 1024 / 1024)}MB, ` +
            `available: ${Math.ceil(availableKb / 1024)}MB.`,
            "Free up disk space on the remote server before deploying the agent.",
          );
        }
      }
    } catch (err) {
      if (err instanceof DeployError) throw err;
      // df not available or parse error — proceed with deployment
    }
  }

  /**
   * Send a JSON-RPC ping to the remote agent and verify the response.
   * Uses NDJSON framing (newline-delimited JSON).
   */
  private verifyAgentPing(channel: ClientChannel): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => {
        reject(
          new DeployError(
            "START_FAILED",
            "Remote agent did not respond to ping within 10 seconds.",
            "The agent may have crashed on startup. Check remote logs at ~/.crab/logs/.",
          ),
        );
      }, 10000);

      let buffer = "";

      const onData = (data: Buffer) => {
        buffer += data.toString();
        const lines = buffer.split("\n");

        for (let i = 0; i < lines.length - 1; i++) {
          const line = lines[i].trim();
          if (!line) continue;

          try {
            const response = JSON.parse(line);
            if (response.id === 1 && response.result) {
              clearTimeout(timeout);
              channel.removeListener("data", onData);
              resolve();
              return;
            }
            if (response.id === 1 && response.error) {
              clearTimeout(timeout);
              channel.removeListener("data", onData);
              reject(
                new DeployError(
                  "START_FAILED",
                  `Agent ping failed: ${response.error.message ?? "unknown error"}`,
                ),
              );
              return;
            }
          } catch {
            // Not valid JSON yet — continue buffering
          }
        }

        // Keep the last incomplete line in the buffer
        buffer = lines[lines.length - 1];
      };

      channel.on("data", onData);

      channel.on("close", () => {
        clearTimeout(timeout);
        channel.removeListener("data", onData);
        reject(
          new DeployError(
            "START_FAILED",
            "Remote agent channel closed unexpectedly before responding to ping.",
            "The agent binary may be incompatible with this server. Check architecture.",
          ),
        );
      });

      // Send the ping RPC request (NDJSON format)
      const pingRequest = JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "ping",
        params: {},
      });
      channel.write(pingRequest + "\n");
    });
  }

  /**
   * Parse a semver string into [major, minor, patch] numbers.
   */
  private parseSemver(version: string): [number, number, number] {
    const parts = version.split(".").map((p) => parseInt(p, 10));
    return [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0];
  }

  /**
   * Get the Electron resources path. In a packaged app this is
   * `process.resourcesPath`; in development it falls back to the
   * project root's resources directory.
   */
  private getResourcesPath(): string {
    // process.resourcesPath is set by Electron in packaged apps
    if (typeof process !== "undefined" && (process as NodeJS.Process & { resourcesPath?: string }).resourcesPath) {
      return (process as NodeJS.Process & { resourcesPath?: string }).resourcesPath!;
    }

    // Development fallback
    return path.join(__dirname, "..", "resources");
  }
}
