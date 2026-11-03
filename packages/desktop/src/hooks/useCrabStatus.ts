import { useCallback, useEffect, useState } from "react";
import { rpc, StorageConfig } from "../ipc";

/**
 * Initialization status for crab large-file management in a repo.
 *
 * - "loading": checking status
 * - "uninitialized": no v2 crab.toml and object-store remote found
 * - "no-storage": initialized but no storage provider configured
 * - "no-credentials": storage configured but credentials missing
 * - "ready": initialized locally; connectivity is verified separately
 * - "error": failed to check status
 */
export type CrabStatus =
  | "loading"
  | "uninitialized"
  | "no-storage"
  | "no-credentials"
  | "ready"
  | "error";

export interface CrabStatusResult {
  status: CrabStatus;
  config: StorageConfig | null;
  error: string | null;
  refresh: () => void;
}

/**
 * Hook that checks whether a repository is fully configured for crab
 * large-file management (initialized, storage configured, credentials
 * available).
 */
export function useCrabStatus(repoId: string | undefined): CrabStatusResult {
  const [status, setStatus] = useState<CrabStatus>("loading");
  const [config, setConfig] = useState<StorageConfig | null>(null);
  const [error, setError] = useState<string | null>(null);

  const check = useCallback(async () => {
    if (!repoId) {
      setStatus("error");
      setError("No repository selected");
      return;
    }

    setStatus("loading");
    setError(null);

    try {
      const storageConfig = await rpc.storageConfigGet(repoId);
      setConfig(storageConfig);

      if (!storageConfig.configured) {
        setStatus("uninitialized");
      } else if (!storageConfig.bucket) {
        setStatus("no-storage");
      } else if (!storageConfig.has_credentials) {
        // Credentials might come from env/IAM — treat as ready
        // but with a warning. Only flag no-credentials when the
        // provider clearly needs explicit credentials and none
        // are found anywhere.
        setStatus("ready");
      } else {
        setStatus("ready");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setStatus("error");
    }
  }, [repoId]);

  useEffect(() => {
    check();
  }, [check]);

  useEffect(() => {
    const changed = (event: Event) => {
      const target = (event as CustomEvent<{ repo_id?: string }>).detail?.repo_id;
      if (!target || target === repoId) void check();
    };
    window.addEventListener("crab:storage-connection-changed", changed);
    return () => window.removeEventListener("crab:storage-connection-changed", changed);
  }, [check, repoId]);

  return { status, config, error, refresh: check };
}
