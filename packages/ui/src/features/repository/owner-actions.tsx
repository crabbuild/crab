import { useState } from "react";
import { RepoIcon } from "@primer/octicons-react";
import { ChevronDownIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Link } from "@/components/navigation/link";
import { GitImport } from "@/features/imports";
import {
  OrganizationChoices,
  type OrganizationMembershipEntry,
} from "@/features/organizations/choices";
import { navigate } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export function RepositoryOwnerActions({
  account,
  csrf,
  personal,
}: {
  account: string;
  csrf: string;
  personal: boolean;
}) {
  const [owner, setOwner] = useState<OrganizationMembershipEntry>();
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
            <Button
              variant="outline"
              className="justify-start"
              onClick={() => {
                setOwner(undefined);
                setOpen(false);
              }}
            >
              Personal
            </Button>
            {open && (
              <OrganizationChoices
                eligible={(entry) => entry.can_create || entry.role === "owner"}
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
        <Button asChild>
          <Link
            href={
              organization
                ? `/-/new?${new URLSearchParams({ organization: organization.id })}`
                : "/-/new"
            }
          >
            <RepoIcon data-icon="inline-start" />
            New repository
          </Link>
        </Button>
      )}
      {(!selected || selected.can_create) && (
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
