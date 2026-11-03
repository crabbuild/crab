import { useEffect, useState } from "react";
import { readWorkspaceDatasetRules } from "../ipc";
import { DatasetRule, DatasetRulesFile, defaultRules, mergeRules } from "./dataset-kind";

/**
 * Resolves the effective dataset classification rules for a repo.
 *
 * Attempts to load workspace-level overrides from
 * `.crab/desktop/dataset-rules.json`. If the file exists and
 * parses correctly, the rules are merged with (or replace) the
 * shipped defaults. Falls back to defaults silently on any error.
 */
export function useDatasetRules(repoId: string): DatasetRule[] {
  const [rules, setRules] = useState<DatasetRule[]>(defaultRules.rules);

  useEffect(() => {
    let cancelled = false;

    readWorkspaceDatasetRules(repoId)
      .then((overrides) => {
        if (cancelled) return;
        if (overrides) {
          const merged = mergeRules(defaultRules.rules, overrides as DatasetRulesFile);
          setRules(merged);
        } else {
          setRules(defaultRules.rules);
        }
      })
      .catch(() => {
        if (!cancelled) setRules(defaultRules.rules);
      });

    return () => {
      cancelled = true;
    };
  }, [repoId]);

  return rules;
}
