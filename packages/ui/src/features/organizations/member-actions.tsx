import { useRef, useState } from "react";
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

type Action = "removal" | "conversion";

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
  onFinished: (action: Action) => void;
  onReload: () => void;
}) {
  const trigger = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const [action, setAction] = useState<Action>("removal");
  const mutation = useMutation(csrf, null);
  const removing = action === "removal";
  function review(next: Action) {
    mutation.reset();
    setAction(next);
    setOpen(true);
  }
  async function save() {
    const result = await mutation.run<{
      removed?: number;
      converted?: number;
    }>(`${endpoint}/members/${action}`, "POST", {
      members: members.map(({ actor, version }) => ({ actor, version })),
    });
    if ((removing ? result?.removed : result?.converted) === members.length) {
      setOpen(false);
      onFinished(action);
    }
  }

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button ref={trigger} variant="outline">
            {members.length} selected <ChevronDownIcon />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          className="w-max max-w-[calc(100vw-2rem)]"
          onCloseAutoFocus={(event) => {
            if (open) event.preventDefault();
          }}
        >
          <DropdownMenuItem onSelect={() => review("conversion")}>
            <ArrowRightLeftIcon />
            Convert to outside collaborators
          </DropdownMenuItem>
          <DropdownMenuItem
            variant="destructive"
            onSelect={() => review("removal")}
          >
            <UserMinusIcon />
            Remove from organization
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (!mutation.pending) setOpen(next);
        }}
      >
        <DialogContent
          onCloseAutoFocus={(event) => {
            if (trigger.current) {
              event.preventDefault();
              trigger.current.focus();
            }
          }}
        >
          <DialogHeader>
            <DialogTitle>
              {removing
                ? "Remove from organization"
                : "Convert to outside collaborators"}
            </DialogTitle>
            <DialogDescription className="wrap-anywhere">
              {removing
                ? "Selected members will lose organization and team access and direct repository grants. Their personal private forks of organization repositories will be retired, and pending invitations they sent will be cancelled."
                : "Selected members will leave the organization and its teams. Crab keeps their direct and team-derived repository roles as direct grants. Organization ownership and baseline access will end, and pending invitations they sent will be cancelled."}
            </DialogDescription>
          </DialogHeader>
          <ul className="max-h-48 divide-y overflow-y-auto rounded-md border">
            {members.map((member) => (
              <li key={member.actor} className="grid gap-1 px-3 py-2">
                <span className="wrap-anywhere font-medium">
                  {member.profile?.name ?? "Member without a public profile"}
                </span>
                <span className="wrap-anywhere text-sm text-muted-foreground">
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
                    : "Remove members"
                  : mutation.pending
                    ? "Converting…"
                    : "Convert to outside collaborators"}
              </Button>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
