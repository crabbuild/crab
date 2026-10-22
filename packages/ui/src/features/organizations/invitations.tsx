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
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { useRequest } from "@/lib/api";
import type { ActorProfile } from "@/features/profile/model";
import { MutationError } from "./feedback";
import type {
  Invitation,
  InvitationView,
  Membership,
  OrganizationPage,
  OrganizationRole,
} from "./model";

type Candidate = Pick<ActorProfile, "id" | "name" | "handle">;
type Offer = { invitation: Invitation; profile: Membership["profile"] };
export function Invitations({
  endpoint,
  id,
  csrf,
}: {
  endpoint: string;
  id: string;
  csrf: string;
}) {
  const [after, setAfter] = useState<string>();
  const offers = useRequest<OrganizationPage<Offer>>(
    `${endpoint}/invitations?${new URLSearchParams(after ? { after } : {})}`,
  );
  const [notice, setNotice] = useState("");
  const refresh = () => {
    setAfter(undefined);
    offers.retry();
  };
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">Pending invitations</h2>
        <div className="flex gap-2">
          <Button variant="outline" onClick={refresh}>
            Refresh invitations
          </Button>
          <InviteDialog
            endpoint={endpoint}
            csrf={csrf}
            onInvited={() => {
              setNotice(
                "Invitation created. Share the invitation link with the invitee.",
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
                No pending invitations.
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
function InviteDialog({
  endpoint,
  csrf,
  onInvited,
}: {
  endpoint: string;
  csrf: string;
  onInvited: () => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>Invite member</Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Invite a member</DialogTitle>
          <DialogDescription>
            Choose a registered username and their organization role. Repository
            access is managed separately.
          </DialogDescription>
        </DialogHeader>
        {open && (
          <InviteForm
            endpoint={endpoint}
            csrf={csrf}
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
}: {
  endpoint: string;
  csrf: string;
  onInvited: () => void;
}) {
  const [search, setSearch] = useState("");
  const query = useDeferredValue(search.trim());
  const [selected, setSelected] = useState<Candidate>();
  const candidates = useRequest<{ items: Candidate[]; truncated: boolean }>(
    query && !selected
      ? `${endpoint}/invitations/candidates?${new URLSearchParams({ q: query })}`
      : null,
  );
  const current = useRequest<InvitationView>(
    selected ? `${endpoint}/invitations/${selected.id}` : null,
  );
  const [role, setRole] = useState<OrganizationRole>("member");
  const mutation = useMutation(csrf, null);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (!selected || !current.data) return;
        void mutation
          .run(`${endpoint}/invitations/${selected.id}`, "PUT", {
            version: current.data.invitation?.version ?? 0,
            role,
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
            disabled={mutation.pending}
            placeholder="Search by username"
            autoComplete="off"
            onChange={(event) => {
              setSearch(event.target.value);
              setSelected(undefined);
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
        <Field>
          <FieldLabel htmlFor="invitation-role">Organization role</FieldLabel>
          <NativeSelect
            id="invitation-role"
            value={role}
            disabled={mutation.pending}
            onChange={(event) =>
              setRole(event.target.value as OrganizationRole)
            }
          >
            <NativeSelectOption value="member">Member</NativeSelectOption>
            <NativeSelectOption value="owner">Owner</NativeSelectOption>
          </NativeSelect>
          <p className="text-sm text-muted-foreground">
            Owners can manage members and organization settings.
          </p>
        </Field>
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
            !selected || !current.data || current.loading || mutation.pending
          }
        >
          {mutation.pending ? "Inviting…" : "Create invitation"}
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
  item: Offer;
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
        : { version: value.version, role: value.role },
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
            {value.role} · {value.state === "expired" ? "Expired" : "Expires"}{" "}
            {timestamp(value.expires_at_ms)}
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
          <Button
            variant="outline"
            disabled={mutation.pending}
            onClick={() => void change(true)}
          >
            Cancel invitation
          </Button>
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
                <Link href={`/-/organizations/${id}`}>View organization</Link>
              </Button>
            )}
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
