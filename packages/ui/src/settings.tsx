import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import {
  ActionList,
  ActionMenu,
  Button,
  Dialog,
  IconButton,
  Label,
  TextInput,
} from "@primer/react";
import {
  AlertIcon,
  ArchiveIcon,
  EyeIcon,
  GitBranchIcon,
  PencilIcon,
  PersonIcon,
  PlusIcon,
  ShieldLockIcon,
  SyncIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  type Ref,
  type Refs,
  type Repository,
  type MembershipState,
  type RepositoryMember,
  type Session,
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
  identity,
  onDefaultChanged,
  onRepositoryChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  section: "general" | "branches" | "members";
  identity: Session["user"];
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
        <Link
          className={section === "members" ? "active" : ""}
          aria-current={section === "members" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "members" })}
        >
          Members
        </Link>
      </nav>
      {section === "members" ? (
        <MemberSettings repo={repo} csrf={csrf} identity={identity} />
      ) : section === "branches" ? (
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
          onRepositoryChanged={onRepositoryChanged}
        />
      )}
    </div>
  );
}

type DraftMember = RepositoryMember & { id: number };
type MembershipFailure = { error?: { code?: string; message?: string } };

function failureMessage(body: unknown, fallback: string) {
  const message = (body as MembershipFailure).error?.message;
  return typeof message === "string" ? message : fallback;
}

function memberAccessDetails(access: RepositoryMember["access"]) {
  return access === "admin"
    ? {
        label: "Admin",
        description: "Can manage repository settings and members.",
        Icon: ShieldLockIcon,
      }
    : access === "write"
      ? {
          label: "Write",
          description: "Can push changes and collaborate on the repository.",
          Icon: PencilIcon,
        }
      : {
          label: "Read",
          description: "Can view and clone the repository.",
          Icon: EyeIcon,
        };
}

function MemberAccess({ access }: { access: RepositoryMember["access"] }) {
  const details = memberAccessDetails(access);
  const { Icon } = details;
  return (
    <span
      className="member-role"
      data-access={access}
      title={details.description}
    >
      <Icon size={16} aria-hidden="true" />
      <span>{details.label}</span>
    </span>
  );
}

function MemberSettings({
  repo,
  csrf,
  identity,
}: {
  repo: Repository;
  csrf: string;
  identity: Session["user"];
}) {
  const [state, setState] = useState<MembershipState>();
  const [draft, setDraft] = useState<DraftMember[]>([]);
  const [error, setError] = useState<string>();
  const [success, setSuccess] = useState<string>();
  const [conflict, setConflict] = useState(false);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [editing, setEditing] = useState<number>();
  const [form, setForm] = useState<RepositoryMember>();
  const [removing, setRemoving] = useState<number>();
  const [redirecting, setRedirecting] = useState(false);
  const nextId = useRef(0);
  const subjectInput = useRef<HTMLInputElement>(null);

  function membersToDraft(members: RepositoryMember[]) {
    return members.map((member) => ({ ...member, id: nextId.current++ }));
  }

  function newMember() {
    return { subject: "", name: "", access: "read" } as const;
  }

  function startNewMember() {
    setEditing(undefined);
    setForm(newMember());
    setRemoving(undefined);
    setError(undefined);
    setSuccess(undefined);
  }

  function closeForm() {
    setEditing(undefined);
    setForm(undefined);
  }

  function normalizedMembers() {
    return draft.map(({ id: _, subject, name, access }) => ({
      subject: subject.trim(),
      name: name.trim(),
      access,
    }));
  }

  async function load() {
    setLoading(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "members"), {
        headers: { Accept: "application/json" },
      });
      const body: unknown = await response.json();
      if (!response.ok)
        throw new Error(failureMessage(body, "Membership could not be loaded"));
      const membership = body as MembershipState;
      setState(membership);
      setDraft(membersToDraft(membership.members));
      setEditing(undefined);
      setForm(undefined);
      setRemoving(undefined);
      setConflict(false);
      setSuccess(undefined);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Membership could not be loaded",
      );
    } finally {
      setLoading(false);
    }
  }
  useEffect(() => {
    void load();
  }, [repo.owner, repo.name]);

  useEffect(() => {
    if (!redirecting) return;
    const timer = window.setTimeout(() => navigate(repoHref(repo)), 750);
    return () => window.clearTimeout(timer);
  }, [redirecting, repo]);

  async function save() {
    if (!state) return;
    setError(undefined);
    setSuccess(undefined);
    setSaving(true);
    try {
      const response = await fetch(endpoint(repo, "members"), {
        method: "PUT",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({
          expected_revision: state.revision,
          members: normalizedMembers(),
        }),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json();
      if (!response.ok) {
        const failure = body as MembershipFailure;
        if (failure.error?.code === "membership_changed") {
          setConflict(true);
          setError(undefined);
        } else
          setError(failureMessage(body, "Membership could not be updated"));
        return;
      }
      const membership = body as MembershipState;
      setState(membership);
      setDraft(membersToDraft(membership.members));
      setEditing(undefined);
      setForm(undefined);
      setRemoving(undefined);
      setConflict(false);
      setSuccess("Membership updated.");
      if (
        identity &&
        !membership.members.some(
          (member) =>
            member.subject === identity.subject && member.access === "admin",
        )
      )
        setRedirecting(true);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Membership could not be updated",
      );
    } finally {
      setSaving(false);
    }
  }

  function saveMember(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!form) return;
    const member = {
      subject: form.subject.trim(),
      name: form.name.trim(),
      access: form.access,
    };
    if (!member.subject || !member.name) return;
    setDraft((members) =>
      editing === undefined
        ? [...members, { ...member, id: nextId.current++ }]
        : members.map((candidate) =>
            candidate.id === editing
              ? { ...member, id: candidate.id }
              : candidate,
          ),
    );
    setEditing(undefined);
    setForm(undefined);
    setError(undefined);
  }

  function startEdit(member: DraftMember) {
    setEditing(member.id);
    setForm({
      subject: member.subject,
      name: member.name,
      access: member.access,
    });
    setRemoving(undefined);
  }

  const missingRequiredField = draft.some(
    (member) => !member.subject.trim() || !member.name.trim(),
  );
  if (loading)
    return (
      <section className="settings-content">
        <p role="status">Loading members…</p>
      </section>
    );
  if (!state)
    return (
      <section className="settings-content">
        <p role="alert">{error ?? "Membership could not be loaded"}</p>
        <Button onClick={() => void load()}>Retry</Button>
      </section>
    );
  return (
    <section className="settings-content" aria-labelledby="member-settings">
      <div className="settings-title members-settings-title">
        <div>
          <h2 id="member-settings">Members</h2>
          <p>Manage who can access this repository and what they can do.</p>
        </div>
        <Button
          size="small"
          variant="primary"
          leadingVisual={PlusIcon}
          onClick={startNewMember}
        >
          Add member
        </Button>
      </div>
      {conflict && (
        <div className="notice error" role="alert">
          <p>
            Another administrator changed membership. Reload before saving
            again.
          </p>
          <Button leadingVisual={SyncIcon} onClick={() => void load()}>
            Reload members
          </Button>
        </div>
      )}
      {error && <p role="alert">{error}</p>}
      {success && <p role="status">{success}</p>}
      <div className="member-settings">
        {draft.length === 0 ? (
          <div className="member-list-shell member-empty">
            <PersonIcon size={24} aria-hidden="true" />
            <strong>No members yet</strong>
            <p>Add someone to grant access to this repository.</p>
          </div>
        ) : (
          <div className="member-list-shell">
            <div className="member-list-header" aria-hidden="true">
              <span>Member</span>
              <span>Access</span>
              <span>Actions</span>
            </div>
            <ul className="member-list" aria-label="Repository members">
              {draft.map((member) => {
                const label = member.name || member.subject;
                return (
                  <li className="member-row" key={member.id}>
                    <div className="member-identity">
                      <span className="member-avatar" aria-hidden="true">
                        <PersonIcon size={16} />
                      </span>
                      <div className="member-subject">
                        <strong>{label}</strong>
                        <span title="Identity provider subject">
                          {member.subject}
                        </span>
                      </div>
                    </div>
                    <MemberAccess access={member.access} />
                    <div className="member-actions">
                      <IconButton
                        icon={PencilIcon}
                        size="small"
                        variant="invisible"
                        aria-label={`Edit ${label}`}
                        title={`Edit ${label}`}
                        onClick={() => startEdit(member)}
                      />
                      <IconButton
                        icon={TrashIcon}
                        size="small"
                        variant="danger"
                        aria-label={`Remove ${label}`}
                        title={`Remove ${label}`}
                        onClick={() => {
                          setRemoving(member.id);
                          setEditing(undefined);
                          setForm(undefined);
                        }}
                      />
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        )}
      </div>
      {form && (
        <Dialog
          title={editing === undefined ? "Add member" : "Edit member"}
          subtitle="Grant repository access to someone who can sign in with your identity provider."
          width="large"
          position={{ narrow: "bottom", regular: "center" }}
          initialFocusRef={subjectInput}
          onClose={closeForm}
        >
          <form className="member-form" onSubmit={saveMember}>
            <div className="member-form-fields">
              <label className="member-field">
                <span>Subject</span>
                <TextInput
                  ref={subjectInput}
                  block
                  value={form.subject}
                  required
                  placeholder="e.g. provider|user-id"
                  autoComplete="off"
                  aria-describedby="member-subject-help"
                  onChange={(event) =>
                    setForm({ ...form, subject: event.target.value })
                  }
                />
                <span id="member-subject-help" className="member-field-help">
                  The stable identity-provider subject for this person.
                </span>
              </label>
              <label className="member-field">
                <span>Display name</span>
                <TextInput
                  block
                  value={form.name}
                  required
                  placeholder="e.g. Ada Lovelace"
                  autoComplete="name"
                  aria-describedby="member-name-help"
                  onChange={(event) =>
                    setForm({ ...form, name: event.target.value })
                  }
                />
                <span id="member-name-help" className="member-field-help">
                  The name shown in this repository&apos;s member list.
                </span>
              </label>
              <div className="member-field">
                <span>Access</span>
                <div className="member-access-control">
                  <ActionMenu>
                    <ActionMenu.Button
                      className="member-access-trigger"
                      aria-label="Access"
                      aria-describedby="member-access-help"
                    >
                      {memberAccessDetails(form.access).label}
                    </ActionMenu.Button>
                    <ActionMenu.Overlay width="medium">
                      <ActionList
                        selectionVariant="single"
                        aria-label="Access level"
                      >
                        {(["read", "write", "admin"] as const).map((access) => {
                          const details = memberAccessDetails(access);
                          const { Icon } = details;
                          return (
                            <ActionList.Item
                              key={access}
                              role="menuitemradio"
                              selected={form.access === access}
                              onSelect={() => setForm({ ...form, access })}
                            >
                              <ActionList.LeadingVisual>
                                <Icon aria-hidden="true" />
                              </ActionList.LeadingVisual>
                              {details.label}
                              <ActionList.Description variant="block">
                                {details.description}
                              </ActionList.Description>
                            </ActionList.Item>
                          );
                        })}
                      </ActionList>
                    </ActionMenu.Overlay>
                  </ActionMenu>
                  <span id="member-access-help" className="member-field-help">
                    <MemberAccess access={form.access} />
                  </span>
                </div>
              </div>
            </div>
            <div className="member-form-actions">
              <Button type="button" onClick={closeForm}>
                Cancel
              </Button>
              <Button variant="primary" type="submit">
                {editing === undefined ? "Add member" : "Save member"}
              </Button>
            </div>
          </form>
        </Dialog>
      )}
      {removing !== undefined && (
        <div
          className="member-remove-confirm"
          role="region"
          aria-label="Remove member"
        >
          <AlertIcon size={20} aria-hidden="true" />
          <div>
            <strong>Remove this member?</strong>
            <p>The change will take effect when you save membership.</p>
            <div className="member-actions">
              <Button size="small" onClick={() => setRemoving(undefined)}>
                Cancel
              </Button>
              <Button
                size="small"
                variant="danger"
                onClick={() => {
                  setDraft((members) =>
                    members.filter((member) => member.id !== removing),
                  );
                  setRemoving(undefined);
                  setError(undefined);
                }}
              >
                Remove member
              </Button>
            </div>
          </div>
        </div>
      )}
      {missingRequiredField && (
        <p className="settings-help" role="status">
          Every member needs a subject and display name before saving.
        </p>
      )}
      <div className="member-save-bar">
        <span className="member-save-hint">
          Changes apply to the repository when you save.
        </span>
        <Button
          variant="primary"
          disabled={saving || missingRequiredField || redirecting}
          onClick={() => void save()}
        >
          {saving ? "Saving…" : "Save members"}
        </Button>
      </div>
    </section>
  );
}

function DefaultBranchSettings({
  repo,
  refs,
  csrf,
  onChanged,
  onRepositoryChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onChanged: () => void;
  onRepositoryChanged: () => void;
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
          {!repo.archived && !editing && choices.length > 0 && (
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
      <ArchiveSettings
        repo={repo}
        csrf={csrf}
        onChanged={onRepositoryChanged}
      />
    </section>
  );
}

function ArchiveSettings({
  repo,
  csrf,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  onChanged: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const fullName = `${repo.owner}/${repo.name}`;
  const action = repo.archived ? "unarchive" : "archive";

  async function updateArchive() {
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "settings/archive"), {
        method: "PUT",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({
          expected_version: repo.archive_version,
          archived: !repo.archived,
          repository: confirmation,
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
      setConfirming(false);
      setConfirmation("");
      onChanged();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : `The repository could not be ${action}d`,
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="danger-zone" aria-labelledby="danger-zone-heading">
      <h2 id="danger-zone-heading">Danger Zone</h2>
      <div className="danger-zone-row">
        <div>
          <strong>
            {repo.archived
              ? "Unarchive this repository"
              : "Archive this repository"}
          </strong>
          <p>
            {repo.archived
              ? "Restore writes to code and collaboration data."
              : "Make code and collaboration data read-only without deleting it."}
          </p>
        </div>
        {!confirming && (
          <Button
            size="small"
            variant="danger"
            onClick={() => setConfirming(true)}
          >
            {repo.archived
              ? "Unarchive this repository"
              : "Archive this repository"}
          </Button>
        )}
      </div>
      {confirming && (
        <div
          className="archive-confirm"
          role="region"
          aria-label={`${action} repository`}
        >
          <AlertIcon size={20} />
          <div>
            <strong>
              {repo.archived
                ? "This will allow changes to the repository again."
                : "This will make the repository read-only for every user."}
            </strong>
            <p>
              To confirm, type <code>{fullName}</code> below.
            </p>
            <label htmlFor="archive-confirmation">Repository name</label>
            <input
              id="archive-confirmation"
              value={confirmation}
              autoComplete="off"
              onChange={(event) => setConfirmation(event.target.value)}
            />
            {error && (
              <p className="error" role="alert">
                {error}
              </p>
            )}
            <div>
              <Button
                size="small"
                disabled={saving}
                onClick={() => {
                  setConfirming(false);
                  setConfirmation("");
                  setError(undefined);
                }}
              >
                Cancel
              </Button>
              <Button
                size="small"
                variant="danger"
                disabled={saving || confirmation !== fullName}
                leadingVisual={ArchiveIcon}
                onClick={updateArchive}
              >
                {saving
                  ? `${repo.archived ? "Unarchiving" : "Archiving"}…`
                  : `I understand the consequences, ${action} this repository`}
              </Button>
            </div>
          </div>
        </div>
      )}
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
        {!repo.archived && editing === undefined && (
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
                {!repo.archived && (
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
