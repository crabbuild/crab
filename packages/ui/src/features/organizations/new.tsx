import { useState } from "react";
import { CheckIcon } from "@primer/octicons-react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { OrganizationAvatar, UserAvatar } from "@/components/ui/user-avatar";
import { navigate, useRequest } from "@/lib/api";
import type {
  ActorSummary,
  Organization,
  OrganizationRole,
  OwnProfile,
} from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { MutationError } from "./feedback";
import { InviteForm } from "./invitations";

// GitHub's two-step setup: name the organization, then add members.
export function NewOrganization({ csrf }: { csrf: string }) {
  const own = useRequest<OwnProfile>("/api/profile");
  return (
    <section className="mx-auto grid w-full max-w-2xl gap-8 px-4 py-10 sm:px-8">
      <header className="grid gap-1 border-b pb-6">
        <p className="m-0 text-sm font-semibold text-muted-foreground">
          Tell us about your organization
        </p>
        <h1 className="m-0 text-3xl font-semibold tracking-tight">
          Set up your organization
        </h1>
      </header>
      <Result state={own} timingLabel="Your profile">
        {({ profile }) =>
          profile ? (
            <OrganizationForm csrf={csrf} />
          ) : (
            <Alert>
              <AlertDescription className="grid gap-3">
                <p className="m-0">
                  Organizations list their owners and members by username.
                  Choose your username in your public profile, then come back to
                  set up your organization.
                </p>
                <Button asChild className="w-fit">
                  <Link href="/-/profile">Set up your public profile</Link>
                </Button>
              </AlertDescription>
            </Alert>
          )
        }
      </Result>
    </section>
  );
}

function OrganizationForm({ csrf }: { csrf: string }) {
  // The UUID is the creation submission: a retry replays it instead of
  // creating a second organization.
  const [id, setId] = useState(() => crypto.randomUUID());
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const mutation = useMutation(csrf, null);
  const trimmed = name.trim();
  const address = new URL(
    organizationHref({ id, name: trimmed }),
    window.location.origin,
  ).href;
  return (
    <form
      className="grid gap-6"
      onSubmit={(event) => {
        event.preventDefault();
        if (!trimmed) return;
        void mutation
          .run<Organization>("/api/organizations", "POST", {
            id,
            name: trimmed,
            description,
          })
          .then((created) => {
            if (created) navigate(organizationHref(created, "?setup=members"));
          });
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="new-organization-name">
            Organization name
            <span aria-hidden="true" className="text-destructive">
              *
            </span>
          </FieldLabel>
          <div className="flex items-center gap-3">
            <OrganizationAvatar id={id} size={36} />
            <Input
              id="new-organization-name"
              value={name}
              required
              maxLength={100}
              autoComplete="organization"
              disabled={mutation.pending}
              aria-describedby="new-organization-name-help"
              onChange={(event) => setName(event.target.value)}
            />
          </div>
          <FieldDescription id="new-organization-name-help">
            This will be the name of your organization on Crab. Your URL will
            be: <code className="break-all">{address}</code>
          </FieldDescription>
        </Field>
        <Field>
          <FieldLabel htmlFor="new-organization-description">
            Description{" "}
            <span className="font-normal text-muted-foreground">
              (optional)
            </span>
          </FieldLabel>
          <Textarea
            id="new-organization-description"
            value={description}
            maxLength={4000}
            disabled={mutation.pending}
            onChange={(event) => setDescription(event.target.value)}
          />
        </Field>
      </FieldGroup>
      <p className="m-0 border-t pt-6 text-sm text-muted-foreground">
        You’ll be the first owner, with full administrative rights to the
        organization, its teams and its repositories. You can invite members
        next.
      </p>
      {mutation.status === 409 ? (
        <Alert variant="destructive">
          <AlertDescription className="grid gap-3">
            <p className="m-0">
              This form already created an organization with different details.
            </p>
            <div className="flex flex-wrap gap-2">
              <Button asChild variant="outline" size="sm">
                <Link href={organizationHref({ id, name: trimmed })}>
                  View organization
                </Link>
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => {
                  setId(crypto.randomUUID());
                  mutation.reset();
                }}
              >
                Create another organization
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      ) : (
        <MutationError error={mutation.error} />
      )}
      <Button
        type="submit"
        className="w-fit"
        disabled={mutation.pending || !trimmed}
      >
        {mutation.pending ? "Creating organization…" : "Next"}
      </Button>
    </form>
  );
}

export function OrganizationSetup({
  organization,
  endpoint,
  csrf,
}: {
  organization: Organization;
  endpoint: string;
  csrf: string;
}) {
  const [invited, setInvited] = useState<
    { recipient: ActorSummary; role: OrganizationRole }[]
  >([]);
  const home = organizationHref(organization);
  return (
    <section className="mx-auto grid w-full max-w-2xl gap-8 px-4 py-10 sm:px-8">
      <header className="grid justify-items-center gap-3 text-center">
        <OrganizationAvatar id={organization.id} size={64} />
        <p className="m-0 text-sm font-semibold text-muted-foreground">
          Welcome to <bdi>{organization.name}</bdi>
        </p>
        <h1 className="m-0 text-3xl font-semibold tracking-tight">
          Add organization members
        </h1>
        <p className="m-0 max-w-lg text-sm text-muted-foreground">
          Members can collaborate in the organization’s repositories, join teams
          and @mention each other. Each person must accept their invitation
          within seven days.
        </p>
      </header>
      <div className="grid gap-5 rounded-lg border p-5">
        <InviteForm
          key={invited.length}
          endpoint={endpoint}
          csrf={csrf}
          onInvited={(recipient, role) =>
            setInvited((previous) => [...previous, { recipient, role }])
          }
        />
        {invited.length > 0 && (
          <ul
            aria-label="Invited people"
            className="m-0 grid list-none gap-2 border-t p-0 pt-4"
          >
            {invited.map(({ recipient, role }) => (
              <li
                key={recipient.id}
                className="flex min-w-0 items-center gap-3 text-sm"
              >
                <UserAvatar
                  name={recipient.handle}
                  avatar={recipient.avatar}
                  size={24}
                />
                <span className="min-w-0 flex-1 wrap-anywhere">
                  <strong>{recipient.name}</strong>{" "}
                  <span className="text-muted-foreground">
                    {recipient.handle}
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1 text-muted-foreground">
                  <CheckIcon aria-hidden="true" />
                  Invited as {role === "owner" ? "owner" : "member"}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="flex justify-end">
        {invited.length ? (
          <Button asChild>
            <Link href={home}>Complete setup</Link>
          </Button>
        ) : (
          <Button asChild variant="outline">
            <Link href={home}>Skip this step</Link>
          </Button>
        )}
      </div>
    </section>
  );
}
