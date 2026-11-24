import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import { endpoint, type Repository } from "@/lib/api";
import type { IssueType } from "@/lib/contracts";

export function IssueTypeControl({
  repo,
  csrf,
  number,
  version,
  type,
  types,
  refresh,
}: {
  repo: Pick<Repository, "id" | "owner" | "name">;
  csrf: string;
  number: number;
  version: number;
  type: IssueType | null | undefined;
  types: IssueType[];
  refresh: () => void;
}) {
  const selected = type?.id ?? "none";
  const [draft, setDraft] = useState(selected);
  const mutation = useMutation(csrf, repo);
  return (
    <form
      className="grid gap-2"
      onSubmit={async (event) => {
        event.preventDefault();
        const saved = await mutation.run<{ version: number }>(
          endpoint(repo, `issues/${number}/type`),
          "PUT",
          {
            expected_version: version,
            type_id: draft === "none" ? null : draft,
          },
        );
        if (saved) refresh();
      }}
    >
      <h3 className="text-sm font-semibold">Issue type</h3>
      <div className="flex flex-wrap gap-2">
        <FieldSelect
          label="Issue type"
          value={draft}
          options={{
            none: "No type",
            ...Object.fromEntries(types.map((item) => [item.id, item.name])),
          }}
          onValueChange={setDraft}
          disabled={mutation.pending}
        />
        <Button
          type="submit"
          size="sm"
          variant="outline"
          disabled={draft === selected || mutation.pending || mutation.conflict}
        >
          Save issue type
        </Button>
      </div>
      <Failure message={mutation.error} />
      {mutation.conflict && (
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            mutation.reset();
            refresh();
          }}
        >
          Reload issue type
        </Button>
      )}
    </form>
  );
}
