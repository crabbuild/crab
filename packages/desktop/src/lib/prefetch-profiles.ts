// Named prefetch-pattern presets persisted in localStorage. Each
// profile is just `{ name, patterns[] }`. One-click Hydrate runs
// `crab hydrate` with the pattern list.
//
// Not tied to the Settings object because profiles evolve
// independently of typed settings — they're pure user data.

import { useEffect, useState } from "react";

export interface PrefetchProfile {
  id: string;
  name: string;
  patterns: string[];
  createdAt: number;
}

const LS = "crab.prefetchProfiles.v1";

function load(): PrefetchProfile[] {
  try {
    const raw = localStorage.getItem(LS);
    if (!raw) return [];
    const arr = JSON.parse(raw) as PrefetchProfile[];
    return Array.isArray(arr) ? arr : [];
  } catch {
    return [];
  }
}

function save(profiles: PrefetchProfile[]) {
  try {
    localStorage.setItem(LS, JSON.stringify(profiles));
  } catch {
    // ignore
  }
}

export function usePrefetchProfiles() {
  const [profiles, setProfiles] = useState<PrefetchProfile[]>(() => load());

  useEffect(() => {
    save(profiles);
  }, [profiles]);

  const add = (name: string, patterns: string[]) => {
    const profile: PrefetchProfile = {
      id: `pf-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      name,
      patterns,
      createdAt: Date.now(),
    };
    setProfiles((prev) => [...prev, profile]);
    return profile;
  };

  const remove = (id: string) =>
    setProfiles((prev) => prev.filter((p) => p.id !== id));

  const update = (id: string, patch: Partial<Omit<PrefetchProfile, "id">>) =>
    setProfiles((prev) =>
      prev.map((p) => (p.id === id ? { ...p, ...patch } : p)),
    );

  return { profiles, add, remove, update };
}
