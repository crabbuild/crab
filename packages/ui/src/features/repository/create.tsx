import { useState } from "react";
import { RepoIcon } from "@primer/octicons-react";
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
import {
  Field,
  FieldError,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";

type Destination = { owner: string; default_branch: string };
type RepositoryAddress = { id: string; owner: string; name: string };
type Submission = { request_id: string; name: string; description: string };

function recover(key: string): Submission | undefined {
  try {
    const data = JSON.parse(
      sessionStorage.getItem(key) ?? "null",
    ) as Partial<Submission> | null;
    if (
      typeof data?.request_id === "string" &&
      typeof data?.name === "string" &&
      typeof data?.description === "string"
    )
      return {
        request_id: data.request_id,
        name: data.name,
        description: data.description,
      };
  } catch {
    // Browsers can disable session storage.
  }
}

function RepositoryForm({
  destination,
  endpoint,
  account,
  csrf,
  onCreated,
}: {
  destination: Destination;
  endpoint: string;
  account?: string;
  csrf: string;
  onCreated: (repository: RepositoryAddress) => void;
}) {
  const key = `crab:repository-creation:${destination.owner}${account ? `:${account}` : ""}`;
  const [attempt, setAttempt] = useState(() => recover(key));
  const [name, setName] = useState(attempt?.name ?? "");
  const [description, setDescription] = useState(attempt?.description ?? "");
  const mutation = useMutation(csrf, null);
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (!name.trim() || mutation.pending) return;
        const fields = { name: name.trim(), description: description.trim() };
        const next =
          attempt?.name === fields.name &&
          attempt.description === fields.description
            ? attempt
            : { ...fields, request_id: crypto.randomUUID() };
        setAttempt(next);
        try {
          sessionStorage.setItem(key, JSON.stringify(next));
        } catch {
          // The form still retains its submission during this browser session.
        }
        const created = await mutation.run<RepositoryAddress>(
          endpoint,
          "POST",
          next,
        );
        if (created) {
          try {
            sessionStorage.removeItem(key);
          } catch {
            // Storage was already unavailable.
          }
          onCreated(created);
        }
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="new-repository-name">Repository name</FieldLabel>
          <Input
            id="new-repository-name"
            required
            autoComplete="off"
            maxLength={100}
            pattern={String.raw`[A-Za-z0-9._\-]+`}
            value={name}
            disabled={mutation.pending}
            onChange={(event) => setName(event.target.value)}
          />
        </Field>
        <Field>
          <FieldLabel htmlFor="new-repository-description">
            Description (optional)
          </FieldLabel>
          <Input
            id="new-repository-description"
            maxLength={350}
            value={description}
            disabled={mutation.pending}
            onChange={(event) => setDescription(event.target.value)}
          />
        </Field>
        <p className="text-sm text-muted-foreground">
          <code>
            {destination.owner}/{name || "repository"}
          </code>{" "}
          · Default branch <code>{destination.default_branch}</code>
        </p>
        {mutation.error && <FieldError>{mutation.error}</FieldError>}
        <div className="flex justify-end">
          <Button type="submit" disabled={!name.trim() || mutation.pending}>
            {mutation.pending ? "Creating…" : "Create repository"}
          </Button>
        </div>
      </FieldGroup>
    </form>
  );
}

export function CreateRepository({
  organization,
  csrf,
  onCreated,
}: {
  organization?: { id: string; name: string; actor: string };
  csrf: string;
  onCreated: (repository: RepositoryAddress) => void;
}) {
  const [open, setOpen] = useState(false);
  const endpoint = organization
    ? `/api/organizations/${organization.id}/repositories/creation`
    : "/api/repos/creation";
  const destination = useRequest<Destination>(open ? endpoint : null);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>
          <RepoIcon data-icon="inline-start" />
          New repository
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create a repository</DialogTitle>
          <DialogDescription>
            {organization
              ? `Create an empty private Git repository owned by ${organization.name}.`
              : "Create an empty private Git repository in your personal namespace."}
          </DialogDescription>
        </DialogHeader>
        <Result state={destination}>
          {(data) => (
            <RepositoryForm
              destination={data}
              endpoint={endpoint}
              account={organization?.actor}
              csrf={csrf}
              onCreated={(repository) => {
                setOpen(false);
                onCreated(repository);
              }}
            />
          )}
        </Result>
      </DialogContent>
    </Dialog>
  );
}
