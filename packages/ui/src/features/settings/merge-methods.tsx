import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { mergeMethods } from "@/features/pull-requests/merge-methods";
import type { MergeMethod } from "@/features/pull-requests/model";

interface MergeSettings {
  version: number;
  enabled: MergeMethod[];
  auto_merge?: boolean;
  delete_head_branches?: boolean;
  merge_queue?: boolean;
  merge_queue_method?: MergeMethod;
  merge_queue_builds?: number;
  merge_queue_timeout_minutes?: number;
  merge_queue_branches?: BranchQueue[];
}

interface BranchQueue {
  branch: string;
  method?: MergeMethod;
  builds?: number;
  timeout_minutes?: number;
}

interface OverrideDraft {
  id: string;
  branch: string;
  method: MergeMethod | "";
  builds: string;
  timeout: string;
}

const queueMethods = mergeMethods.filter(
  ({ value }) => value !== "fast_forward",
);
const allowedNumber = (value: number, max: number) =>
  Number.isInteger(value) && value >= 1 && value <= max;

function QueueMethodSelect({
  value,
  onChange,
  inherited,
}: {
  value: MergeMethod | "";
  onChange: (method: MergeMethod | "") => void;
  inherited?: boolean;
}) {
  return (
    <NativeSelect
      value={value}
      onChange={({ target }) => {
        const selected = queueMethods.find(
          ({ value }) => value === target.value,
        );
        if (selected) onChange(selected.value);
        else if (inherited && target.value === "") onChange("");
      }}
    >
      {inherited && (
        <NativeSelectOption value="">Repository default</NativeSelectOption>
      )}
      {queueMethods.map(({ value, label }) => (
        <NativeSelectOption key={value} value={value}>
          {label}
        </NativeSelectOption>
      ))}
    </NativeSelect>
  );
}

export function MergeMethodSettings({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const path = endpoint(repo, "settings/merge-methods");
  const settings = useRequest<MergeSettings>(path);
  return (
    <section
      className="settings-section"
      aria-labelledby="merge-methods-heading"
    >
      <header>
        <div>
          <h3 id="merge-methods-heading">Pull request merge methods</h3>
          <p>
            Choose which methods contributors can use to merge pull requests.
          </p>
        </div>
      </header>
      <Result state={settings} timingLabel="Pull request merge methods">
        {(value) => (
          <MergeMethodForm
            key={value.version}
            repo={repo}
            csrf={csrf}
            path={path}
            settings={value}
            reload={settings.retry}
          />
        )}
      </Result>
    </section>
  );
}

function MergeMethodForm({
  repo,
  csrf,
  path,
  settings,
  reload,
}: {
  repo: Repository;
  csrf: string;
  path: string;
  settings: MergeSettings;
  reload: () => void;
}) {
  const [enabled, setEnabled] = useState(settings.enabled);
  const [autoMerge, setAutoMerge] = useState(settings.auto_merge ?? false);
  const savedDeleteBranches = settings.delete_head_branches ?? false;
  const [deleteBranches, setDeleteBranches] = useState(savedDeleteBranches);
  const [mergeQueue, setMergeQueue] = useState(settings.merge_queue ?? false);
  const savedQueueMethod = settings.merge_queue_method ?? "merge_commit";
  const [queueMethod, setQueueMethod] = useState(savedQueueMethod);
  const savedBuilds = settings.merge_queue_builds ?? 1;
  const [queueBuilds, setQueueBuilds] = useState(String(savedBuilds));
  const savedTimeout = settings.merge_queue_timeout_minutes ?? 60;
  const [queueTimeout, setQueueTimeout] = useState(String(savedTimeout));
  const [overrides, setOverrides] = useState<OverrideDraft[]>(() =>
    (settings.merge_queue_branches ?? []).map((entry) => ({
      id: `saved:${entry.branch}`,
      branch: entry.branch,
      method: entry.method ?? "",
      builds: String(entry.builds ?? ""),
      timeout: String(entry.timeout_minutes ?? ""),
    })),
  );
  const branches: BranchQueue[] = overrides.map((entry) => ({
    branch: entry.branch.trim(),
    ...(entry.method ? { method: entry.method } : {}),
    ...(entry.builds ? { builds: Number(entry.builds) } : {}),
    ...(entry.timeout ? { timeout_minutes: Number(entry.timeout) } : {}),
  }));
  const names = new Set(branches.map((entry) => entry.branch));
  const validOverrides =
    branches.length === names.size &&
    branches.every(
      (entry) =>
        entry.branch &&
        !entry.branch.startsWith("refs/") &&
        (entry.method ||
          entry.builds !== undefined ||
          entry.timeout_minutes !== undefined) &&
        (entry.builds === undefined || allowedNumber(entry.builds, 100)) &&
        (entry.timeout_minutes === undefined ||
          allowedNumber(entry.timeout_minutes, 720)),
    );
  const savedBranches = settings.merge_queue_branches ?? [];
  const changedOverrides =
    branches.length !== savedBranches.length ||
    branches.some(
      (entry, index) =>
        entry.branch !== savedBranches[index]?.branch ||
        entry.method !== savedBranches[index]?.method ||
        entry.builds !== savedBranches[index]?.builds ||
        entry.timeout_minutes !== savedBranches[index]?.timeout_minutes,
    );
  const builds = Number(queueBuilds);
  const timeout = Number(queueTimeout);
  const valid =
    !repo.can_admin ||
    (allowedNumber(builds, 100) &&
      allowedNumber(timeout, 720) &&
      validOverrides);
  const mutation = useMutation(csrf, repo);
  const changed =
    autoMerge !== (settings.auto_merge ?? false) ||
    deleteBranches !== savedDeleteBranches ||
    mergeQueue !== (settings.merge_queue ?? false) ||
    queueMethod !== savedQueueMethod ||
    builds !== savedBuilds ||
    timeout !== savedTimeout ||
    changedOverrides ||
    settings.enabled.length !== enabled.length ||
    settings.enabled.some((method) => !enabled.includes(method));

  async function save() {
    const saved = await mutation.run<MergeSettings>(path, "PUT", {
      expected_version: settings.version,
      enabled,
      ...(autoMerge !== (settings.auto_merge ?? false)
        ? { auto_merge: autoMerge }
        : {}),
      ...(deleteBranches !== savedDeleteBranches
        ? { delete_head_branches: deleteBranches }
        : {}),
      ...(mergeQueue !== (settings.merge_queue ?? false)
        ? { merge_queue: mergeQueue }
        : {}),
      ...(queueMethod !== savedQueueMethod
        ? { merge_queue_method: queueMethod }
        : {}),
      ...(builds !== savedBuilds ? { merge_queue_builds: builds } : {}),
      ...(timeout !== savedTimeout
        ? { merge_queue_timeout_minutes: timeout }
        : {}),
      ...(changedOverrides ? { merge_queue_branches: branches } : {}),
    });
    if (saved) reload();
  }

  function changeOverride(
    id: string,
    fields: Partial<Omit<OverrideDraft, "id">>,
  ) {
    setOverrides((current) =>
      current.map((entry) =>
        entry.id === id ? { ...entry, ...fields } : entry,
      ),
    );
  }

  return (
    <form
      className="grid gap-3 p-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (valid && enabled.length && changed) void save();
      }}
    >
      <fieldset
        className="grid gap-3"
        disabled={repo.archived || mutation.pending}
      >
        <legend className="sr-only">Enabled merge methods</legend>
        {mergeMethods.map(({ value, label, description }) => (
          <label key={value} className="flex items-start gap-2">
            <input
              className="mt-1"
              type="checkbox"
              checked={enabled.includes(value)}
              onChange={({ target }) => {
                setEnabled((current) =>
                  target.checked
                    ? [...current, value]
                    : current.filter((item) => item !== value),
                );
              }}
            />
            <span>
              {label}
              <small className="block text-muted-foreground">
                {description}
              </small>
            </span>
          </label>
        ))}
        <label className="flex items-start gap-2">
          <input
            className="mt-1"
            type="checkbox"
            checked={autoMerge}
            onChange={({ target }) => setAutoMerge(target.checked)}
          />
          <span>
            Allow auto-merge
            <small className="block text-muted-foreground">
              Let writers schedule a merge after required reviews and checks
              pass.
            </small>
          </span>
        </label>
        {repo.can_admin && (
          <div className="grid gap-3">
            <label className="flex items-start gap-2">
              <input
                className="mt-1"
                type="checkbox"
                checked={deleteBranches}
                onChange={({ target }) => setDeleteBranches(target.checked)}
              />
              <span>
                Automatically delete head branches
                <small className="block text-muted-foreground">
                  Delete eligible branches in this repository after their pull
                  requests merge.
                </small>
              </span>
            </label>
            <label className="flex items-start gap-2">
              <input
                className="mt-1"
                type="checkbox"
                checked={mergeQueue}
                onChange={({ target }) => setMergeQueue(target.checked)}
              />
              <span>
                Allow merge queue
                <small className="block text-muted-foreground">
                  Let writers test and merge pull requests in order on branches
                  with required checks.
                </small>
              </span>
            </label>
            <label className="grid gap-1">
              Queue merge method
              <QueueMethodSelect
                value={queueMethod}
                onChange={(method) => {
                  if (method) setQueueMethod(method);
                }}
              />
              <small className="text-muted-foreground">
                Default for branches without a method override. Changing the
                method removes waiting entries on affected branches.
              </small>
            </label>
            <label className="grid gap-1">
              Concurrent queue builds
              <Input
                type="number"
                min={1}
                max={100}
                step={1}
                required
                value={queueBuilds}
                onChange={({ target }) => setQueueBuilds(target.value)}
              />
              <small className="text-muted-foreground">
                Include changes from earlier queue entries when preparing later
                candidates for CI.
              </small>
            </label>
            <label className="grid gap-1">
              Queue check timeout (minutes)
              <Input
                type="number"
                min={1}
                max={720}
                step={1}
                required
                value={queueTimeout}
                onChange={({ target }) => setQueueTimeout(target.value)}
              />
            </label>
            <fieldset className="grid gap-3 rounded-md border p-3">
              <legend className="px-1 text-sm font-medium">
                Branch queue overrides
              </legend>
              <p className="text-sm text-muted-foreground">
                Use exact branch names. Each unset field inherits the repository
                default.
              </p>
              {overrides.map((entry) => (
                <div
                  key={entry.id}
                  className="grid gap-2 rounded-md border p-3 sm:grid-cols-2"
                >
                  <label className="grid gap-1">
                    Branch name
                    <Input
                      required
                      maxLength={255}
                      placeholder="release/stable"
                      value={entry.branch}
                      onChange={({ target }) =>
                        changeOverride(entry.id, { branch: target.value })
                      }
                    />
                  </label>
                  <label className="grid gap-1">
                    Branch queue method
                    <QueueMethodSelect
                      inherited
                      value={entry.method}
                      onChange={(method) =>
                        changeOverride(entry.id, { method })
                      }
                    />
                  </label>
                  <label className="grid gap-1">
                    Branch build limit
                    <Input
                      type="number"
                      min={1}
                      max={100}
                      step={1}
                      placeholder={String(builds)}
                      value={entry.builds}
                      onChange={({ target }) =>
                        changeOverride(entry.id, { builds: target.value })
                      }
                    />
                  </label>
                  <label className="grid gap-1">
                    Branch check timeout (minutes)
                    <Input
                      type="number"
                      min={1}
                      max={720}
                      step={1}
                      placeholder={String(timeout)}
                      value={entry.timeout}
                      onChange={({ target }) =>
                        changeOverride(entry.id, { timeout: target.value })
                      }
                    />
                  </label>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="justify-self-start"
                    onClick={() =>
                      setOverrides((current) =>
                        current.filter(({ id }) => id !== entry.id),
                      )
                    }
                  >
                    Remove {entry.branch.trim() || "branch"} override
                  </Button>
                </div>
              ))}
              {!validOverrides && (
                <p role="status" className="text-sm text-muted-foreground">
                  Give each override a unique branch name and at least one
                  setting.
                </p>
              )}
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={overrides.length >= 100}
                className="justify-self-start"
                onClick={() =>
                  setOverrides((current) => [
                    ...current,
                    {
                      id: crypto.randomUUID(),
                      branch: "",
                      method: "",
                      builds: "",
                      timeout: "",
                    },
                  ])
                }
              >
                Add branch override
              </Button>
            </fieldset>
          </div>
        )}
      </fieldset>
      {!enabled.length && (
        <p role="status">Choose at least one merge method.</p>
      )}
      {mutation.error && (
        <div className="grid justify-items-start gap-2">
          <Failure message={mutation.error} />
          <Button type="button" variant="outline" onClick={reload}>
            Reload merge methods
          </Button>
        </div>
      )}
      <Button
        type="submit"
        className="justify-self-start"
        disabled={
          repo.archived ||
          mutation.pending ||
          !changed ||
          !enabled.length ||
          !valid
        }
      >
        {mutation.pending ? "Saving…" : "Save merge methods"}
      </Button>
    </form>
  );
}
