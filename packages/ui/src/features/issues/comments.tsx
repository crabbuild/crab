import type { Issue } from "./model";
import { IssueStateControls } from "./state";
import { Conversation } from "@/components/discussion/conversation";
import {
  CommentComposer,
  useCommentSubmission,
} from "@/components/discussion/conversation/compose";
import { repoHref } from "@/lib/repository-route";
import type { Repository } from "@/lib/api";
export function Comments({
  repo,
  issue,
  version,
  csrf,
  disabled,
  refreshing,
  onChanged,
}: {
  repo: Repository;
  issue: Issue;
  version: string;
  csrf: string;
  disabled: boolean;
  refreshing: boolean;
  onChanged: () => void;
}) {
  const target = "issues/" + issue.number;
  const comment = useCommentSubmission(repo, target, csrf);
  const href = repoHref(repo, { view: "issues", issue: String(issue.number) });
  return (
    <section className="issue-comments" aria-label="Discussion">
      <Conversation
        repo={repo}
        target={target}
        href={href}
        csrf={csrf}
        revision={version}
        active
      >
        {(busy, posted) =>
          (!repo.archived || comment.body || comment.intent) && (
            <CommentComposer
              href={href}
              onCreated={posted}
              id="new-comment"
              className="new-comment"
              comment={comment}
              busy={busy}
              readOnly={repo.archived}
              onStateChanged={onChanged}
              actions={(change) =>
                issue.can_close &&
                !repo.archived && (
                  <IssueStateControls
                    issue={issue}
                    disabled={
                      disabled ||
                      refreshing ||
                      comment.pending ||
                      Boolean(comment.intent)
                    }
                    pending={comment.pending && comment.intent === "state"}
                    refreshing={refreshing}
                    conflict={comment.conflict}
                    withComment={Boolean(comment.body.trim())}
                    onChange={change}
                    onRefresh={() => {
                      comment.review();
                      onChanged();
                    }}
                  />
                )
              }
            />
          )
        }
      </Conversation>
    </section>
  );
}
