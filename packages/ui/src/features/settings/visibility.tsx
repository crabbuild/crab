import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
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
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useRequest, type Repository } from "@/lib/api";

type Setting = { version: number; value: "private" | "public" };
type Status = {
  setting: Setting;
  can_change: boolean;
  private_forks: number;
  public_forks: number;
};

export function VisibilitySettings({
  repo,
  csrf,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  onChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  const endpoint = `/api/repositories/${repo.id}/visibility`;
  const status = useRequest<Status>(open ? endpoint : null);
  return (
    <div className="danger-zone-row">
      <div>
        <strong>Change repository visibility</strong>
        <p>This repository is {repo.visibility ?? "private"}.</p>
      </div>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogTrigger asChild>
          <Button variant="outline" size="sm" disabled={repo.archived}>
            Change visibility
          </Button>
        </DialogTrigger>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Change repository visibility</DialogTitle>
            <DialogDescription>
              Confirm who should be able to view this repository’s code.
            </DialogDescription>
          </DialogHeader>
          <Result state={status} timingLabel="Repository visibility">
            {(data) =>
              data.can_change ? (
                <VisibilityForm
                  repo={repo}
                  csrf={csrf}
                  status={data}
                  endpoint={endpoint}
                  onChanged={() => {
                    setOpen(false);
                    onChanged();
                  }}
                />
              ) : (
                <p className="text-sm text-muted-foreground">
                  Your organization restricts visibility changes to owners.
                </p>
              )
            }
          </Result>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function VisibilityForm({
  repo,
  csrf,
  status,
  endpoint,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  status: Status;
  endpoint: string;
  onChanged: () => void;
}) {
  const [confirmation, setConfirmation] = useState("");
  const mutation = useMutation(csrf, repo);
  const name = `${repo.owner}/${repo.name}`;
  const value = status.setting.value === "public" ? "private" : "public";
  return (
    <form
      className="grid min-w-0 gap-4"
      onSubmit={async (event) => {
        event.preventDefault();
        if (confirmation !== name || mutation.pending) return;
        const result = await mutation.run<Setting>(endpoint, "PUT", {
          value,
          confirmation,
          version: status.setting.version,
        });
        if (result) onChanged();
      }}
    >
      {value === "public" ? (
        <p className="text-sm">
          Anyone will be able to view and download the code and its Git history.
          {repo.organization_id &&
            " Repository property values will also be public."}
          {status.private_forks > 0 &&
            " Existing private forks will become separate private repositories."}
        </p>
      ) : (
        <p className="text-sm">
          Only people with repository access will be able to view and clone it.
          Previously downloaded copies can still be used.
          {status.public_forks > 0 &&
            " Existing forks will stay public in a separate network."}
        </p>
      )}
      <Field>
        <FieldLabel htmlFor="visibility-confirmation" className="wrap-anywhere">
          Type {name} to confirm
        </FieldLabel>
        <Input
          id="visibility-confirmation"
          value={confirmation}
          onChange={(event) => setConfirmation(event.target.value)}
          spellCheck={false}
          autoComplete="off"
        />
      </Field>
      {mutation.error && (
        <p role="alert" className="text-destructive">
          {mutation.error}
        </p>
      )}
      <Button
        type="submit"
        variant="destructive"
        disabled={confirmation !== name || mutation.pending}
      >
        Make repository {value}
      </Button>
    </form>
  );
}
