import { Author } from "@/components/discussion/author";
import { Reactions } from "@/components/discussion/reactions";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Conversation } from "@/components/discussion/conversation";
import {
  CommentComposer,
  type useCommentSubmission,
} from "@/components/discussion/conversation/compose";
import { useMutation } from "@/components/discussion/mutations";
import { useReturnFocus } from "@/components/discussion/hooks";
import { Failure } from "@/components/discussion/failure";
import { timestamp } from "@/components/discussion/time";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
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
  const stateMutation = useMutation(csrf);
  const stateTrigger = useReturnFocus<HTMLButtonElement>(
    stateMutation.pending || refreshing,
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
      >
        {(busy, posted) => (
          <>
            <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
            {!repo.archived && (
              <CommentComposer
                href={href}
                onCreated={posted}
                id="pull-comment"
                className="pull-comment-form"
                comment={comment}
                busy={busy || refreshing}
                actions={
                  pull.can_manage && (
                    <Button
                      ref={stateTrigger}
                      variant={
                        pull.state === "open" ? "destructive" : "outline"
                      }
                      disabled={
                        busy ||
                        refreshing ||
                        stateMutation.pending ||
                        stateMutation.conflict
                      }
                      onClick={async () => {
                        const updated = await stateMutation.run<PullRequest>(
                          endpoint(repo, target),
                          "PATCH",
                          {
                            version: pull.version,
                            state: pull.state === "open" ? "closed" : "open",
                          },
                        );
                        if (updated) refresh();
                      }}
                    >
                      {pull.state === "open"
                        ? "Close pull request"
                        : "Reopen pull request"}
                    </Button>
                  )
                }
              />
            )}
            <Failure message={stateMutation.error} />
            {stateMutation.conflict && (
              <Button
                variant="outline"
                onClick={() => {
                  stateMutation.reset();
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
