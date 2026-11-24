import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { useRequest } from "@/lib/api";

type Policy = {
  version: number;
  member_creation: boolean;
  baseline: "read" | "write" | null;
};

export function RepositoryPolicySettings({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const path = `/api/organizations/${organization}/repository-policy`;
  const state = useRequest<Policy>(path);
  return (
    <section
      aria-labelledby="organization-repository-policy"
      className="grid max-w-2xl gap-4 border-t pt-6"
    >
      <h2 id="organization-repository-policy" className="text-lg font-semibold">
        Repository policy
      </h2>
      <Result state={state} timingLabel="Repository policy">
        {(data) => (
          <PolicyForm
            key={data.version}
            initial={data}
            path={path}
            csrf={csrf}
            refresh={state.retry}
          />
        )}
      </Result>
    </section>
  );
}

function PolicyForm({
  initial,
  path,
  csrf,
  refresh,
}: {
  initial: Policy;
  path: string;
  csrf: string;
  refresh: () => void;
}) {
  const [saved, setSaved] = useState(initial);
  const [creation, setCreation] = useState(initial.member_creation);
  const [baseline, setBaseline] = useState(initial.baseline);
  const [notice, setNotice] = useState(false);
  const mutation = useMutation(csrf, null);
  const changed =
    creation !== saved.member_creation || baseline !== saved.baseline;
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (!changed || mutation.conflict) return;
        const result = await mutation.run<Policy>(path, "PUT", {
          version: saved.version,
          member_creation: creation,
          baseline,
        });
        if (result) {
          setSaved(result);
          setNotice(true);
        }
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="organization-repo-creation">
            Who can create repositories
          </FieldLabel>
          <FieldSelect
            id="organization-repo-creation"
            label="Who can create repositories"
            value={creation ? "members" : "owners"}
            options={{
              owners: "Organization owners",
              members: "All organization members",
            }}
            disabled={mutation.pending}
            onValueChange={(value) => {
              setCreation(value === "members");
              setNotice(false);
            }}
          />
          <p className="text-sm text-muted-foreground">
            Members receive Admin on repositories they create. Importing into
            the organization requires an owner.
          </p>
        </Field>
        <Field>
          <FieldLabel htmlFor="organization-repo-baseline">
            Baseline member access
          </FieldLabel>
          <FieldSelect
            id="organization-repo-baseline"
            label="Baseline member access"
            value={baseline ?? "none"}
            options={{ none: "None", read: "Read", write: "Write" }}
            disabled={mutation.pending}
            onValueChange={(value) => {
              setBaseline(value === "read" || value === "write" ? value : null);
              setNotice(false);
            }}
          />
          <p className="text-sm text-muted-foreground">
            Applies to current members across this organization’s repositories.
            Higher repository and team grants still apply.
          </p>
        </Field>
        {notice && <p role="status">Repository policy saved.</p>}
        {mutation.error && (
          <p role="alert">
            {mutation.conflict
              ? "Another owner changed this policy. Reload the current settings before editing."
              : mutation.error}
          </p>
        )}
        <div className="flex flex-wrap gap-2">
          <Button
            type="submit"
            disabled={!changed || mutation.pending || mutation.conflict}
          >
            Save repository policy
          </Button>
          {mutation.conflict && (
            <Button variant="outline" onClick={refresh}>
              Reload policy
            </Button>
          )}
        </div>
      </FieldGroup>
    </form>
  );
}
