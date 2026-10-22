import { Author } from "@/components/discussion/author";
import { Reactions } from "@/components/discussion/reactions";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Conversation } from "@/components/discussion/conversation";
import {
  CommentComposer,
  type useCommentSubmission,
} from "@/components/discussion/conversation/compose";
import { useReturnFocus } from "@/components/discussion/hooks";
import { timestamp } from "@/components/discussion/time";
import { Button } from "@/components/ui/button";
import { type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { MergePanel } from "../merge";
import type { PullRequest } from "../model";
export function PullConversation({
  repo,
  pull,
  csrf,
  refresh,
  refreshing,
  active,
  revision,
  comment,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
  refreshing: boolean;
  active: boolean;
  revision: string;
  comment: ReturnType<typeof useCommentSubmission>;
}) {
  const stateTrigger = useReturnFocus<HTMLButtonElement>(
    (comment.pending && comment.intent === "state") || refreshing,
  );
  const target = "pulls/" + pull.number;
  const href = repoHref(repo, { view: "pulls", pull: String(pull.number) });
  return (
    <div className="pull-conversation">
      <article className="panel discussion-card">
        <header>
          <strong>
            <Author author={pull.author} />
          </strong>
          <span className="muted">commented {timestamp(pull.created_at)}</span>
        </header>
        <DiscussionMarkdown>{pull.body}</DiscussionMarkdown>
        <Reactions
          repo={repo}
          csrf={csrf}
          target={target}
          summary={pull.reactions}
        />
      </article>
      <Conversation
        repo={repo}
        target={target}
        href={href}
        csrf={csrf}
        revision={revision}
        active={active}
        onChanged={refresh}
      >
        {(busy, posted) => (
          <>
            <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
            {(!repo.archived || comment.body || comment.intent) && (
              <CommentComposer
                href={href}
                onCreated={posted}
                id="pull-comment"
                className="pull-comment-form"
                comment={comment}
                busy={busy || refreshing}
                readOnly={repo.archived}
                onStateChanged={refresh}
                actions={(change) =>
                  pull.can_manage &&
                  !repo.archived && (
                    <Button
                      ref={comment.conflict ? undefined : stateTrigger}
                      variant={
                        pull.state === "open" ? "destructive" : "outline"
                      }
                      disabled={
                        busy ||
                        refreshing ||
                        comment.pending ||
                        Boolean(comment.intent) ||
                        comment.conflict
                      }
                      onClick={() =>
                        change({
                          version: pull.version,
                          state: pull.state === "open" ? "closed" : "open",
                        })
                      }
                    >
                      {comment.body.trim()
                        ? pull.state === "open"
                          ? "Close with comment"
                          : "Reopen with comment"
                        : pull.state === "open"
                          ? "Close pull request"
                          : "Reopen pull request"}
                    </Button>
                  )
                }
              />
            )}
            {comment.conflict && (
              <Button
                ref={stateTrigger}
                variant="outline"
                onClick={() => {
                  comment.review();
                  refresh();
                }}
              >
                Reload pull request
              </Button>
            )}
          </>
        )}
      </Conversation>
    </div>
  );
}
