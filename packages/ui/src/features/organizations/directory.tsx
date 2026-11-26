import { useState } from "react";
import { PlusIcon } from "@primer/octicons-react";
import { timestamp } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { OrganizationAvatar } from "@/components/ui/user-avatar";
import { useRequest } from "@/lib/api";
import { organizationHref } from "@/lib/organization-route";
import { cn } from "@/lib/utils";
import type {
  Invitation,
  Organization,
  OrganizationRole,
} from "@/lib/contracts";

export type OrganizationEntry =
  | {
      kind: "member";
      organization: Organization;
      actor: string;
      role: OrganizationRole;
      can_create: boolean;
      can_create_private: boolean;
      can_create_public: boolean;
    }
  | { kind: "invitation"; organization: Organization; invitation: Invitation };

export function useOrganizationDirectory(after?: string, enabled = true) {
  return useRequest<{ items: OrganizationEntry[]; next: string | null }>(
    enabled
      ? `/api/organizations${after ? `?${new URLSearchParams({ after })}` : ""}`
      : null,
  );
}

export const organizationInvitationsHref =
  "/-/notifications?tab=organization-invitations";

export function OrganizationDirectory({
  invitations = false,
  title = true,
}: {
  invitations?: boolean;
  title?: boolean;
}) {
  const [after, setAfter] = useState<string>();
  const state = useOrganizationDirectory(after);
  const label = invitations ? "Organization invitations" : "Your organizations";
  return (
    <section
      className={cn(
        "grid w-full gap-4",
        title && !invitations && "mx-auto max-w-4xl",
      )}
      aria-label={label}
    >
      {invitations ? (
        <h2 className="text-lg font-semibold">{label}</h2>
      ) : (
        title && (
          <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-4">
            <h1 className="m-0 text-2xl font-semibold tracking-tight">
              {label}
            </h1>
            <NewOrganizationButton />
          </div>
        )
      )}
      <Result state={state} timingLabel={label}>
        {({ items, next }) => {
          const visible = invitations
            ? items.filter((item) => item.kind === "invitation")
            : items;
          return (
            <>
              {visible.length ? (
                <ul className="divide-y rounded-lg border" aria-label={label}>
                  {visible.map((entry) => {
                    const { organization } = entry;
                    const pending = entry.kind === "invitation";
                    const owner =
                      entry.kind === "member" && entry.role === "owner";
                    return (
                      <li
                        key={organization.id}
                        className="flex flex-wrap items-center gap-3 px-4 py-3"
                      >
                        <OrganizationAvatar id={organization.id} size={32} />
                        <div className="min-w-0 flex-1 basis-44">
                          <Link
                            href={organizationHref(
                              organization,
                              pending ? "/invitation" : "",
                            )}
                            className="font-semibold wrap-anywhere"
                          >
                            {organization.name}
                          </Link>
                          {pending ? (
                            <p className="m-0 text-sm text-muted-foreground">
                              Expires{" "}
                              {timestamp(entry.invitation.expires_at_ms)}
                            </p>
                          ) : (
                            organization.description && (
                              <p
                                dir="auto"
                                className="m-0 line-clamp-2 text-sm wrap-anywhere text-muted-foreground"
                              >
                                {organization.description}
                              </p>
                            )
                          )}
                        </div>
                        {pending ? (
                          <Badge variant="secondary">Invitation pending</Badge>
                        ) : (
                          <span className="text-sm text-muted-foreground">
                            {owner ? "Owner" : "Member"}
                          </span>
                        )}
                        {owner && (
                          <Button asChild variant="outline" size="sm">
                            <Link
                              href={organizationHref(
                                organization,
                                "?tab=settings",
                              )}
                              aria-label={`Settings for ${organization.name}`}
                            >
                              Settings
                            </Link>
                          </Button>
                        )}
                      </li>
                    );
                  })}
                </ul>
              ) : (
                <div
                  role="status"
                  className="grid justify-items-center gap-3 rounded-lg border p-8 text-center text-sm text-muted-foreground"
                >
                  <p className="m-0">
                    {next || after
                      ? "No matching organizations on this page."
                      : invitations
                        ? "You have no pending organization invitations."
                        : "You don’t belong to any organizations yet. Create one, or accept an invitation to join one."}
                  </p>
                  {!invitations && !next && !after && <NewOrganizationButton />}
                </div>
              )}
              {(after || next) && (
                <div className="flex flex-wrap gap-2">
                  {after && (
                    <Button
                      variant="outline"
                      onClick={() => setAfter(undefined)}
                    >
                      First page
                    </Button>
                  )}
                  {next && (
                    <Button variant="outline" onClick={() => setAfter(next)}>
                      Next page
                    </Button>
                  )}
                </div>
              )}
            </>
          );
        }}
      </Result>
    </section>
  );
}

export function NewOrganizationButton() {
  return (
    <Button asChild size="sm">
      <Link href="/-/organizations/new">
        <PlusIcon aria-hidden="true" />
        New organization
      </Link>
    </Button>
  );
}
