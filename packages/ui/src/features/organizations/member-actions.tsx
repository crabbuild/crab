import { useRef, useState, type RefObject } from "react";
import {
  ArrowRightLeftIcon,
  ChevronDownIcon,
  UserMinusIcon,
} from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { OrganizationMembership } from "@/lib/contracts";
import { MutationError } from "./feedback";

export type MemberChange = "removal" | "conversion";

export function MemberActions({
  endpoint,
  csrf,
  members,
  onFinished,
  onReload,
}: {
  endpoint: string;
  csrf: string;
  members: OrganizationMembership[];
  onFinished: (action: MemberChange) => void;
  onReload: () => void;
}) {
  const trigger = useRef<HTMLButtonElement>(null);
  const [action, setAction] = useState<MemberChange>();
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button ref={trigger} variant="outline" size="sm">
            {members.length} selected <ChevronDownIcon />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          className="w-max max-w-[calc(100vw-2rem)]"
          onCloseAutoFocus={(event) => {
            if (action) event.preventDefault();
          }}
        >
          <DropdownMenuItem onSelect={() => setAction("conversion")}>
            <ArrowRightLeftIcon />
            Convert to outside collaborators
          </DropdownMenuItem>
          <DropdownMenuItem
            variant="destructive"
            onSelect={() => setAction("removal")}
          >
            <UserMinusIcon />
            Remove from organization
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <MemberChangeDialog
        endpoint={endpoint}
        csrf={csrf}
        members={members}
        action={action}
        returnFocus={trigger}
        onClose={() => setAction(undefined)}
        onFinished={onFinished}
        onReload={onReload}
      />
    </>
  );
}

// Row menus and the selection menu review removal and conversion through the
// same bulk endpoints, so one member and many members share one guard path.
export function MemberChangeDialog({
  endpoint,
  csrf,
  members,
  action,
  returnFocus,
  onClose,
  onFinished,
  onReload,
}: {
  endpoint: string;
  csrf: string;
  members: OrganizationMembership[];
  action?: MemberChange;
  returnFocus: RefObject<HTMLButtonElement | null>;
  onClose: () => void;
  onFinished: (action: MemberChange) => void;
  onReload: () => void;
}) {
  const mutation = useMutation(csrf, null);
  const [shown, setShown] = useState<MemberChange>("removal");
  if (action && action !== shown) setShown(action);
  const removing = shown === "removal";
  const single = members.length === 1;
  async function save() {
    const result = await mutation.run<{
      removed?: number;
      converted?: number;
    }>(`${endpoint}/members/${shown}`, "POST", {
      members: members.map(({ actor, version }) => ({ actor, version })),
    });
    if ((removing ? result?.removed : result?.converted) === members.length) {
      onClose();
      onFinished(shown);
    }
  }
  return (
    <Dialog
      open={action !== undefined}
      onOpenChange={(next) => {
        if (next || mutation.pending) return;
        mutation.reset();
        onClose();
      }}
    >
      <DialogContent
        onCloseAutoFocus={(event) => {
          if (returnFocus.current) {
            event.preventDefault();
            returnFocus.current.focus();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>
            {removing
              ? "Remove from organization"
              : single
                ? "Convert to outside collaborator"
                : "Convert to outside collaborators"}
          </DialogTitle>
          <DialogDescription className="wrap-anywhere">
            {removing
              ? `${single ? "This member" : "Selected members"} will lose organization and team access and direct repository grants. Their personal private forks of organization repositories will be retired, and pending invitations they sent will be cancelled.`
              : `${single ? "This member" : "Selected members"} will leave the organization and its teams. Crab keeps their direct and team-derived repository roles as direct grants. Organization ownership and baseline access will end, and pending invitations they sent will be cancelled.`}
          </DialogDescription>
        </DialogHeader>
        <ul className="max-h-48 divide-y overflow-y-auto rounded-md border">
          {members.map((member) => (
            <li key={member.actor} className="grid gap-1 px-3 py-2">
              <span className="font-medium wrap-anywhere">
                {member.profile?.name ?? "Member without a public profile"}
              </span>
              <span className="text-sm wrap-anywhere text-muted-foreground">
                {member.profile ? `@${member.profile.handle}` : member.actor}
                {member.role === "owner" && " · Owner"}
              </span>
            </li>
          ))}
        </ul>
        <MutationError error={mutation.error} />
        <DialogFooter>
          <DialogClose asChild>
            <Button variant="outline" disabled={mutation.pending}>
              Cancel
            </Button>
          </DialogClose>
          {mutation.status === 409 || mutation.status === 404 ? (
            <Button onClick={onReload}>Reload members</Button>
          ) : (
            <Button
              variant={removing ? "destructive" : "default"}
              onClick={() => void save()}
              disabled={mutation.pending}
            >
              {removing
                ? mutation.pending
                  ? "Removing…"
                  : single
                    ? "Remove member"
                    : "Remove members"
                : mutation.pending
                  ? "Converting…"
                  : single
                    ? "Convert to outside collaborator"
                    : "Convert to outside collaborators"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
