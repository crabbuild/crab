import { useState } from "react";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { useOrganizationDirectory, type OrganizationEntry } from "./directory";

export type OrganizationMembershipEntry = Extract<
  OrganizationEntry,
  { kind: "member" }
>;

export function OrganizationChoices({
  eligible,
  select,
}: {
  eligible: (entry: OrganizationMembershipEntry) => boolean;
  select: (entry: OrganizationMembershipEntry) => void;
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
                className="max-h-[35dvh] divide-y overflow-auto rounded-lg border"
              >
                {choices.map((entry) => (
                  <li key={entry.organization.id}>
                    <Button
                      type="button"
                      variant="ghost"
                      className="h-auto w-full justify-start text-start whitespace-normal wrap-anywhere"
                      onClick={() => select(entry)}
                    >
                      {entry.organization.name}
                    </Button>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-muted-foreground" role="status">
                No eligible organizations on this page.
              </p>
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
