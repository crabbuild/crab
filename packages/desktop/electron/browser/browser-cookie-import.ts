/* eslint-disable max-lines -- Why: cookie import is a single pipeline that must stay coherent. */
import { type BrowserWindow,dialog,session } from "electron";
import { existsSync,readdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";

// ─── Types ──────────────────────────────────────────────────────────────────

export type BrowserProfile = {
  name: string;
  directory: string;
};

export type DetectedBrowser = {
  family: string | null;
  label: string;
  cookiesPath: string;
  keychainService?: string;
  keychainAccount?: string;
  profiles: BrowserProfile[];
  selectedProfile: string;
};

type BrowserCookieImportSummary = {
  totalCookies: number;
  imported: number;
  skipped: number;
};

export type BrowserCookieImportResult =
  | { ok: true; profileId?: string; summary: BrowserCookieImportSummary }
  | { ok: false; reason: string; profileId?: string };

type RawCookieEntry = Record<string, unknown>;

type ValidatedCookie = {
  url: string;
  name: string;
  value: string;
  domain: string | null;
  path: string;
  secure: boolean;
  httpOnly: boolean;
  sameSite: "unspecified" | "no_restriction" | "lax" | "strict";
  expirationDate: number | null;
};

// ─── Validation ─────────────────────────────────────────────────────────────

function validateCookieEntry(entry: RawCookieEntry): ValidatedCookie | null {
  const name = typeof entry.name === "string" ? entry.name.trim() : "";
  const value = typeof entry.value === "string" ? entry.value : "";
  const domain = typeof entry.domain === "string" ? entry.domain : null;
  const path = typeof entry.path === "string" ? entry.path : "/";
  const secure = entry.secure === true;
  const httpOnly = entry.httpOnly === true;
  if (!name) return null;

  // Build URL or use domain
  let url: string;
  if (typeof entry.url === "string" && entry.url.trim()) {
    url = entry.url.trim();
    if (!url.startsWith("http://") && !url.startsWith("https://")) {
      url = (secure ? "https://" : "http://") + url;
    }
  } else if (domain) {
    const proto = secure ? "https://" : "http://";
    url = proto + (domain.startsWith(".") ? domain.slice(1) : domain) + path;
  } else {
    return null;
  }

  let sameSite: ValidatedCookie["sameSite"] = "unspecified";
  if (typeof entry.sameSite === "string") {
    const s = entry.sameSite.toLowerCase();
    if (s === "no_restriction" || s === "lax" || s === "strict") {
      sameSite = s;
    }
  }

  const expirationDate =
    typeof entry.expirationDate === "number" && Number.isFinite(entry.expirationDate)
      ? entry.expirationDate
      : null;

  return { url, name, value, domain, path, secure, httpOnly, sameSite, expirationDate };
}

// ─── Cookie Import Pipeline ────────────────────────────────────────────────

function isSafeBrowserProfileDirectory(dir: string): boolean {
  return !dir.includes("..") && !dir.includes("/") && !dir.includes("\\");
}

async function importValidatedCookies(
  validated: ValidatedCookie[],
  totalEntries: number,
  targetPartition: string
): Promise<BrowserCookieImportResult> {
  const sess = session.fromPartition(targetPartition);
  let imported = 0;
  let skippedEntries = 0;

  for (const cookie of validated) {
    try {
      const details: Electron.CookiesSetDetails = {
        url: cookie.url,
        name: cookie.name,
        value: cookie.value,
        path: cookie.path,
        secure: cookie.secure,
        httpOnly: cookie.httpOnly,
        sameSite: cookie.sameSite,
      };
      if (cookie.domain) details.domain = cookie.domain;
      if (cookie.expirationDate != null) details.expirationDate = cookie.expirationDate;

      await sess.cookies.set(details);
      imported++;
    } catch {
      skippedEntries++;
    }
  }

  const skipped = totalEntries - validated.length + skippedEntries;
  return { ok: true, summary: { totalCookies: totalEntries, imported, skipped } };
}

// ─── Browser Detection ─────────────────────────────────────────────────────

function browserRootPath(macSubPath: string): string | null {
  if (process.platform === "darwin") {
    const p = join(homedir(), "Library", "Application Support", macSubPath);
    return existsSync(p) ? p : null;
  }
  if (process.platform === "win32") {
    const local = process.env.LOCALAPPDATA;
    if (local) {
      const p = join(local, macSubPath);
      return existsSync(p) ? p : null;
    }
  }
  if (process.platform === "linux") {
    const config = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
    const p = join(config, macSubPath);
    return existsSync(p) ? p : null;
  }
  return null;
}

function chromiumCookiesPath(profileDir: string): string | null {
  const candidates = [
    join(profileDir, "Cookies"),           // Chrome < 131 (SQLite)
    join(profileDir, "Network", "Cookies"), // Chrome >= 131
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  return null;
}

function discoverChromiumProfiles(root: string): BrowserProfile[] {
  const profiles: BrowserProfile[] = [];
  try {
    const entries = readdirSync(root, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const dirName = entry.name;
      const name = dirName.startsWith("Profile ") ? dirName : "";
      profiles.push({ name: name || "Default", directory: dirName });
    }
  } catch {
    // best-effort
  }
  if (!profiles.some((p) => p.directory === "Default")) {
    profiles.unshift({ name: "Default", directory: "Default" });
  }
  return profiles;
}

const CHROMIUM_BROWSER_DEFS: Array<{
  family: string;
  label: string;
  macSubPath: string;
  linuxSubPath: string;
  winSubPath: string;
}> = [
  { family: "chrome", label: "Google Chrome", macSubPath: "Google/Chrome", linuxSubPath: "google-chrome", winSubPath: "Google/Chrome/User Data" },
  { family: "chromium", label: "Chromium", macSubPath: "Chromium", linuxSubPath: "chromium", winSubPath: "Chromium/User Data" },
  { family: "brave", label: "Brave", macSubPath: "BraveSoftware/Brave-Browser", linuxSubPath: "BraveSoftware/Brave-Browser", winSubPath: "BraveSoftware/Brave-Browser/User Data" },
  { family: "edge", label: "Microsoft Edge", macSubPath: "Microsoft Edge", linuxSubPath: "microsoft-edge", winSubPath: "Microsoft/Edge/User Data" },
  { family: "opera", label: "Opera", macSubPath: "com.operasoftware.Opera", linuxSubPath: "opera", winSubPath: "Opera Software/Opera Stable" },
  { family: "vivaldi", label: "Vivaldi", macSubPath: "Vivaldi", linuxSubPath: "vivaldi", winSubPath: "Vivaldi/User Data" },
];

function detectFirefox(): DetectedBrowser | null {
  const possibleRoots: string[] = [];
  if (process.platform === "darwin") {
    possibleRoots.push(join(homedir(), "Library", "Application Support", "Firefox"));
  } else if (process.platform === "linux") {
    possibleRoots.push(join(homedir(), ".mozilla", "firefox"));
  } else if (process.platform === "win32") {
    const appData = process.env.APPDATA;
    if (appData) possibleRoots.push(join(appData, "Mozilla", "Firefox"));
  }

  for (const root of possibleRoots) {
    if (!existsSync(root)) continue;
    const profilesIni = join(root, "profiles.ini");
    if (!existsSync(profilesIni)) continue;
    const profilesDir = join(root, "Profiles");
    if (!existsSync(profilesDir)) continue;

    try {
      const entries = readdirSync(profilesDir, { withFileTypes: true });
      const profiles: BrowserProfile[] = [];
      let selected = "";

      for (const entry of entries) {
        if (!entry.isDirectory()) continue;
        const cookiesPath = join(profilesDir, entry.name, "cookies.sqlite");
        if (!existsSync(cookiesPath)) continue;
        profiles.push({ name: entry.name, directory: entry.name });
        if (!selected) {
          selected = entry.name;
          return {
            family: "firefox",
            label: "Firefox",
            cookiesPath,
            profiles,
            selectedProfile: selected,
          };
        }
      }
    } catch {
      // best-effort
    }
  }
  return null;
}

function detectSafari(): DetectedBrowser | null {
  if (process.platform !== "darwin") return null;
  const dbPath = join(homedir(), "Library", "Cookies", "Cookies.binarycookies");
  if (!existsSync(dbPath)) return null;
  return {
    family: "safari",
    label: "Safari",
    cookiesPath: dbPath,
    profiles: [{ name: "Default", directory: "Default" }],
    selectedProfile: "Default",
  };
}

export function detectInstalledBrowsers(): DetectedBrowser[] {
  const detected: DetectedBrowser[] = [];

  for (const def of CHROMIUM_BROWSER_DEFS) {
    let root: string | null = null;
    if (process.platform === "darwin") {
      root = browserRootPath(def.macSubPath);
    } else if (process.platform === "linux") {
      root = browserRootPath(def.linuxSubPath);
    } else if (process.platform === "win32") {
      root = browserRootPath(def.winSubPath);
    }
    if (!root) continue;

    const profiles = discoverChromiumProfiles(root);
    for (const profile of profiles) {
      const profileDir = join(root, profile.directory);
      const cookiesPath = chromiumCookiesPath(profileDir);
      if (cookiesPath) {
        detected.push({
          family: def.family,
          label: def.label,
          cookiesPath,
          profiles,
          selectedProfile: profile.directory,
        });
        break;
      }
    }
  }

  const firefox = detectFirefox();
  if (firefox) detected.push(firefox);

  const safari = detectSafari();
  if (safari) detected.push(safari);

  return detected;
}

export function selectBrowserProfile(
  browser: DetectedBrowser,
  profileDirectory: string
): DetectedBrowser | null {
  if (!isSafeBrowserProfileDirectory(profileDirectory)) return null;

  if (browser.family === "firefox") {
    let profilesRoot: string | null = null;
    if (process.platform === "darwin") {
      profilesRoot = join(homedir(), "Library", "Application Support", "Firefox", "Profiles");
    } else if (process.platform === "linux") {
      profilesRoot = join(homedir(), ".mozilla", "firefox", "Profiles");
    } else if (process.platform === "win32") {
      const appData = process.env.APPDATA;
      if (appData) profilesRoot = join(appData, "Mozilla", "Firefox", "Profiles");
    }
    if (!profilesRoot) return null;
    const cookiesPath = join(profilesRoot, profileDirectory, "cookies.sqlite");
    if (!existsSync(cookiesPath)) return null;
    return { ...browser, cookiesPath, selectedProfile: profileDirectory };
  }

  // Chromium-based: find the browser def and resolve profile directory
  const def = CHROMIUM_BROWSER_DEFS.find((d) => d.family === browser.family);
  if (!def) return null;
  let root: string | null = null;
  if (process.platform === "darwin") {
    root = browserRootPath(def.macSubPath);
  } else if (process.platform === "linux") {
    root = browserRootPath(def.linuxSubPath);
  } else if (process.platform === "win32") {
    root = browserRootPath(def.winSubPath);
  }
  if (!root) return null;

  const profileDir = join(root, profileDirectory);
  const cookiesPath = chromiumCookiesPath(profileDir);
  if (!cookiesPath) return null;
  return { ...browser, cookiesPath, selectedProfile: profileDirectory };
}

// ─── File Dialog ───────────────────────────────────────────────────────────

export async function pickCookieFile(parentWindow: BrowserWindow | null): Promise<string | null> {
  const opts = {
    title: "Import Cookies",
    filters: [
      { name: "Cookie Files", extensions: ["json", "txt"] },
      { name: "All Files", extensions: ["*"] },
    ],
    properties: ["openFile" as const],
  };
  const result = parentWindow
    ? await dialog.showOpenDialog(parentWindow, opts)
    : await dialog.showOpenDialog(opts);

  if (result.canceled || result.filePaths.length === 0) return null;
  return result.filePaths[0];
}

// ─── Import from File ──────────────────────────────────────────────────────

export async function importCookiesFromFile(
  filePath: string,
  targetPartition: string
): Promise<BrowserCookieImportResult> {
  let rawContent: string;
  try {
    rawContent = await readFile(filePath, "utf-8");
  } catch {
    return { ok: false, reason: "Could not read the selected file." };
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(rawContent);
  } catch {
    return { ok: false, reason: "File is not valid JSON. Expected a JSON array of cookie objects." };
  }

  if (!Array.isArray(parsed)) {
    return { ok: false, reason: "Expected a JSON array of cookie objects." };
  }

  if (parsed.length === 0) {
    return { ok: false, reason: "Cookie file is empty." };
  }

  const validated: ValidatedCookie[] = [];
  let skipped = 0;
  for (const entry of parsed) {
    if (typeof entry !== "object" || entry === null) {
      skipped++;
      continue;
    }
    const cookie = validateCookieEntry(entry as RawCookieEntry);
    if (cookie) validated.push(cookie);
    else skipped++;
  }

  if (validated.length === 0) {
    return {
      ok: false,
      reason: `No valid cookies found. ${skipped} entries were skipped due to missing or invalid fields.`,
    };
  }

  return importValidatedCookies(validated, parsed.length, targetPartition);
}

// ─── Import from Browser ───────────────────────────────────────────────────

export function getUserAgentForBrowser(family: string | null): string | null {
  if (process.platform !== "darwin") return null;

  const platform = "Macintosh; Intel Mac OS X 10_15_7";
  const chromeBase = "AppleWebKit/537.36 (KHTML, like Gecko)";

  function readBrowserVersion(appPath: string): string | null {
    try {
      return (
        execFileSync("defaults", ["read", `${appPath}/Contents/Info`, "CFBundleShortVersionString"], {
          encoding: "utf-8",
          timeout: 5_000,
        }).trim() || null
      );
    } catch {
      return null;
    }
  }

  if (family === "chrome") {
    const ver = readBrowserVersion("/Applications/Google Chrome.app");
    if (ver)
      return `Mozilla/5.0 (${platform}) ${chromeBase} Chrome/${ver} Safari/${537.36}`;
  }
  if (family === "brave") {
    const ver = readBrowserVersion("/Applications/Brave Browser.app");
    if (ver)
      return `Mozilla/5.0 (${platform}) ${chromeBase} Chrome/${ver} Safari/${537.36}`;
  }
  if (family === "edge") {
    const ver = readBrowserVersion("/Applications/Microsoft Edge.app");
    if (ver)
      return `Mozilla/5.0 (${platform}) ${chromeBase} Chrome/${ver} Safari/${537.36}`;
  }
  if (family === "vivaldi") {
    const ver = readBrowserVersion("/Applications/Vivaldi.app");
    if (ver)
      return `Mozilla/5.0 (${platform}) ${chromeBase} Chrome/${ver} Safari/${537.36}`;
  }
  if (family === "opera") {
    const ver = readBrowserVersion("/Applications/Opera.app");
    if (ver)
      return `Mozilla/5.0 (${platform}) ${chromeBase} Chrome/${ver} Safari/${537.36}`;
  }
  if (family === "firefox") {
    const ver = readBrowserVersion("/Applications/Firefox.app");
    if (ver) return `Mozilla/5.0 (${platform}; rv:${ver}) Gecko/20100101 Firefox/${ver}`;
  }
  if (family === "safari") {
    const ver = readBrowserVersion("/Applications/Safari.app");
    if (ver) return `Mozilla/5.0 (${platform}) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/${ver} Safari/604.1`;
  }

  return null;
}

export async function importCookiesFromBrowser(
  browser: DetectedBrowser,
  targetPartition: string
): Promise<BrowserCookieImportResult> {
  if (!existsSync(browser.cookiesPath)) {
    return { ok: false, reason: `Cookie database not found at ${browser.cookiesPath}.` };
  }

  // For Chromium-based browsers, read from the Cookies SQLite database
  if (browser.family && browser.family !== "firefox" && browser.family !== "safari") {
    try {
      const { DatabaseSync } = require("node:sqlite") as any;
      const db = new DatabaseSync(browser.cookiesPath);
      try {
        const stmt = db.prepare(
          "SELECT host_key, name, value, path, secure, httponly, has_expires, expires_utc, is_persistent, priority FROM cookies"
        );
        const rows = stmt.all() as Array<{
          host_key: string;
          name: string;
          value: string;
          path: string;
          secure: number;
          httponly: number;
          has_expires: number;
          expires_utc: number;
          is_persistent: number;
          priority: number;
        }>;

        const validated: ValidatedCookie[] = [];
        for (const row of rows) {
          if (!row.name || !row.host_key) continue;
          const domain = row.host_key.startsWith(".") ? row.host_key : row.host_key;
          const proto = row.secure ? "https://" : "http://";
          const url = proto + (domain.startsWith(".") ? domain.slice(1) : domain) + (row.path || "/");
          // Chrome stores expiration in microseconds since 1601-01-01
          const expirationDate = row.has_expires
            ? (row.expires_utc / 1_000_000) - 11644473600
            : null;

          validated.push({
            url,
            name: row.name,
            value: row.value,
            domain,
            path: row.path || "/",
            secure: !!row.secure,
            httpOnly: !!row.httponly,
            sameSite: "unspecified",
            expirationDate,
          });
        }

        return importValidatedCookies(validated, rows.length, targetPartition);
      } finally {
        db.close();
      }
    } catch (err) {
      return {
        ok: false,
        reason: `Could not read Chromium cookie database: ${err instanceof Error ? err.message : String(err)}`,
      };
    }
  }

  // For Firefox, read from cookies.sqlite
  if (browser.family === "firefox") {
    try {
      const { DatabaseSync } = require("node:sqlite") as any;
      const db = new DatabaseSync(browser.cookiesPath);
      try {
        const stmt = db.prepare(
          "SELECT host, name, value, path, isSecure, isHttpOnly, expiry FROM moz_cookies"
        );
        const rows = stmt.all() as Array<{
          host: string;
          name: string;
          value: string;
          path: string;
          isSecure: number;
          isHttpOnly: number;
          expiry: number;
        }>;

        const validated: ValidatedCookie[] = [];
        for (const row of rows) {
          if (!row.name || !row.host) continue;
          const domain = row.host;
          const proto = row.isSecure ? "https://" : "http://";
          const url = proto + (domain.startsWith(".") ? domain.slice(1) : domain) + (row.path || "/");

          validated.push({
            url,
            name: row.name,
            value: row.value,
            domain,
            path: row.path || "/",
            secure: !!row.isSecure,
            httpOnly: !!row.isHttpOnly,
            sameSite: "unspecified",
            expirationDate: row.expiry || null,
          });
        }

        return importValidatedCookies(validated, rows.length, targetPartition);
      } finally {
        db.close();
      }
    } catch (err) {
      return {
        ok: false,
        reason: `Could not read Firefox cookie database: ${err instanceof Error ? err.message : String(err)}`,
      };
    }
  }

  // Safari uses binary cookies format - best-effort
  if (browser.family === "safari") {
    return { ok: false, reason: "Safari cookie import is not supported on this platform. Use the file-based import instead." };
  }

  return { ok: false, reason: "Unknown browser type." };
}
