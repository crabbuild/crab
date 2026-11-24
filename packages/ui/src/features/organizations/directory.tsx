import { useState } from "react";
import { Building2Icon } from "lucide-react";
import { timestamp } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useRequest } from "@/lib/api";
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
    }
  | { kind: "invitation"; organization: Organization; invitation: Invitation };

export function useOrganizationDirectory(after?: string, enabled = true) {
  return useRequest<{ items: OrganizationEntry[]; next: string | null }>(
    enabled
      ? `/api/organizations${after ? `?${new URLSearchParams({ after })}` : ""}`
      : null,
  );
}

export function organizationHref(
  organization: Pick<Organization, "id" | "name">,
  section = "",
) {
  const name = organization.name
    .normalize("NFKD")
    .toLowerCase()
    .replace(/\p{Mark}/gu, "")
    .replace(/[^\p{Letter}\p{Number}]+/gu, "-");
  const slug =
    Array.from(name)
      .slice(0, 60)
      .join("")
      .replace(/^-+|-+$/g, "") || "organization";
  return `/-/organizations/${encodeURIComponent(slug)}--${organization.id}${section}`;
}

export const organizationInvitationsHref =
  "/-/notifications?tab=organization-invitations";

export function OrganizationDirectory({
  invitations = false,
}: {
  invitations?: boolean;
}) {
  const [after, setAfter] = useState<string>();
  const state = useOrganizationDirectory(after);
  const title = invitations ? "Organization invitations" : "Your organizations";
  return (
    <section
      className="mx-auto w-full max-w-4xl space-y-4"
      aria-labelledby="organization-directory"
    >
      {invitations ? (
        <h2 id="organization-directory" className="text-lg font-semibold">
          {title}
        </h2>
      ) : (
        <h1
          id="organization-directory"
          className="text-2xl font-semibold tracking-tight"
        >
          {title}
        </h1>
      )}
      <Result state={state} timingLabel={title}>
        {({ items, next }) => {
          const visible = invitations
            ? items.filter((item) => item.kind === "invitation")
            : items;
          return (
            <>
              {visible.length ? (
                <ul className="divide-y rounded-lg border" aria-label={title}>
                  {visible.map((entry) => {
                    const { organization } = entry;
                    const pending = entry.kind === "invitation";
                    return (
                      <li
                        key={organization.id}
                        className="flex flex-wrap items-start gap-3 p-4"
                      >
                        <Building2Icon
                          aria-hidden="true"
                          className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                        />
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
                            <p className="mt-1 text-sm text-muted-foreground">
                              Expires{" "}
                              {timestamp(entry.invitation.expires_at_ms)}
                            </p>
                          ) : (
                            organization.description && (
                              <p
                                dir="auto"
                                className="mt-1 line-clamp-2 text-sm wrap-anywhere text-muted-foreground"
                              >
                                {organization.description}
                              </p>
                            )
                          )}
                        </div>
                        <Badge variant={pending ? "secondary" : "outline"}>
                          {pending
                            ? "Invitation pending"
                            : entry.role === "owner"
                              ? "Owner"
                              : "Member"}
                        </Badge>
                      </li>
                    );
                  })}
                </ul>
              ) : (
                <p
                  role="status"
                  className="rounded-lg border p-6 text-sm text-muted-foreground"
                >
                  {next || after
                    ? "No matching organizations on this page."
                    : invitations
                      ? "You have no pending organization invitations."
                      : "You don’t belong to any organizations yet. Pending invitations will appear here."}
                </p>
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
