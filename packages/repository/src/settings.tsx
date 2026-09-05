import { useEffect, useMemo, useState } from "react";
import { Button, Label } from "@primer/react";
import {
  AlertIcon,
  GitBranchIcon,
  PencilIcon,
  PlusIcon,
  ShieldLockIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  repoHref,
  type Ref,
  type Refs,
  type Repository,
} from "./api";
import { Link, short } from "./ui";

const names = new Intl.Collator("en", { numeric: true, sensitivity: "base" });
const branchName = (ref: Ref) => ref.name.replace(/^refs\/heads\//, "");
type ProtectionRule = Repository["protected_branches"][number];
type ProtectionState = { version: number; rules: ProtectionRule[] };

export function Settings({
  repo,
  refs,
  csrf,
  section,
  onDefaultChanged,
  onRepositoryChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  section: "general" | "branches";
  onDefaultChanged: () => void;
  onRepositoryChanged: () => void;
}) {
  return (
    <div className="settings-layout">
      <nav className="settings-nav" aria-label="Repository settings">
        <strong>Settings</strong>
        <Link
          className={section === "general" ? "active" : ""}
          aria-current={section === "general" ? "page" : undefined}
          href={repoHref(repo, { view: "settings" })}
        >
          General
        </Link>
        <Link
          className={section === "branches" ? "active" : ""}
          aria-current={section === "branches" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "branches" })}
        >
          Branches
        </Link>
      </nav>
      {section === "branches" ? (
        <BranchProtectionSettings
          repo={repo}
          csrf={csrf}
          onRepositoryChanged={onRepositoryChanged}
        />
      ) : (
        <DefaultBranchSettings
          repo={repo}
          refs={refs}
          csrf={csrf}
          onChanged={onDefaultChanged}
        />
      )}
    </div>
  );
}

function DefaultBranchSettings({
  repo,
  refs,
  csrf,
  onChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onChanged: () => void;
}) {
  const branches = useMemo(
    () =>
      refs.refs
        .filter((ref) => ref.name.startsWith("refs/heads/"))
        .sort((left, right) =>
          names.compare(branchName(left), branchName(right)),
        ),
    [refs.refs],
  );
  const choices = branches.filter((ref) => ref.name !== refs.head?.name);
  const [editing, setEditing] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [selectedName, setSelectedName] = useState(choices[0]?.name ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const selected = choices.find((ref) => ref.name === selectedName);

  function cancel() {
    setEditing(false);
    setConfirming(false);
    setError(undefined);
    setSelectedName(choices[0]?.name ?? "");
  }

  async function updateDefaultBranch() {
    if (!refs.head || !selected) return;
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "settings/default-branch"), {
        method: "PATCH",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({
          name: branchName(selected),
          expected_head: refs.head.name,
          expected_oid: selected.oid,
        }),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json();
      if (!response.ok) {
        const failure = body as { error?: { message?: string } };
        throw new Error(
          failure.error?.message ?? `Request failed (${response.status})`,
        );
      }
      setEditing(false);
      setConfirming(false);
      onChanged();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The default branch could not be updated",
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="settings-content" aria-labelledby="general-settings">
      <h2 id="general-settings">General</h2>
      <section
        className="settings-section"
        aria-labelledby="default-branch-heading"
      >
        <header>
          <div>
            <h3 id="default-branch-heading">Default branch</h3>
            <p>
              The default branch is the base branch for pull requests and code
              commits.
            </p>
          </div>
          {!editing && choices.length > 0 && (
            <Button
              size="small"
              leadingVisual={PencilIcon}
              onClick={() => setEditing(true)}
            >
              Change
            </Button>
          )}
        </header>
        <div className="default-branch-current">
          <GitBranchIcon />
          <strong>
            {refs.head ? branchName(refs.head) : "No default branch"}
          </strong>
          {refs.head && <code>{short(refs.head.oid)}</code>}
          <Label>default</Label>
        </div>
        {choices.length === 0 && (
          <p className="settings-help">
            Create another branch before changing the default branch.
          </p>
        )}
        {editing && !confirming && (
          <div className="default-branch-form">
            <label htmlFor="default-branch-select">Choose a branch</label>
            <select
              id="default-branch-select"
              value={selectedName}
              onChange={(event) => setSelectedName(event.target.value)}
            >
              {choices.map((ref) => (
                <option key={ref.name} value={ref.name}>
                  {branchName(ref)}
                </option>
              ))}
            </select>
            <div>
              <Button size="small" onClick={cancel}>
                Cancel
              </Button>
              <Button
                size="small"
                variant="primary"
                disabled={!selected}
                onClick={() => setConfirming(true)}
              >
                Update
              </Button>
            </div>
          </div>
        )}
        {confirming && selected && (
          <div className="default-branch-confirm">
            <AlertIcon size={20} />
            <div>
              <strong>
                Change the default branch to {branchName(selected)}?
              </strong>
              <p>
                New pull requests and code views will use this branch by
                default.
              </p>
              {error && (
                <p className="error" role="alert">
                  {error}
                </p>
              )}
              <div>
                <Button size="small" disabled={saving} onClick={cancel}>
                  Cancel
                </Button>
                <Button
                  size="small"
                  variant="danger"
                  disabled={saving}
                  onClick={updateDefaultBranch}
                >
                  {saving
                    ? "Updating…"
                    : "I understand, update the default branch"}
                </Button>
              </div>
            </div>
          </div>
        )}
      </section>
    </section>
  );
}

function BranchProtectionSettings({
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
        {editing === undefined && (
          <Button
            size="small"
            variant="primary"
            leadingVisual={PlusIcon}
            onClick={() => startEdit("new")}
          >
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
                        <Label key={check}>{check}</Label>
                      ))}
                    </div>
                  )}
                </div>
                <div className="protection-actions">
                  <Button
                    size="small"
                    leadingVisual={PencilIcon}
                    aria-label={`Edit ${rule.branch}`}
                    onClick={() => startEdit(index)}
                  >
                    Edit
                  </Button>
                  <Button
                    size="small"
                    variant="danger"
                    leadingVisual={TrashIcon}
                    aria-label={`Delete ${rule.branch}`}
                    onClick={() => {
                      setDeleting(index);
                      setEditing(undefined);
                      setError(undefined);
                    }}
                  >
                    Delete
                  </Button>
                </div>
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
                        <Button size="small" disabled={saving} onClick={cancel}>
                          Cancel
                        </Button>
                        <Button
                          size="small"
                          variant="danger"
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
              <Button size="small" disabled={saving} onClick={cancel}>
                Cancel
              </Button>
              <Button
                size="small"
                variant="primary"
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
