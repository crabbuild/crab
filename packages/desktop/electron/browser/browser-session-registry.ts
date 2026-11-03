import { app,session } from "electron";
import type { Session } from "electron";
import { randomUUID } from "node:crypto";
import { existsSync,readFileSync,renameSync,writeFileSync } from "node:fs";
import { join } from "node:path";

export const CRAB_BROWSER_PARTITION = "persist:crab-browser";

// ─── Types ──────────────────────────────────────────────────────────────────

type BrowserSessionSource = {
  browserFamily?: string | null;
  profileName?: string | null;
  importedAt?: number | null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  [key: string]: any;
};

export type BrowserSessionProfileScope = "default" | "worktree" | "global";

export type BrowserSessionProfile = {
  id: string;
  scope: BrowserSessionProfileScope;
  partition: string;
  label: string;
  source: BrowserSessionSource;
};

// ─── Session Registry ───────────────────────────────────────────────────────

class BrowserSessionRegistry {
  private readonly profiles = new Map<string, BrowserSessionProfile>();

  constructor() {
    const persisted = this.loadPersistedSource();
    this.profiles.set("default", {
      id: "default",
      scope: "default",
      partition: CRAB_BROWSER_PARTITION,
      label: "Default",
      source: persisted,
    });

    // Restore persisted profiles from prior sessions.
    for (const profile of this.loadProfiles()) {
      this.profiles.set(profile.id, profile);
    }
  }

  private get metadataPath(): string {
    return join(app.getPath("userData"), "crab-browser-session-meta.json");
  }

  private loadPersistedSource(): BrowserSessionSource {
    try {
      const raw = readFileSync(this.metadataPath, "utf-8");
      const data = JSON.parse(raw);
      return data?.defaultSource ?? {};
    } catch {
      return {};
    }
  }

  private persistMeta(data: Record<string, unknown>): void {
    try {
      const tmpPath = `${this.metadataPath}.tmp`;
      writeFileSync(tmpPath, JSON.stringify(data));
      renameSync(tmpPath, this.metadataPath);
    } catch {
      // best-effort
    }
  }

  private persistProfiles(): void {
    const nonDefault = [...this.profiles.values()].filter((p) => p.id !== "default");
    this.persistMeta({
      defaultSource: this.profiles.get("default")?.source ?? {},
      profiles: nonDefault,
    });
  }

  private loadProfiles(): BrowserSessionProfile[] {
    try {
      const raw = readFileSync(this.metadataPath, "utf-8");
      const data = JSON.parse(raw);
      return Array.isArray(data?.profiles) ? data.profiles : [];
    } catch {
      return [];
    }
  }

  // ── Public API ──────────────────────────────────────────────────────────

  listProfiles(): BrowserSessionProfile[] {
    return [...this.profiles.values()];
  }

  getProfile(profileId: string): BrowserSessionProfile | null {
    return this.profiles.get(profileId) ?? null;
  }

  resolvePartition(profileId: string | null): string | null {
    if (!profileId) return CRAB_BROWSER_PARTITION;
    const profile = this.profiles.get(profileId);
    return profile?.partition ?? null;
  }

  createProfile(scope: BrowserSessionProfileScope, label: string): BrowserSessionProfile | null {
    if (!label?.trim()) return null;

    const id = `profile-${randomUUID().slice(0, 8)}`;
    const partition = `persist:crab-browser-${id}`;

    const profile: BrowserSessionProfile = {
      id,
      scope,
      partition,
      label: label.trim(),
      source: {},
    };

    this.profiles.set(id, profile);
    this.persistProfiles();
    return profile;
  }

  async deleteProfile(profileId: string): Promise<boolean> {
    if (profileId === "default") return false;
    const profile = this.profiles.get(profileId);
    if (!profile) return false;

    // Clean up the session partition's data
    try {
      const partition = profile.partition.replace("persist:", "");
      const partitionPath = join(app.getPath("userData"), "Partitions", partition);
      if (existsSync(partitionPath)) {
        const { rmSync } = await import("node:fs");
        rmSync(partitionPath, { recursive: true, force: true });
      }
    } catch {
      // best-effort
    }

    this.profiles.delete(profileId);
    this.persistProfiles();
    return true;
  }

  updateProfileSource(profileId: string, source: BrowserSessionSource): void {
    const profile = this.profiles.get(profileId);
    if (!profile) return;
    profile.source = { ...profile.source, ...source };
    this.persistProfiles();
  }

  /** Set up session policies for a custom partition (permissions, UA, etc.) */

  /** Clear all cookies from the default browser session partition. */
  async clearDefaultSessionCookies(): Promise<boolean> {
    try {
      const sess = session.fromPartition(CRAB_BROWSER_PARTITION);
      const cookies = await sess.cookies.get({});
      for (const cookie of cookies) {
        await sess.cookies.remove(cookie.domain || "", cookie.name);
      }
      return true;
    } catch {
      return false;
    }
  }

  setupSessionPolicies(partition: string): Session {
    const sess = session.fromPartition(partition);

    // Permission handler
    const autoGranted = new Set(["fullscreen", "clipboard-read", "clipboard-sanitized-write"]);
    sess.setPermissionRequestHandler((_wc, permission, callback) => {
      callback(autoGranted.has(permission));
    });

    // Certificate errors — deny by default
    (sess as any).on("certificate-error", (event: any, _wc: any, _url: any, _error: any, _certificate: any, callback: any) => {
      event.preventDefault();
      callback(false);
    });

    return sess;
  }
}

export const browserSessionRegistry = new BrowserSessionRegistry();
