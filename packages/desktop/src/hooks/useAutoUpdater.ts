import { useEffect, useState, useCallback } from "react";

export type UpdateStatus =
  | "idle"
  | "checking"
  | "available"
  | "not-available"
  | "downloading"
  | "downloaded"
  | "error";

export interface UpdateProgress {
  percent: number;
  bytesPerSecond: number;
  transferred: number;
  total: number;
}

export interface UpdateInfo {
  version: string;
  releaseDate?: string;
  releaseNotes?: unknown;
}

export interface AutoUpdaterState {
  status: UpdateStatus;
  info: UpdateInfo | null;
  progress: UpdateProgress | null;
  error: string | null;
  checkForUpdate: () => void;
  downloadUpdate: () => void;
  quitAndInstall: () => void;
}

/**
 * React hook that subscribes to auto-update events from the main process
 * and exposes a simple state machine for the UI.
 */
export function useAutoUpdater(): AutoUpdaterState {
  const [status, setStatus] = useState<UpdateStatus>("idle");
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const unsubs: Array<() => void> = [];

    unsubs.push(
      window.crab.updater.onChecking(() => {
        setStatus("checking");
        setError(null);
      }),
    );

    unsubs.push(
      window.crab.updater.onUpdateAvailable((updateInfo) => {
        setStatus("available");
        setInfo(updateInfo);
      }),
    );

    unsubs.push(
      window.crab.updater.onUpdateNotAvailable(() => {
        setStatus("not-available");
      }),
    );

    unsubs.push(
      window.crab.updater.onProgress((p) => {
        setStatus("downloading");
        setProgress(p);
      }),
    );

    unsubs.push(
      window.crab.updater.onUpdateDownloaded((updateInfo) => {
        setStatus("downloaded");
        setInfo(updateInfo);
        setProgress(null);
      }),
    );

    unsubs.push(
      window.crab.updater.onError((err) => {
        setStatus("error");
        setError(err.message);
      }),
    );

    return () => {
      for (const unsub of unsubs) unsub();
    };
  }, []);

  const checkForUpdate = useCallback(() => {
    window.crab.updater.checkForUpdate();
  }, []);

  const downloadUpdate = useCallback(() => {
    window.crab.updater.downloadUpdate();
  }, []);

  const quitAndInstall = useCallback(() => {
    window.crab.updater.quitAndInstall();
  }, []);

  return { status, info, progress, error, checkForUpdate, downloadUpdate, quitAndInstall };
}
