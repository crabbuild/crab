import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "../../api";
import { useMutation } from "@/components/discussion/mutations";

export function PullReadinessControl({
  repo,
  pull,
  csrf,
  onSaved,
}: {
  repo: Repository;
  pull: {
    number: number;
    version: number;
    draft: boolean;
    can_manage: boolean;
  };
  csrf: string;
  onSaved: () => void;
}) {
  const mutation = useMutation(csrf);
  if (!pull.can_manage || repo.archived) return null;
  return (
    <div
      className={`pull-readiness-control${pull.draft ? " merge-footer" : ""}`}
    >
      <Button
        size="sm"
        variant={pull.draft ? "default" : "ghost"}
        disabled={mutation.pending}
        onClick={async () => {
          const saved = await mutation.run(
            endpoint(repo, `pulls/${pull.number}/readiness`),
            "PUT",
            { version: pull.version, draft: !pull.draft },
          );
          if (saved) onSaved();
        }}
      >
        {mutation.pending
          ? "Saving…"
          : pull.draft
            ? "Mark ready for review"
            : "Convert to draft"}
      </Button>
      {mutation.error && (
        <p className="error" role="alert">
          {mutation.error}
        </p>
      )}
      {mutation.conflict && (
        <Button variant="outline" size="sm" onClick={onSaved}>
          Reload pull request
        </Button>
      )}
    </div>
  );
}
