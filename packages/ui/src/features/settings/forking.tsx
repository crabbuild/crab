import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import type { Repository } from "@/lib/api";

type Policy = NonNullable<Repository["forking"]>;
type Setting = Pick<Policy, "version" | "allowed">;

export function ForkingSettings({
  repo,
  policy,
  csrf,
  onChanged,
}: {
  repo: Repository;
  policy: Policy;
  csrf: string;
  onChanged: () => void;
}) {
  const [allowed, setAllowed] = useState(policy.allowed);
  const [saved, setSaved] = useState<Setting>(policy);
  const mutation = useMutation(csrf, repo);
  const readOnly = !policy.organization_allows || repo.archived;
  return (
    <section className="settings-section" aria-labelledby="forking-heading">
      <header>
        <div>
          <h3 id="forking-heading">Forking</h3>
          <p>
            Choose whether new private forks can be created from this
            repository.
          </p>
        </div>
      </header>
      <form
        className="p-4"
        onSubmit={async (event) => {
          event.preventDefault();
          if (
            readOnly ||
            mutation.pending ||
            mutation.conflict ||
            allowed === saved.allowed
          )
            return;
          const result = await mutation.run<Setting>(
            `/api/repositories/${repo.id}/forking`,
            "PUT",
            {
              version: saved.version,
              allowed,
            },
          );
          if (result) {
            setSaved(result);
            onChanged();
          }
        }}
      >
        <FieldGroup>
          <Field orientation="horizontal">
            <Checkbox
              id="repository-allow-forks"
              aria-describedby="repository-allow-forks-help"
              checked={policy.organization_allows && allowed}
              disabled={readOnly || mutation.pending || mutation.conflict}
              onCheckedChange={(value) => setAllowed(value === true)}
            />
            <FieldContent>
              <FieldLabel htmlFor="repository-allow-forks">
                Allow forking
              </FieldLabel>
              <FieldDescription id="repository-allow-forks-help">
                {!policy.organization_allows
                  ? "An organization owner must enable private forks in Member privileges."
                  : repo.archived
                    ? "Unarchive this repository to change its forking setting."
                    : "Changes apply to new forks. Existing forks keep their access."}
              </FieldDescription>
            </FieldContent>
          </Field>
          {mutation.error && (
            <p role="alert" className="text-destructive">
              {mutation.error}
            </p>
          )}
          <div>
            {mutation.conflict ? (
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  setAllowed(saved.allowed);
                  mutation.reset();
                  onChanged();
                }}
              >
                Reload settings
              </Button>
            ) : (
              <Button
                type="submit"
                disabled={
                  readOnly || mutation.pending || allowed === saved.allowed
                }
              >
                Save forking setting
              </Button>
            )}
          </div>
        </FieldGroup>
      </form>
    </section>
  );
}
