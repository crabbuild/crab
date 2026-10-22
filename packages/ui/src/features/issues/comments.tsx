import type { ReactNode } from "react";
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
  actions,
}: {
  repo: Repository;
  issue: number;
  version: string;
  csrf: string;
  actions?: (busy: boolean) => ReactNode;
}) {
  const target = "issues/" + issue;
  const comment = useCommentSubmission(repo, target, csrf);
  const href = repoHref(repo, { view: "issues", issue: String(issue) });
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
          !repo.archived && (
            <CommentComposer
              href={href}
              onCreated={posted}
              id="new-comment"
              className="new-comment"
              comment={comment}
              busy={busy}
              actions={actions?.(comment.pending)}
            />
          )
        }
      </Conversation>
    </section>
  );
}
