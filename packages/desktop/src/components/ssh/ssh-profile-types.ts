/**
 * Shared SSH profile type for the renderer.
 *
 * Extracted from SshView.tsx to break the circular dependency:
 *   SshView → ConnectionCard/ConnectionForm → SshView
 */

// Minimal SshProfile type for the renderer (mirrors electron/ssh-manager.ts)
export interface SshProfile {
  id: string;
  label: string;
  host: string;
  port: number;
  username: string;
  authMethod: { type: "agent" } | { type: "key"; keyPath: string } | { type: "key_passphrase"; keyPath: string } | { type: "password" };
  remotePath?: string;
  jumpHost?: string;
  forwardAgent: boolean;
  compression: boolean;
  keepAliveInterval: number;
  autoReconnect: boolean;
  autoDeployAgent: boolean;
  colour?: string;
  tags: string[];
  portForwards: Array<{
    id: string;
    direction: "local" | "remote";
    localPort: number;
    actualLocalPort?: number;
    remoteHost: string;
    remotePort: number;
    label?: string;
    autoStart: boolean;
    status: "active" | "stopped" | "error";
  }>;
  createdAt: number;
  lastConnectedAt?: number;
  pinned?: boolean;
  connectionTimeoutMs?: number;
  operationTimeoutMs?: number;
}
