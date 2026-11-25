import { useDeferredValue, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { useMutation } from "@/components/discussion/mutations";
import { timestamp } from "@/components/discussion/time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { FieldSelect } from "@/components/ui/field-select";
import { useRequest } from "@/lib/api";
import type {
  ActorSummary,
  InvitationOffer,
  InvitationState,
  InvitationView,
  OrganizationPage,
  OrganizationRole,
} from "@/lib/contracts";
import { MutationError } from "./feedback";
import { organizationHref } from "@/lib/organization-route";
import { organizationInvitationsHref } from "./directory";

export function Invitations({
  endpoint,
  id,
  csrf,
  filters,
}: {
  endpoint: string;
  id: string;
  csrf: string;
  filters: { q: string; role: string; status: InvitationState };
}) {
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams(after ? { after } : {});
  params.set("status", filters.status);
  if (filters.q) params.set("q", filters.q);
  if (filters.role !== "all") params.set("role", filters.role);
  const pending = filters.status === "pending";
  const offers = useRequest<OrganizationPage<InvitationOffer>>(
    `${endpoint}/invitations?${params}`,
  );
  const [notice, setNotice] = useState("");
  const refresh = () => {
    setAfter(undefined);
    offers.retry();
  };
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">
          {pending ? "Pending invitations" : "Organization invitations"}
        </h2>
        <div className="flex gap-2">
          <Button variant="outline" onClick={refresh}>
            Refresh invitations
          </Button>
          <InviteDialog
            endpoint={endpoint}
            csrf={csrf}
            onInvited={() => {
              setNotice(
                "Invitation created. It appears in the invitee’s notifications.",
              );
              refresh();
            }}
          />
        </div>
      </div>
      <p className="text-sm text-muted-foreground">
        People must accept before becoming members. Invitations expire after
        seven days.
      </p>
      <p role="status" className="text-sm">
        {notice}
      </p>
      <Result state={offers} timingLabel="Organization invitations">
        {(data) => (
          <>
            {data.items.length === 0 ? (
              <p className="py-8 text-center text-sm text-muted-foreground">
                {pending && !filters.q && filters.role === "all"
                  ? "No pending invitations."
                  : "No invitations match these filters."}
              </p>
            ) : (
              <ul className="divide-y rounded-lg border">
                {data.items.map((item) => (
                  <OfferRow
                    key={`${item.invitation.recipient}:${item.invitation.version}`}
                    item={item}
                    endpoint={endpoint}
                    csrf={csrf}
                    onChanged={refresh}
                  />
                ))}
              </ul>
            )}
            <div className="flex flex-wrap items-center gap-2">
              <Button
                variant="outline"
                disabled={!after}
                onClick={() => setAfter(undefined)}
              >
                First page
              </Button>
              <Button
                variant="outline"
                disabled={!data.next}
                onClick={() => setAfter(data.next ?? undefined)}
              >
                Next page
              </Button>
              <Button
                variant="ghost"
                onClick={() => {
                  void navigator.clipboard
                    .writeText(
                      new URL(
                        `/-/organizations/${id}/invitation`,
                        window.location.origin,
                      ).href,
                    )
                    .then(
                      () =>
                        setNotice(
                          "Invitation link copied. Only the invited account can accept.",
                        ),
                      () =>
                        setNotice(
                          "Unable to copy. Open the invitation page and copy its address.",
                        ),
                    );
                }}
              >
                Copy invitation link
              </Button>
              <Button variant="ghost" asChild>
                <Link href={`/-/organizations/${id}/invitation`}>
                  Open invitation page
                </Link>
              </Button>
            </div>
          </>
        )}
      </Result>
    </div>
  );
}
export function InviteDialog({
  endpoint,
  csrf,
  onInvited,
  recipient,
}: {
  endpoint: string;
  csrf: string;
  onInvited: () => void;
  recipient?: ActorSummary;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant={recipient ? "ghost" : "default"}>
          {recipient ? "Invite to organization" : "Invite member"}
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Invite a member</DialogTitle>
          <DialogDescription>
            {recipient
              ? "Choose how this collaborator should join the organization."
              : "Choose a registered username and set their organization access."}
          </DialogDescription>
        </DialogHeader>
        {open && (
          <InviteForm
            endpoint={endpoint}
            csrf={csrf}
            recipient={recipient}
            onInvited={() => {
              setOpen(false);
              onInvited();
            }}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}
function InviteForm({
  endpoint,
  csrf,
  onInvited,
  recipient,
}: {
  endpoint: string;
  csrf: string;
  onInvited: () => void;
  recipient?: ActorSummary;
}) {
  const [search, setSearch] = useState(recipient?.handle ?? "");
  const query = useDeferredValue(search.trim());
  const [selected, setSelected] = useState(recipient);
  const candidates = useRequest<{ items: ActorSummary[]; truncated: boolean }>(
    query && !selected
      ? `${endpoint}/invitations/candidates?${new URLSearchParams({ q: query })}`
      : null,
  );
  const current = useRequest<InvitationView>(
    selected ? `${endpoint}/invitations/${selected.id}` : null,
  );
  const [role, setRole] = useState<OrganizationRole>("member");
  const [access, setAccess] = useState<string>();
  const previous = current.data?.invitation;
  const pending = previous?.state === "pending" && previous.reinstates;
  const reinstatement =
    current.data?.reinstatement ??
    (pending && previous
      ? { role: previous.role, version: previous.member_version }
      : undefined);
  const restore = (access ?? (pending ? "restore" : "fresh")) === "restore";
  const mutation = useMutation(csrf, null);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (!selected || !current.data || (restore && !reinstatement)) return;
        void mutation
          .run(`${endpoint}/invitations/${selected.id}`, "PUT", {
            version: current.data.invitation?.version ?? 0,
            ...(restore && reinstatement
              ? {
                  role: reinstatement.role,
                  reinstate_version: reinstatement.version,
                }
              : { role }),
          })
          .then((value) => {
            if (value) onInvited();
          });
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="invite-username">Username</FieldLabel>
          <Input
            id="invite-username"
            value={search}
            maxLength={40}
            disabled={mutation.pending || Boolean(recipient)}
            placeholder="Search by username"
            autoComplete="off"
            onChange={(event) => {
              setSearch(event.target.value);
              setSelected(undefined);
              setAccess(undefined);
            }}
          />
          {!selected && query && (
            <Result state={candidates}>
              {(data) => (
                <>
                  <ul className="flex max-h-56 flex-col gap-1 overflow-y-auto rounded-lg border p-1">
                    {data.items.map((candidate) => (
                      <li key={candidate.id}>
                        <Button
                          className="w-full justify-start"
                          type="button"
                          variant="ghost"
                          onClick={() => {
                            setSelected(candidate);
                            setSearch(candidate.handle);
                          }}
                        >
                          {candidate.name}{" "}
                          <span className="text-muted-foreground">
                            @{candidate.handle}
                          </span>
                        </Button>
                      </li>
                    ))}
                  </ul>
                  {data.items.length === 0 && (
                    <p className="text-sm text-muted-foreground">
                      No matching public profiles. Ask the person to register a
                      profile first.
                    </p>
                  )}
                  {data.truncated && (
                    <p className="text-sm text-muted-foreground">
                      More matches available. Keep typing to narrow your search.
                    </p>
                  )}
                </>
              )}
            </Result>
          )}
          {selected && (
            <p className="text-sm">
              Inviting {selected.name} (@{selected.handle})
            </p>
          )}
        </Field>
        {(reinstatement || restore) && (
          <Field>
            <FieldLabel htmlFor="invitation-access">Previous access</FieldLabel>
            <FieldSelect
              id="invitation-access"
              label="Previous access"
              value={restore ? "restore" : "fresh"}
              disabled={current.loading || mutation.pending}
              onValueChange={setAccess}
              options={[
                { value: "fresh", label: "Start fresh" },
                {
                  value: "restore",
                  label: "Restore previous access",
                  disabled: !reinstatement,
                },
              ]}
            />
            {restore && (
              <p className="text-sm text-muted-foreground">
                {reinstatement
                  ? `Rejoin as ${reinstatement.role === "owner" ? "an owner" : "a member"} and restore eligible team and repository access upon acceptance.`
                  : "Previous access can no longer be restored. Choose Start fresh to send an invitation."}
              </p>
            )}
          </Field>
        )}
        {!restore && (
          <Field>
            <FieldLabel htmlFor="invitation-role">Organization role</FieldLabel>
            <FieldSelect
              id="invitation-role"
              label="Organization role"
              value={role}
              disabled={mutation.pending}
              onValueChange={(role) => setRole(role as OrganizationRole)}
              options={{ member: "Member", owner: "Owner" }}
            />
            <p className="text-sm text-muted-foreground">
              Owners can manage members and organization settings.
            </p>
          </Field>
        )}
        {selected && current.error && <MutationError error={current.error} />}
        <MutationError error={mutation.error} />
        {mutation.error && (
          <Button type="button" variant="outline" onClick={current.retry}>
            Reload current data
          </Button>
        )}
        <Button
          type="submit"
          disabled={
            !selected ||
            !current.data ||
            current.loading ||
            mutation.pending ||
            (restore && !reinstatement)
          }
        >
          {mutation.pending
            ? "Inviting…"
            : restore
              ? "Invite and reinstate"
              : reinstatement
                ? "Invite and start fresh"
                : "Create invitation"}
        </Button>
      </FieldGroup>
    </form>
  );
}
function OfferRow({
  item,
  endpoint,
  csrf,
  onChanged,
}: {
  item: InvitationOffer;
  endpoint: string;
  csrf: string;
  onChanged: () => void;
}) {
  const mutation = useMutation(csrf, null);
  const value = item.invitation;
  async function change(cancel: boolean) {
    const result = await mutation.run(
      `${endpoint}/invitations/${value.recipient}`,
      cancel ? "DELETE" : "PUT",
      cancel
        ? { version: value.version }
        : {
            version: value.version,
            role: value.role,
            reinstate_version: value.reinstates
              ? value.member_version
              : undefined,
          },
    );
    if (result) onChanged();
  }
  return (
    <li className="flex flex-col gap-3 p-4">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-3">
        <div className="min-w-0 break-words">
          <strong className="font-medium">
            {item.profile?.name ?? "Account without a public profile"}
          </strong>
          <p className="mt-1 break-all text-sm text-muted-foreground">
            {item.profile ? `@${item.profile.handle}` : value.recipient} ·{" "}
            {value.role} · <span className="capitalize">{value.state}</span>
            {value.reinstates && <> · Reinstatement</>}
            {value.state === "pending" && (
              <> · Expires {timestamp(value.expires_at_ms)}</>
            )}
          </p>
        </div>
        <div className="flex gap-2">
          {value.state === "expired" && (
            <Button
              variant="outline"
              disabled={mutation.pending}
              onClick={() => void change(false)}
            >
              Retry invitation
            </Button>
          )}
          {(value.state === "pending" || value.state === "expired") && (
            <Button
              variant="outline"
              disabled={mutation.pending}
              onClick={() => void change(true)}
            >
              Cancel invitation
            </Button>
          )}
        </div>
      </div>
      <MutationError error={mutation.error} />
    </li>
  );
}
export function OrganizationInvitation({
  id,
  csrf,
}: {
  id: string;
  csrf: string;
}) {
  const endpoint = `/api/organizations/${id}/invitation`;
  const own = useRequest<InvitationView>(endpoint);
  const mutation = useMutation(csrf, null);
  async function answer(version: number, accept: boolean) {
    if (await mutation.run(endpoint, "POST", { version, accept })) own.retry();
  }
  return (
    <section className="mx-auto w-full max-w-2xl p-4 sm:p-8">
      <Result state={own} timingLabel="Organization invitation">
        {({ organization, invitation }) => (
          <div className="flex flex-col gap-5 rounded-lg border p-6">
            <h1 className="break-words text-2xl font-semibold tracking-tight">
              {organization
                ? `Invitation to ${organization.name}`
                : "Organization invitation"}
            </h1>
            {invitation && (
              <>
                <Badge variant="outline" className="w-fit">
                  {invitation.state}
                </Badge>
                {invitation.state === "pending" ? (
                  <>
                    <p>
                      Join as{" "}
                      {invitation.role === "owner" ? "an owner" : "a member"}.
                      This invitation expires{" "}
                      {timestamp(invitation.expires_at_ms)}.
                    </p>
                    {invitation.reinstates && (
                      <p>
                        Accepting restores your eligible team and repository
                        access.
                      </p>
                    )}
                    <div className="flex flex-wrap gap-2">
                      <Button
                        disabled={mutation.pending}
                        onClick={() => void answer(invitation.version, true)}
                      >
                        Join organization
                      </Button>
                      <Button
                        variant="outline"
                        disabled={mutation.pending}
                        onClick={() => void answer(invitation.version, false)}
                      >
                        Decline invitation
                      </Button>
                    </div>
                  </>
                ) : (
                  <p>
                    {invitation.state === "accepted"
                      ? "You accepted this invitation."
                      : "This invitation is closed. Ask an organization owner for a new invitation."}
                  </p>
                )}
              </>
            )}
            {organization && invitation?.state === "accepted" && (
              <Button asChild variant="outline">
                <Link href={organizationHref(organization)}>
                  View organization
                </Link>
              </Button>
            )}
            <Link href={organizationInvitationsHref} className="w-fit text-sm">
              Back to organization invitations
            </Link>
            <MutationError error={mutation.error} />
            <Button variant="ghost" onClick={own.retry}>
              Reload current data
            </Button>
          </div>
        )}
      </Result>
    </section>
  );
}
