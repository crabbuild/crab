import { controlFocusOwner } from "@/lib/utils";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogFooter,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { applicationDialogs } from "@/lib/application-dialogs";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";

export function ApplicationDialogs() {
  const request = useSyncExternalStore(
    applicationDialogs.subscribe,
    applicationDialogs.current,
  );
  const [value, setValue] = useState("");
  const owner = useRef<HTMLElement | null>(null);
  if (request && !owner.current) owner.current = controlFocusOwner();
  const restoreOwner = (event: Event) => {
    // A queued dialog is one interaction. Closing an earlier request must not
    // steal focus from the next request or lose the original calling control.
    event.preventDefault();
    if (!applicationDialogs.current()) {
      owner.current?.focus();
      owner.current = null;
    }
  };
  useLayoutEffect(() => {
    setValue(request?.initialValue ?? "");
  }, [request]);
  useEffect(() => () => applicationDialogs.cancel(), []);
  if (!request) return null;
  if (request.kind === "confirm")
    return (
      <AlertDialog
        key={request.id}
        open
        onOpenChange={(open) => {
          if (!open) applicationDialogs.complete(request, false);
        }}
      >
        <AlertDialogContent
          onCloseAutoFocus={restoreOwner}
          aria-describedby={undefined}
        >
          <AlertDialogTitle className="text-sm">
            {request.message}
          </AlertDialogTitle>
          <AlertDialogFooter>
            <AlertDialogCancel
              onClick={() => applicationDialogs.complete(request, false)}
            >
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              onClick={() => applicationDialogs.complete(request, true)}
            >
              Confirm
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    );
  return (
    <Dialog
      key={request.id}
      open
      onOpenChange={(open) => {
        if (!open) applicationDialogs.complete(request, null);
      }}
    >
      <DialogContent
        onCloseAutoFocus={restoreOwner}
        aria-describedby={undefined}
        showCloseButton={false}
      >
        <DialogTitle className="text-sm">{request.message}</DialogTitle>
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            applicationDialogs.complete(request, value);
          }}
        >
          <Input
            autoFocus
            aria-label={request.message}
            value={value}
            onChange={(event) => setValue(event.target.value)}
            onFocus={(event) => event.currentTarget.select()}
          />
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              onClick={() => applicationDialogs.complete(request, null)}
            >
              Cancel
            </Button>
            <Button type="submit" variant="default">
              OK
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
