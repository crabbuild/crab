import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";

type Policy = {
  version: number;
  member_creation: boolean;
  member_transfer: boolean;
  member_deletion: boolean;
  private_forks: boolean;
  member_visibility: boolean;
  baseline: "read" | "write" | null;
  default_branch: string;
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
      className="grid max-w-2xl gap-4"
    >
      <h2 id="organization-repository-policy" className="text-lg font-semibold">
        Member privileges
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
  const [transfer, setTransfer] = useState(initial.member_transfer);
  const [deletion, setDeletion] = useState(initial.member_deletion);
  const [privateForks, setPrivateForks] = useState(initial.private_forks);
  const [memberVisibility, setMemberVisibility] = useState(
    initial.member_visibility,
  );
  const [baseline, setBaseline] = useState(initial.baseline);
  const [defaultBranch, setDefaultBranch] = useState(initial.default_branch);
  const [notice, setNotice] = useState(false);
  const mutation = useMutation(csrf, null);
  const changed =
    creation !== saved.member_creation ||
    transfer !== saved.member_transfer ||
    deletion !== saved.member_deletion ||
    privateForks !== saved.private_forks ||
    memberVisibility !== saved.member_visibility ||
    baseline !== saved.baseline ||
    defaultBranch !== saved.default_branch;
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (!changed || mutation.conflict) return;
        const result = await mutation.run<Policy>(path, "PUT", {
          version: saved.version,
          member_creation: creation,
          member_transfer: transfer,
          member_deletion: deletion,
          private_forks: privateForks,
          member_visibility: memberVisibility,
          baseline,
          default_branch: defaultBranch,
        });
        if (result) {
          setSaved(result);
          setNotice(true);
        }
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="organization-default-branch">
            Default branch for new repositories
          </FieldLabel>
          <Input
            id="organization-default-branch"
            aria-describedby="organization-default-branch-help"
            value={defaultBranch}
            required
            spellCheck={false}
            autoComplete="off"
            disabled={mutation.pending}
            onChange={(event) => {
              setDefaultBranch(event.target.value);
              setNotice(false);
            }}
          />
          <FieldDescription id="organization-default-branch-help">
            New repositories start with this branch name. Imports follow the
            source repository.
          </FieldDescription>
        </Field>
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
          <FieldLabel htmlFor="organization-repo-transfer">
            Who can transfer repositories
          </FieldLabel>
          <FieldSelect
            id="organization-repo-transfer"
            label="Who can transfer repositories"
            value={transfer ? "admins" : "owners"}
            options={{
              owners: "Organization owners",
              admins: "Owners and members with repository Admin access",
            }}
            disabled={mutation.pending}
            onValueChange={(value) => {
              setTransfer(value === "admins");
              setNotice(false);
            }}
          />
          <p className="text-sm text-muted-foreground">
            Restricting transfers also blocks pending offers from non-owners.
          </p>
        </Field>
        <Field>
          <FieldLabel htmlFor="organization-repo-deletion">
            Who can delete repositories
          </FieldLabel>
          <FieldSelect
            id="organization-repo-deletion"
            label="Who can delete repositories"
            value={deletion ? "admins" : "owners"}
            options={{
              owners: "Organization owners",
              admins: "Owners and members with repository Admin access",
            }}
            disabled={mutation.pending}
            onValueChange={(value) => {
              setDeletion(value === "admins");
              setNotice(false);
            }}
          />
          <FieldDescription>
            Organization owners can restore eligible repositories for 90 days.
          </FieldDescription>
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
        <Field orientation="horizontal">
          <Checkbox
            id="organization-member-visibility"
            aria-describedby="organization-member-visibility-help"
            checked={memberVisibility}
            disabled={mutation.pending}
            onCheckedChange={(value) => {
              setMemberVisibility(value === true);
              setNotice(false);
            }}
          />
          <FieldContent>
            <FieldLabel htmlFor="organization-member-visibility">
              Allow repository admins to change visibility
            </FieldLabel>
            <FieldDescription id="organization-member-visibility-help">
              Organization owners can always change repository visibility.
            </FieldDescription>
          </FieldContent>
        </Field>
        <Field orientation="horizontal">
          <Checkbox
            id="organization-private-forks"
            aria-describedby="organization-private-forks-help"
            checked={privateForks}
            disabled={mutation.pending}
            onCheckedChange={(value) => {
              setPrivateForks(value === true);
              setNotice(false);
            }}
          />
          <FieldContent>
            <FieldLabel htmlFor="organization-private-forks">
              Allow forking of private repositories
            </FieldLabel>
            <FieldDescription id="organization-private-forks-help">
              Repository admins can turn off new forks for individual
              repositories.
            </FieldDescription>
          </FieldContent>
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
