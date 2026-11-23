import { useId, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useRequest } from "@/lib/api";

const path = "/api/notifications/email";

type Preference = {
  configured: boolean;
  address: string | null;
  enabled: boolean;
  version: number;
};

export function EmailNotifications({ csrf }: { csrf: string }) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          Email settings
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Email notifications</DialogTitle>
          <DialogDescription>
            Receive email about inbox activity and personal repository transfer
            offers.
          </DialogDescription>
        </DialogHeader>
        {open && <EmailSettings csrf={csrf} />}
      </DialogContent>
    </Dialog>
  );
}

function EmailSettings({ csrf }: { csrf: string }) {
  const checkboxId = useId();
  const settings = useRequest<Preference>(path);
  const mutation = useMutation(csrf, null);
  const [draft, setDraft] = useState<boolean>();
  const [updated, setUpdated] = useState<Preference>();
  return (
    <Result state={settings} timingLabel="Email settings">
      {(loaded) => {
        const saved = updated ?? loaded;
        if (!saved.configured) {
          return (
            <p className="text-sm">
              Email notifications are unavailable on this server.
            </p>
          );
        }
        if (!saved.address) {
          return (
            <div className="grid gap-3 text-sm">
              <p>Sign in with a verified email address to enable delivery.</p>
              <Button asChild variant="outline" className="w-fit">
                <a href="/auth/login?return_to=%2F-%2Fnotifications">
                  Sign in again
                </a>
              </Button>
            </div>
          );
        }
        const enabled = draft ?? saved.enabled;
        const conflict = mutation.status === 409;
        return (
          <form
            className="grid gap-3"
            onSubmit={async (event) => {
              event.preventDefault();
              const result = await mutation.run<Preference>(path, "PUT", {
                enabled,
                version: saved.version,
              });
              if (result) {
                setUpdated(result);
                setDraft(undefined);
              }
            }}
          >
            <p className="break-all text-sm">
              Verified email <strong>{saved.address}</strong>
            </p>
            <label
              htmlFor={checkboxId}
              className="flex w-fit items-center gap-2 text-sm"
            >
              <Checkbox
                id={checkboxId}
                checked={enabled}
                disabled={mutation.pending || conflict}
                onCheckedChange={(checked) => setDraft(checked === true)}
              />
              Send me notification emails
            </label>
            {mutation.error && <p role="alert">{mutation.error}</p>}
            {updated && draft === undefined && !mutation.error && (
              <p role="status" className="text-sm">
                {saved.enabled
                  ? "Email notifications on"
                  : "Email notifications off"}
              </p>
            )}
            {conflict && (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                onClick={() => {
                  setDraft(undefined);
                  setUpdated(undefined);
                  mutation.reset();
                  settings.retry();
                }}
              >
                Reload preference
              </Button>
            )}
            <Button
              type="submit"
              className="justify-self-end"
              disabled={
                enabled === saved.enabled || mutation.pending || conflict
              }
            >
              {mutation.pending ? "Saving…" : "Save preference"}
            </Button>
          </form>
        );
      }}
    </Result>
  );
}
