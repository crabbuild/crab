import type { RefObject } from "react";
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
import { useRequest } from "@/lib/api";
import type { Organization } from "@/lib/contracts";
import { MutationError } from "./feedback";

type Props = {
  organization: Pick<Organization, "id" | "name">;
  actor: string;
  csrf: string;
  onLeft: () => void;
};

export function LeaveOrganizationDialog({
  open,
  returnFocus,
  onClose,
  ...props
}: Props & {
  open: boolean;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <DialogContent
        onCloseAutoFocus={(event) => {
          if (returnFocus.current?.isConnected) {
            event.preventDefault();
            returnFocus.current.focus();
          }
        }}
      >
        {open && <LeaveForm {...props} />}
      </DialogContent>
    </Dialog>
  );
}

// Leaving is the removal an owner performs, naming only yourself.
function LeaveForm({ organization, actor, csrf, onLeft }: Props) {
  const endpoint = `/api/organizations/${organization.id}`;
  const own = useRequest<{ member: { version: number } | null }>(
    `${endpoint}/members/${actor}`,
  );
  const mutation = useMutation(csrf, null);
  const version = own.data?.member?.version;
  async function leave() {
    if (version === undefined) return;
    const result = await mutation.run<{ removed: number }>(
      `${endpoint}/members/removal`,
      "POST",
      { members: [{ actor, version }] },
    );
    if (result?.removed === 1) onLeft();
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle className="wrap-anywhere">
          Leave {organization.name}?
        </DialogTitle>
        <DialogDescription>
          You’ll lose access to this organization, its teams and your direct
          repository grants. Your private forks of its repositories will be
          retired, and invitations you sent will be cancelled.
        </DialogDescription>
      </DialogHeader>
      <MutationError error={mutation.error ?? own.error} />
      <DialogFooter>
        <DialogClose asChild>
          <Button variant="outline" disabled={mutation.pending}>
            Cancel
          </Button>
        </DialogClose>
        {mutation.status === 404 ? (
          <Button onClick={onLeft}>Reload organizations</Button>
        ) : mutation.status === 409 || own.error ? (
          <Button
            onClick={() => {
              mutation.reset();
              own.retry();
            }}
          >
            Try again
          </Button>
        ) : (
          <Button
            variant="destructive"
            disabled={version === undefined || mutation.pending}
            onClick={() => void leave()}
          >
            {mutation.pending ? "Leaving…" : "Leave organization"}
          </Button>
        )}
      </DialogFooter>
    </>
  );
}
