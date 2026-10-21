import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
import {
  AlertIcon,
  PencilIcon,
  PlusIcon,
  ShieldLockIcon,
  TrashIcon,
} from "@primer/octicons-react";
import { useEffect, useState } from "react";

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
  const [checks, setChecks] = useState("");
  const [deleting, setDeleting] = useState<number>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();

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
    setChecks(rule?.required_checks.join("\n") ?? "");
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
      required_checks: checks
        .split(/\r?\n/)
        .map((check) => check.trim())
        .filter(Boolean),
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
              Direct changes are blocked. Pull requests must satisfy each
              rule&apos;s approvals and status checks before merge.
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
                  <p>
                    {rule.required_approvals === 0
                      ? "No approving reviews required"
                      : `${rule.required_approvals} approving review${rule.required_approvals === 1 ? "" : "s"} required`}
                  </p>
                  {rule.required_checks.length > 0 && (
                    <div className="protection-checks">
                      {rule.required_checks.map((check) => (
                        <Badge variant="outline" key={check}>
                          {check}
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
            <input
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
            <select
              id="protection-approvals"
              value={approvals}
              onChange={(event) => setApprovals(event.target.value)}
            >
              {Array.from({ length: 21 }, (_, value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
            <label htmlFor="protection-checks">Required status checks</label>
            <textarea
              id="protection-checks"
              value={checks}
              rows={4}
              placeholder={"ci/test\nsecurity"}
              onChange={(event) => setChecks(event.target.value)}
            />
            <p className="settings-help-inline">
              Enter one exact, case-insensitive check name per line.
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
                variant="default"
                disabled={saving || branch.trim().length === 0}
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
