import { useState } from "react";
import { ChevronDownIcon } from "lucide-react";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { GitImport } from "@/features/imports";
import {
  useOrganizationDirectory,
  type OrganizationEntry,
} from "@/features/organizations/directory";
import { navigate } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { CreateRepository } from "./create";

type OrganizationOwner = Extract<OrganizationEntry, { kind: "member" }>;

export function RepositoryOwnerActions({
  account,
  csrf,
  personal,
}: {
  account: string;
  csrf: string;
  personal: boolean;
}) {
  const [owner, setOwner] = useState<OrganizationOwner>();
  const [open, setOpen] = useState(false);
  const selected = personal ? undefined : owner;
  const organization = selected?.organization;
  const onCreated = (repo: { owner: string; name: string }) =>
    navigate(repoHref(repo));
  return (
    <div className="flex min-w-0 flex-wrap items-start gap-2">
      {!personal && (
        <Dialog open={open} onOpenChange={setOpen}>
          <DialogTrigger asChild>
            <Button variant="outline" className="max-w-full">
              <span className="truncate">
                Owner · {organization?.name ?? "Personal"}
              </span>
              <ChevronDownIcon data-icon="inline-end" />
            </Button>
          </DialogTrigger>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Choose repository owner</DialogTitle>
              <DialogDescription>
                Select a destination for repository creation or import.
              </DialogDescription>
            </DialogHeader>
            {open && (
              <OwnerChoices
                select={(entry) => {
                  setOwner(entry);
                  setOpen(false);
                }}
              />
            )}
          </DialogContent>
        </Dialog>
      )}
      {(!selected || selected.can_create) && (
        <CreateRepository
          key={`create-${organization?.id ?? "personal"}`}
          organization={
            organization
              ? {
                  id: organization.id,
                  name: organization.name,
                  actor: selected.actor,
                }
              : undefined
          }
          csrf={csrf}
          onCreated={onCreated}
        />
      )}
      {(!selected || selected.role === "owner") && (
        <GitImport
          key={`import-${organization?.id ?? "personal"}`}
          account={selected?.actor ?? account}
          csrf={csrf}
          personal={!organization}
          organization={
            organization
              ? { id: organization.id, name: organization.name }
              : undefined
          }
          onImported={onCreated}
        />
      )}
    </div>
  );
}

function OwnerChoices({
  select,
}: {
  select: (entry?: OrganizationOwner) => void;
}) {
  const [after, setAfter] = useState<string>();
  const state = useOrganizationDirectory(after);
  return (
    <>
      <Button
        variant="outline"
        className="justify-start"
        onClick={() => select()}
      >
        Personal
      </Button>
      <Result state={state} timingLabel="Eligible organizations">
        {({ items, next }) => {
          const eligible = items.filter(
            (entry): entry is OrganizationOwner =>
              entry.kind === "member" &&
              (entry.can_create || entry.role === "owner"),
          );
          return (
            <>
              {eligible.length ? (
                <ul
                  aria-label="Eligible organizations"
                  className="max-h-[35dvh] divide-y overflow-auto rounded-lg border"
                >
                  {eligible.map((entry) => (
                    <li key={entry.organization.id}>
                      <Button
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
    </>
  );
}
