import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import {
  endpoint,
  navigate,
  type MembershipState,
  type Repository,
  type RepositoryMember,
  type RepositoryGroup,
  type Session,
} from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import {
  AlertIcon,
  IssueOpenedIcon,
  EyeIcon,
  PencilIcon,
  PersonIcon,
  PlusIcon,
  ShieldLockIcon,
  SyncIcon,
  TrashIcon,
} from "@primer/octicons-react";
import { ChevronDownIcon } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent } from "react";

import { GroupAccess } from "./groups";

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
      : access === "triage"
        ? {
            label: "Triage",
            description:
              "Can manage issues and pull requests without pushing code.",
            Icon: IssueOpenedIcon,
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

export function MemberSettings({
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
  const [groups, setGroups] = useState<RepositoryGroup[]>([]);
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
      setGroups(membership.groups);
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
          "X-Crab-Repository-Id": repo.id,
        },
        body: JSON.stringify({
          expected_revision: state.revision,
          members: normalizedMembers(),
          groups,
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
      setGroups(membership.groups);
      setEditing(undefined);
      setForm(undefined);
      setRemoving(undefined);
      setConflict(false);
      setSuccess("Access updated.");
      if (identity && !membership.can_admin) setRedirecting(true);
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
        <Button variant="outline" onClick={() => void load()}>
          Retry
        </Button>
      </section>
    );
  return (
    <section className="settings-content" aria-labelledby="member-settings">
      <div className="settings-title members-settings-title">
        <div>
          <h2 id="member-settings">Members</h2>
          <p>Manage who can access this repository and what they can do.</p>
        </div>
        <Button size="sm" variant="default" onClick={startNewMember}>
          <PlusIcon data-icon="inline-start" />
          Add member
        </Button>
      </div>
      {conflict && (
        <div className="notice error" role="alert">
          <p>
            Another administrator changed membership. Reload before saving
            again.
          </p>
          <Button variant="outline" onClick={() => void load()}>
            <SyncIcon data-icon="inline-start" />
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
                      <Button
                        size="icon-sm"
                        variant="ghost"
                        aria-label={`Edit ${label}`}
                        title={`Edit ${label}`}
                        onClick={() => startEdit(member)}
                      >
                        <PencilIcon data-icon="inline-start" />
                      </Button>
                      <Button
                        size="icon-sm"
                        variant="destructive"
                        aria-label={`Remove ${label}`}
                        title={`Remove ${label}`}
                        onClick={() => {
                          setRemoving(member.id);
                          setEditing(undefined);
                          setForm(undefined);
                        }}
                      >
                        <TrashIcon data-icon="inline-start" />
                      </Button>
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
          open
          onOpenChange={(open) => {
            if (!open) closeForm();
          }}
        >
          <DialogContent
            className="sm:max-w-xl max-h-[calc(100dvh-2rem)] overflow-y-auto"
            onOpenAutoFocus={(event) => {
              event.preventDefault();
              subjectInput.current?.focus();
            }}
          >
            <DialogHeader>
              <DialogTitle>
                {editing === undefined ? "Add member" : "Edit member"}
              </DialogTitle>
              <DialogDescription>
                Grant repository access to someone who can sign in with your
                identity provider.
              </DialogDescription>
            </DialogHeader>
            <form className="member-form" onSubmit={saveMember}>
              <div className="member-form-fields">
                <label className="member-field">
                  <span>Subject</span>
                  <Input
                    ref={subjectInput}
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
                  <Input
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
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button
                          variant="outline"
                          className="member-access-trigger"
                          aria-label="Access"
                          aria-describedby="member-access-help"
                        >
                          {memberAccessDetails(form.access).label}
                          <ChevronDownIcon data-icon="inline-end" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent className="w-72">
                        <DropdownMenuRadioGroup
                          value={form.access}
                          aria-label="Access level"
                          onValueChange={(access) => {
                            if (
                              access === "read" ||
                              access === "triage" ||
                              access === "write" ||
                              access === "admin"
                            )
                              setForm({ ...form, access });
                          }}
                        >
                          {(["read", "triage", "write", "admin"] as const).map(
                            (access) => {
                              const details = memberAccessDetails(access);
                              const { Icon } = details;
                              return (
                                <DropdownMenuRadioItem
                                  key={access}
                                  value={access}
                                >
                                  <Icon aria-hidden="true" />
                                  <div>
                                    <div>{details.label}</div>
                                    <p className="text-xs text-muted-foreground">
                                      {details.description}
                                    </p>
                                  </div>
                                </DropdownMenuRadioItem>
                              );
                            },
                          )}
                        </DropdownMenuRadioGroup>
                      </DropdownMenuContent>
                    </DropdownMenu>

                    <span id="member-access-help" className="member-field-help">
                      <MemberAccess access={form.access} />
                    </span>
                  </div>
                </div>
              </div>
              <div className="member-form-actions">
                <Button variant="outline" type="button" onClick={closeForm}>
                  Cancel
                </Button>
                <Button variant="default" type="submit">
                  {editing === undefined ? "Add member" : "Save member"}
                </Button>
              </div>
            </form>
          </DialogContent>
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
              <Button
                variant="outline"
                size="sm"
                onClick={() => setRemoving(undefined)}
              >
                Cancel
              </Button>
              <Button
                size="sm"
                variant="destructive"
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
      {state?.groups_supported && (
        <>
          <Separator className="my-6" />
          <GroupAccess
            value={groups}
            onChange={setGroups}
            disabled={saving || loading || redirecting}
          />
        </>
      )}
      <Separator className="my-6" />
      <div className="member-save-bar">
        <span className="member-save-hint">
          Changes apply to the repository when you save.
        </span>
        <Button
          variant="default"
          disabled={saving || missingRequiredField || redirecting}
          onClick={() => void save()}
        >
          {saving ? "Saving…" : "Save access"}
        </Button>
      </div>
    </section>
  );
}
