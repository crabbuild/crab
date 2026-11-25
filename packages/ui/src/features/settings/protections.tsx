import { Textarea } from "@/components/ui/textarea";
import { Input } from "@/components/ui/input";
import { FieldSelect } from "@/components/ui/field-select";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import {
  AlertIcon,
  PencilIcon,
  PlusIcon,
  ShieldLockIcon,
  TrashIcon,
} from "@primer/octicons-react";
import { useEffect, useState } from "react";
import type { Connections } from "./integrations";

type ProtectionRule = Repository["protected_branches"][number];

type ProtectionState = { version: number; rules: ProtectionRule[] };

export function BranchProtectionSettings({
  repo,
  csrf,
  onRepositoryChanged,
}: {
  repo: Repository;
  csrf: string;
  onRepositoryChanged: () => void;
}) {
  const [state, setState] = useState<ProtectionState>({
    version: repo.protection_version,
    rules: repo.protected_branches,
  });
  const [editing, setEditing] = useState<number | "new">();
  const [branch, setBranch] = useState("");
  const [approvals, setApprovals] = useState("0");
  const [codeOwners, setCodeOwners] = useState(false);
  const [requireLastPushApproval, setRequireLastPushApproval] = useState(false);
  const [requireResolution, setRequireResolution] = useState(false);
  const [requireUpToDate, setRequireUpToDate] = useState(false);
  const [requireQueue, setRequireQueue] = useState(false);
  const [requireLinearHistory, setRequireLinearHistory] = useState(false);
  const [allowDeletions, setAllowDeletions] = useState(false);
  const [checks, setChecks] = useState("");
  const [integrations, setIntegrations] = useState<string[]>([]);
  const [chooseIntegrations, setChooseIntegrations] = useState(false);
  const connections = useRequest<Connections>(
    chooseIntegrations ||
      state.rules.some((rule) => rule.required_integrations?.length)
      ? endpoint(repo, "settings/integrations")
      : null,
  );
  const [deleting, setDeleting] = useState<number>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const requiredChecks = checks
    .split(/\r?\n/)
    .map((check) => check.trim())
    .filter(Boolean);
  const hasRequiredChecks =
    requiredChecks.length > 0 || integrations.length > 0;

  useEffect(() => {
    setState({
      version: repo.protection_version,
      rules: repo.protected_branches,
    });
  }, [repo.protection_version, repo.protected_branches]);

  function startEdit(index: number | "new") {
    const rule = index === "new" ? undefined : state.rules[index];
    setBranch(rule?.branch ?? "");
    setApprovals(String(rule?.required_approvals ?? 0));
    setCodeOwners(Boolean(rule?.required_code_owner_reviews));
    setRequireLastPushApproval(Boolean(rule?.require_last_push_approval));
    setRequireResolution(Boolean(rule?.require_conversation_resolution));
    setRequireUpToDate(Boolean(rule?.require_up_to_date));
    setRequireQueue(Boolean(rule?.require_merge_queue));
    setRequireLinearHistory(Boolean(rule?.require_linear_history));
    setAllowDeletions(Boolean(rule?.allow_deletions));
    setChecks(rule?.required_checks.join("\n") ?? "");
    setIntegrations(rule?.required_integrations ?? []);
    setChooseIntegrations(!!rule?.required_integrations?.length);
    setEditing(index);
    setDeleting(undefined);
    setError(undefined);
  }

  function cancel() {
    setEditing(undefined);
    setDeleting(undefined);
    setError(undefined);
  }

  async function replaceRules(rules: ProtectionRule[]) {
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(
        endpoint(repo, "settings/branch-protections"),
        {
          method: "PUT",
          headers: {
            Accept: "application/json",
            "Content-Type": "application/json",
            "X-CSRF-Token": csrf,
            "X-Crab-Repository-Id": repo.id,
          },
          body: JSON.stringify({
            expected_version: state.version,
            rules,
          }),
        },
      );
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json();
      if (!response.ok) {
        const failure = body as {
          error?: { code?: string; message?: string };
        };
        if (failure.error?.code === "settings_changed") onRepositoryChanged();
        throw new Error(
          failure.error?.message ?? `Request failed (${response.status})`,
        );
      }
      setState(body as ProtectionState);
      setEditing(undefined);
      setDeleting(undefined);
      onRepositoryChanged();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Branch protection settings could not be updated",
      );
    } finally {
      setSaving(false);
    }
  }

  function saveRule() {
    if (editing === undefined) return;
    const rule: ProtectionRule = {
      branch: branch.trim(),
      required_approvals: Number(approvals),
      ...(codeOwners ? { required_code_owner_reviews: true } : {}),
      ...(requireLastPushApproval ? { require_last_push_approval: true } : {}),
      ...(requireResolution ? { require_conversation_resolution: true } : {}),
      ...(requireUpToDate ? { require_up_to_date: true } : {}),
      ...(requireQueue ? { require_merge_queue: true } : {}),
      ...(requireLinearHistory ? { require_linear_history: true } : {}),
      ...(allowDeletions ? { allow_deletions: true } : {}),
      required_checks: requiredChecks,
      ...(integrations.length ? { required_integrations: integrations } : {}),
    };
    const rules = [...state.rules];
    if (editing === "new") rules.push(rule);
    else rules[editing] = rule;
    void replaceRules(rules);
  }

  return (
    <section className="settings-content" aria-labelledby="branch-settings">
      <div className="settings-title">
        <div>
          <h2 id="branch-settings">Branches</h2>
          <p>Control how changes reach important branches.</p>
        </div>
        {!repo.archived && editing === undefined && (
          <Button size="sm" variant="default" onClick={() => startEdit("new")}>
            <PlusIcon data-icon="inline-start" />
            Add branch protection rule
          </Button>
        )}
      </div>
      <section
        className="settings-section protection-settings"
        aria-labelledby="protection-heading"
      >
        <header>
          <div>
            <h3 id="protection-heading">Branch protection rules</h3>
            <p>
              Pull requests must satisfy the configured requirements before
              merging into a protected branch.
            </p>
          </div>
        </header>
        {state.rules.length === 0 ? (
          <div className="settings-empty">
            <ShieldLockIcon size={24} />
            <strong>No branch protection rules</strong>
            <p>Add a rule to require pull requests for an exact branch.</p>
          </div>
        ) : (
          <ul className="protection-list">
            {state.rules.map((rule, index) => (
              <li key={rule.branch}>
                <ShieldLockIcon />
                <div className="protection-summary">
                  <strong>
                    <code>{rule.branch}</code>
                  </strong>
                  {(rule.required_approvals > 0 ||
                    rule.required_code_owner_reviews ||
                    !rule.require_last_push_approval) && (
                    <p>
                      {rule.required_approvals === 0
                        ? rule.required_code_owner_reviews
                          ? "Code owner approval required"
                          : "No approving reviews required"
                        : `${rule.required_approvals} approving review${rule.required_approvals === 1 ? "" : "s"} required`}
                      {rule.required_approvals > 0 &&
                      rule.required_code_owner_reviews
                        ? " · Code owner approval required"
                        : ""}
                    </p>
                  )}
                  {rule.require_last_push_approval && (
                    <p>Approval of the most recent push required</p>
                  )}
                  {rule.require_conversation_resolution && (
                    <p>Conversation resolution required</p>
                  )}
                  {rule.require_merge_queue && <p>Merge queue required</p>}
                  {rule.allow_deletions && <p>Deletions allowed</p>}
                  {rule.require_linear_history && (
                    <p>Linear history required</p>
                  )}
                  {rule.require_up_to_date && !rule.require_merge_queue && (
                    <p>Up-to-date head branch required</p>
                  )}
                  {(rule.required_checks.length > 0 ||
                    !!rule.required_integrations?.length) && (
                    <div className="protection-checks">
                      {rule.required_checks.map((check) => (
                        <Badge variant="outline" key={check}>
                          {check}
                        </Badge>
                      ))}
                      {rule.required_integrations?.map((id) => (
                        <Badge variant="outline" key={id}>
                          GitHub Actions ·{" "}
                          {connections.data?.items.find(
                            (item) => item.id === id,
                          )?.name ?? "Workflow"}
                        </Badge>
                      ))}
                    </div>
                  )}
                </div>
                {!repo.archived && (
                  <div className="protection-actions">
                    <Button
                      variant="outline"
                      size="sm"
                      aria-label={`Edit ${rule.branch}`}
                      onClick={() => startEdit(index)}
                    >
                      <PencilIcon data-icon="inline-start" />
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="destructive"
                      aria-label={`Delete ${rule.branch}`}
                      onClick={() => {
                        setDeleting(index);
                        setEditing(undefined);
                        setError(undefined);
                      }}
                    >
                      <TrashIcon data-icon="inline-start" />
                      Delete
                    </Button>
                  </div>
                )}
                {deleting === index && (
                  <div
                    className="protection-delete-confirm"
                    role="region"
                    aria-label={`Delete ${rule.branch} protection`}
                  >
                    <AlertIcon />
                    <div>
                      <strong>Remove protection from {rule.branch}?</strong>
                      <p>
                        Direct updates will be allowed after this rule is
                        removed.
                      </p>
                      {error && (
                        <p className="error" role="alert">
                          {error}
                        </p>
                      )}
                      <div>
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={saving}
                          onClick={cancel}
                        >
                          Cancel
                        </Button>
                        <Button
                          size="sm"
                          variant="destructive"
                          disabled={saving}
                          onClick={() =>
                            void replaceRules(
                              state.rules.filter(
                                (_, candidate) => candidate !== index,
                              ),
                            )
                          }
                        >
                          {saving ? "Removing…" : "Remove rule"}
                        </Button>
                      </div>
                    </div>
                  </div>
                )}
              </li>
            ))}
          </ul>
        )}
        {editing !== undefined && (
          <div className="protection-form">
            <h3>
              {editing === "new"
                ? "Add branch protection rule"
                : `Edit ${state.rules[editing].branch}`}
            </h3>
            <label htmlFor="protection-branch">Branch name</label>
            <Input
              id="protection-branch"
              value={branch}
              maxLength={255}
              autoFocus
              onChange={(event) => setBranch(event.target.value)}
            />
            <p className="settings-help-inline">
              Use an exact branch name without the <code>refs/heads/</code>
              prefix.
            </p>
            <label htmlFor="protection-approvals">
              Required approving reviews
            </label>
            <FieldSelect
              id="protection-approvals"
              value={approvals}
              onValueChange={(nextValue) => setApprovals(nextValue)}
              options={[
                ...Array.from({ length: 21 }, (_, value) => ({
                  value: String(value),
                  label: value,
                })),
              ]}
            />
            <label className="flex items-center gap-2">
              <Checkbox
                checked={codeOwners}
                onCheckedChange={(checked) => setCodeOwners(checked === true)}
              />
              Require review from code owners
            </label>
            <p className="settings-help-inline">
              Require an approval for matching paths from an eligible owner in
              the base branch&apos;s CODEOWNERS file.
            </p>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={requireLastPushApproval}
                onCheckedChange={(checked) =>
                  setRequireLastPushApproval(checked === true)
                }
              />
              Require approval of the most recent reviewable push
            </label>
            <p className="settings-help-inline">
              A repository writer other than the person who pushed must approve.
            </p>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={requireResolution}
                onCheckedChange={(checked) =>
                  setRequireResolution(checked === true)
                }
              />
              Require conversation resolution
            </label>
            <p className="settings-help-inline">
              Resolve every published review thread before merging.
            </p>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={requireQueue}
                disabled={saving || (!hasRequiredChecks && !requireQueue)}
                onCheckedChange={(checked) => setRequireQueue(checked === true)}
              />
              Require merge queue
            </label>
            <p className="settings-help-inline">
              Require every pull request to pass checks in the queue. Configure
              at least one required check and a compatible queue merge method.
            </p>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={requireLinearHistory}
                onCheckedChange={(checked) =>
                  setRequireLinearHistory(checked === true)
                }
              />
              Require linear history
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={allowDeletions}
                onCheckedChange={(checked) =>
                  setAllowDeletions(checked === true)
                }
              />
              Allow deletions
            </label>
            <p className="settings-help-inline">
              Writers can delete the branch when the default branch and ruleset
              settings permit it.
            </p>
            <label htmlFor="protection-checks">Required status checks</label>
            <Textarea
              id="protection-checks"
              value={checks}
              rows={4}
              placeholder={"ci/test\nsecurity"}
              onChange={(event) => setChecks(event.target.value)}
            />
            <p className="settings-help-inline">
              Enter one exact, case-insensitive check name per line.
            </p>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={requireUpToDate}
                disabled={!hasRequiredChecks && !requireUpToDate}
                onCheckedChange={(checked) =>
                  setRequireUpToDate(checked === true)
                }
              />
              Require branches to be up to date before merging
            </label>
            {!hasRequiredChecks && (
              <p className="settings-help-inline">
                Add a required check or workflow to use this setting.
              </p>
            )}
            {chooseIntegrations ? (
              <fieldset className="grid gap-2 rounded-md border p-3">
                <legend className="text-sm font-semibold">
                  Required GitHub Actions workflows
                </legend>
                <Result state={connections} timingLabel="Available workflows">
                  {({ items, available }) => (
                    <>
                      {!available && (
                        <p className="settings-help-inline">
                          GitHub Actions is unavailable on this server.
                        </p>
                      )}
                      {items.map((item) => (
                        <label
                          key={item.id}
                          className="flex items-center gap-2 text-sm"
                        >
                          <Checkbox
                            checked={integrations.includes(item.id)}
                            disabled={
                              saving ||
                              (!available && !integrations.includes(item.id))
                            }
                            onCheckedChange={(checked) =>
                              setIntegrations((current) =>
                                checked === true
                                  ? [...current, item.id]
                                  : current.filter((id) => id !== item.id),
                              )
                            }
                          />
                          {item.name} · workflow {item.workflow}
                        </label>
                      ))}
                      {integrations
                        .filter((id) => !items.some((item) => item.id === id))
                        .map((id) => (
                          <label
                            key={id}
                            className="flex items-center gap-2 text-sm"
                          >
                            <Checkbox
                              checked
                              disabled={saving}
                              onCheckedChange={() =>
                                setIntegrations((current) =>
                                  current.filter((item) => item !== id),
                                )
                              }
                            />
                            Disconnected workflow{" "}
                            <code className="break-all">{id}</code>
                          </label>
                        ))}
                      {items.length === 0 && available && (
                        <p className="text-sm">
                          <Link
                            className="underline"
                            href={repoHref(repo, {
                              view: "settings",
                              section: "integrations",
                            })}
                          >
                            Connect a GitHub Actions workflow
                          </Link>
                        </p>
                      )}
                    </>
                  )}
                </Result>
              </fieldset>
            ) : (
              <Button
                className="w-fit"
                variant="outline"
                size="sm"
                onClick={() => setChooseIntegrations(true)}
              >
                Require a GitHub Actions workflow
              </Button>
            )}
            {error && (
              <p className="error" role="alert">
                {error}
              </p>
            )}
            <div>
              <Button
                variant="outline"
                size="sm"
                disabled={saving}
                onClick={cancel}
              >
                Cancel
              </Button>
              <Button
                size="sm"
                variant="default"
                disabled={
                  saving ||
                  branch.trim().length === 0 ||
                  ((requireUpToDate || requireQueue) && !hasRequiredChecks)
                }
                onClick={saveRule}
              >
                {saving ? "Saving…" : "Save changes"}
              </Button>
            </div>
          </div>
        )}
      </section>
    </section>
  );
}
