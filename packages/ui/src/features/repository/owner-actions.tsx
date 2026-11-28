import { useState } from "react";
import { PersonIcon, RepoIcon } from "@primer/octicons-react";
import { CheckIcon, ChevronDownIcon, InfoIcon } from "lucide-react";
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
import { OrganizationAvatar } from "@/components/ui/user-avatar";
import { Link } from "@/components/navigation/link";
import { GitImport } from "@/features/imports";
import {
  OrganizationChoices,
  type OrganizationMembershipEntry,
} from "@/features/organizations/choices";
import { navigate } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";

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
  const personalHandle =
    account && !account.startsWith("user-") ? account : undefined;
  const onCreated = (repo: { owner: string; name: string }) =>
    navigate(repoHref(repo));
  return (
    <div className="flex min-w-0 flex-wrap items-start gap-2">
      {!personal && (
        <Dialog open={open} onOpenChange={setOpen}>
          <DialogTrigger asChild>
            <Button variant="outline" className="max-w-full">
              {organization ? (
                <OrganizationAvatar
                  id={organization.id}
                  avatar={organization.avatar}
                  size={18}
                  className="shrink-0"
                />
              ) : (
                <PersonIcon
                  size={16}
                  className="shrink-0 text-muted-foreground"
                  aria-hidden="true"
                />
              )}
              <span className="truncate">
                Owner · {organization?.name ?? "Personal"}
              </span>
              <ChevronDownIcon data-icon="inline-end" />
            </Button>
          </DialogTrigger>
          <DialogContent className="gap-4 sm:max-w-md">
            <DialogHeader>
              <DialogTitle>Choose repository owner</DialogTitle>
              <DialogDescription>
                Select where newly created or imported repositories will live.
              </DialogDescription>
            </DialogHeader>
            <div className="grid gap-1.5">
              <p className="m-0 text-xs font-medium tracking-wide text-muted-foreground uppercase">
                Personal account
              </p>
              <Button
                type="button"
                variant="ghost"
                aria-pressed={!organization}
                className={cn(
                  "h-auto w-full items-center justify-start gap-3 rounded-lg border px-3 py-2.5 text-start whitespace-normal wrap-anywhere",
                  !organization && "border-primary/50 bg-accent/50",
                )}
                onClick={() => {
                  setOwner(undefined);
                  setOpen(false);
                }}
              >
                <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-primary/10 text-primary">
                  <PersonIcon size={16} aria-hidden="true" />
                </span>
                <span className="grid min-w-0 flex-1 gap-0.5">
                  <span className="flex flex-wrap items-center gap-1.5">
                    <span className="font-medium text-foreground">
                      Personal
                    </span>
                    {personalHandle && (
                      <Badge
                        variant="secondary"
                        className="px-1.5 py-0 text-[11px] font-medium"
                      >
                        @{personalHandle}
                      </Badge>
                    )}
                  </span>
                  <span className="text-xs text-muted-foreground">
                    Create repositories under your personal account
                  </span>
                </span>
                {!organization && (
                  <CheckIcon
                    className="ms-auto size-4 shrink-0 text-primary"
                    aria-hidden="true"
                  />
                )}
              </Button>
            </div>
            <div className="grid gap-1.5">
              <p className="m-0 text-xs font-medium tracking-wide text-muted-foreground uppercase">
                Organizations
              </p>
              {open && (
                <OrganizationChoices
                  selectedId={organization?.id}
                  eligible={(entry) =>
                    entry.can_create || entry.role === "owner"
                  }
                  select={(entry) => {
                    setOwner(entry);
                    setOpen(false);
                  }}
                />
              )}
            </div>
            <div className="flex items-start gap-2 rounded-lg border bg-muted/40 px-3 py-2.5 text-xs text-muted-foreground">
              <InfoIcon
                className="mt-0.5 size-3.5 shrink-0 text-muted-foreground"
                aria-hidden="true"
              />
              <span>
                Tip: Switching owner updates both{" "}
                <strong className="font-medium text-foreground">
                  New repository
                </strong>{" "}
                and{" "}
                <strong className="font-medium text-foreground">
                  Import from Git
                </strong>
                . Organization policies may restrict repository visibility.
              </span>
            </div>
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
