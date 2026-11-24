import { useState } from "react";
import { EditDiscussion } from "@/components/discussion/conversation/edit";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useOverlayState } from "@/components/ui/use-overlay-state";
import { endpoint, type Repository } from "@/lib/api";
import type { PullRequest } from "./model";

export function EditPullRequest({
  repo,
  pull,
  csrf,
  accept,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  accept: (pull: PullRequest) => void;
}) {
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf, repo);
  const canEdit = pull.can_edit && !repo.archived && !pull.merge_pending;
  const overlay = useOverlayState({
    open,
    onOpenChange(next) {
      if (mutation.pending) return;
      mutation.reset();
      setOpen(next);
    },
  });

  if (!open && !canEdit) return null;
  return (
    <Dialog {...overlay}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" disabled={!canEdit}>
          Edit pull request
        </Button>
      </DialogTrigger>
      <DialogContent
        aria-describedby={undefined}
        className="max-h-[85dvh] overflow-y-auto sm:max-w-3xl"
        showCloseButton={!mutation.pending}
      >
        <DialogHeader>
          <DialogTitle>Edit pull request</DialogTitle>
        </DialogHeader>
        {overlay.open && (
          <EditDiscussion
            source={pull}
            url={endpoint(repo, `pulls/${pull.number}`)}
            repositoryId={repo.id}
            id="edit-pull-description"
            bodyLabel="Description"
            canEdit={canEdit}
            mutation={mutation}
            onCancel={() => {
              mutation.reset();
              setOpen(false);
            }}
            onSaved={(saved) => {
              accept(saved);
              setOpen(false);
            }}
            onReviewed={accept}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}
