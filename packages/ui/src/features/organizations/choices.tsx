import { Building2Icon, CheckIcon } from "lucide-react";
import { useState } from "react";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { OrganizationAvatar } from "@/components/ui/user-avatar";
import { cn } from "@/lib/utils";
import { useOrganizationDirectory, type OrganizationEntry } from "./directory";

export type OrganizationMembershipEntry = Extract<
  OrganizationEntry,
  { kind: "member" }
>;

function visibilityRestrictionLabel(entry: OrganizationMembershipEntry) {
  if (entry.can_create_public && !entry.can_create_private) {
    return "Public repositories only";
  }
  if (entry.can_create_private && !entry.can_create_public) {
    return "Private repositories only";
  }
  return undefined;
}

export function OrganizationChoices({
  eligible,
  select,
  selectedId,
}: {
  eligible: (entry: OrganizationMembershipEntry) => boolean;
  select: (entry: OrganizationMembershipEntry) => void;
  selectedId?: string;
}) {
  const [after, setAfter] = useState<string>();
  const state = useOrganizationDirectory(after);
  return (
    <Result state={state} timingLabel="Eligible organizations">
      {({ items, next }) => {
        const choices = items.filter(
          (entry): entry is OrganizationMembershipEntry =>
            entry.kind === "member" && eligible(entry),
        );
        return (
          <>
            {choices.length ? (
              <ul
                aria-label="Eligible organizations"
                className="max-h-[35dvh] divide-y overflow-auto rounded-lg border bg-card"
              >
                {choices.map((entry) => {
                  const isSelected = selectedId === entry.organization.id;
                  const subtitle =
                    entry.organization.description?.trim() ||
                    visibilityRestrictionLabel(entry);
                  return (
                    <li key={entry.organization.id}>
                      <Button
                        type="button"
                        variant="ghost"
                        aria-pressed={selectedId ? isSelected : undefined}
                        className={cn(
                          "h-auto w-full items-center justify-start gap-3 rounded-none px-3 py-2.5 text-start whitespace-normal wrap-anywhere",
                          isSelected && "bg-accent/60",
                        )}
                        onClick={() => select(entry)}
                      >
                        <OrganizationAvatar
                          id={entry.organization.id}
                          avatar={entry.organization.avatar}
                          size={32}
                          className="shrink-0"
                        />
                        <span className="grid min-w-0 flex-1 gap-0.5">
                          <span className="flex flex-wrap items-center gap-1.5">
                            <span className="font-medium text-foreground">
                              {entry.organization.name}
                            </span>
                            <Badge
                              variant="secondary"
                              className="px-1.5 py-0 text-[11px] font-medium"
                            >
                              {entry.role === "owner" ? "Owner" : "Member"}
                            </Badge>
                          </span>
                          {(entry.organization.handle || subtitle) && (
                            <span className="truncate text-xs text-muted-foreground">
                              {entry.organization.handle && (
                                <span>@{entry.organization.handle}</span>
                              )}
                              {entry.organization.handle && subtitle && " · "}
                              {subtitle}
                            </span>
                          )}
                        </span>
                        {isSelected && (
                          <CheckIcon
                            className="ms-auto size-4 shrink-0 text-primary"
                            aria-hidden="true"
                          />
                        )}
                      </Button>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <div
                className="rounded-lg border border-dashed bg-muted/20 px-4 py-5 text-center text-sm text-muted-foreground"
                role="status"
              >
                <Building2Icon
                  className="mx-auto mb-1.5 size-5 text-muted-foreground"
                  aria-hidden="true"
                />
                <p className="font-medium text-foreground">
                  No eligible organizations on this page.
                </p>
                <p className="mt-1 text-xs">
                  Organizations where you can create repositories will appear
                  here.
                </p>
              </div>
            )}
            {(after || next) && (
              <div className="flex flex-wrap gap-2">
                {after && (
                  <Button variant="outline" onClick={() => setAfter(undefined)}>
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
  );
}
