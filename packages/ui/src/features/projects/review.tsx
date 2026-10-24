import { useState } from "react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { useProjectCommands } from "./commands";
import type { Project } from "./model";
export function ProjectDraftReview({
  repo,
  project,
  onReviewed,
}: {
  repo: Repository;
  project: string;
  onReviewed: (latest: Project) => void;
}) {
  const [open, setOpen] = useState(false);
  const commands = useProjectCommands();
  const resource = useRequest<{ project: Project; can_manage: boolean }>(
    open ? endpoint(repo, `projects/${project}`) : null,
  );
  if (!commands.error || commands.locked) return null;
  return (
    <section className="flex flex-col gap-3" aria-label="Review saved project">
      {!open ? (
        <Button type="button" variant="outline" onClick={() => setOpen(true)}>
          Review saved project
        </Button>
      ) : (
        <Result state={resource}>
          {(data) => (
            <>
              <p>
                Your draft is still in this form. The saved project is{" "}
                <strong>{data.project.definition.name}</strong>, version{" "}
                {data.project.version}
                {data.project.archived ? ", archived" : ""}.
              </p>
              <p className="muted">
                Workflow: {data.project.definition.statuses.join(" · ")}
              </p>
              {data.can_manage && !repo.archived && (
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => {
                    onReviewed(data.project);
                    commands.clearError();
                    setOpen(false);
                  }}
                >
                  Keep my draft against this version
                </Button>
              )}
            </>
          )}
        </Result>
      )}
    </section>
  );
}
