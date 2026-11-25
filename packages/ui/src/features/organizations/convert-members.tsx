import { useState } from "react";
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
  DialogTrigger,
} from "@/components/ui/dialog";
import type { OrganizationMembership } from "@/lib/contracts";
import { MutationError } from "./feedback";

export function ConvertMembers({
  endpoint,
  csrf,
  members,
  onConverted,
  onReload,
}: {
  endpoint: string;
  csrf: string;
  members: OrganizationMembership[];
  onConverted: () => void;
  onReload: () => void;
}) {
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf, null);
  async function save() {
    const result = await mutation.run<{ converted: number }>(endpoint, "POST", {
      members: members.map(({ actor, version }) => ({ actor, version })),
    });
    if (result?.converted === members.length) {
      setOpen(false);
      onConverted();
    }
  }
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (mutation.pending) return;
        mutation.reset();
        setOpen(value);
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline">Convert to outside collaborators</Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Convert to outside collaborators</DialogTitle>
          <DialogDescription className="wrap-anywhere">
            Selected members will leave the organization and its teams. Crab
            keeps their direct and team-derived repository roles as direct
            grants. Organization ownership and baseline access will end.
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
            <Button onClick={() => void save()} disabled={mutation.pending}>
              {mutation.pending
                ? "Converting…"
                : "Convert to outside collaborators"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
