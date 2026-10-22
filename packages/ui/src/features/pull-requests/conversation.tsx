import { Author } from "@/components/discussion/author";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { ActivityTimeline } from "@/features/activity";
import { Reactions } from "@/components/discussion/reactions";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Result } from "@/components/request/result";
import { useState } from "react";

import { timestamp } from "@/components/discussion/time";
import { MergePanel } from "./merge";
import type { Page, PullComment, PullRequest, PullReview } from "./model";
import { PullTimeline } from "./timeline";

export function PullConversation({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const commentsPath = endpoint(repo, `pulls/${pull.number}/comments`);
  const comments = useRequest<Page<PullComment>>(commentsPath);
  const reviews = useRequest<Page<PullReview>>(
    endpoint(repo, `pulls/${pull.number}/reviews`),
  );
  const [body, setBody] = useState("");
  const [activityEpoch, setActivityEpoch] = useState(0);
  const commentMutation = useMutation(csrf);
  const stateMutation = useMutation(csrf);
  const submission = useSubmission();
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
          target={`pulls/${pull.number}`}
          summary={pull.reactions}
        />
      </article>
      <Result state={comments} timingLabel="Conversation comments">
        {(commentPage) => (
          <Result state={reviews} timingLabel="Pull request reviews">
            {(reviewPage) => (
              <PullTimeline
                repo={repo}
                pull={pull.number}
                csrf={csrf}
                comments={commentPage.items}
                reviews={reviewPage.items}
              />
            )}
          </Result>
        )}
      </Result>
      <ActivityTimeline
        key={`${pull.version}:${activityEpoch}`}
        repo={repo}
        target={`pulls/${pull.number}`}
      />
      <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
      {!repo.archived && (
        <form
          className="discussion-form pull-comment-form"
          onSubmit={async (event) => {
            event.preventDefault();
            const input = { body };
            const created = await commentMutation.run<PullComment>(
              commentsPath,
              "POST",
              { ...input, request_id: submission(input) },
            );
            if (created) {
              setActivityEpoch((value) => value + 1);
              setBody("");
              comments.retry();
            }
          }}
        >
          <h3>Add a comment</h3>
          <Editor
            id="pull-comment"
            label="Comment"
            value={body}
            onChange={setBody}
            disabled={commentMutation.pending}
            required
          />
          <Failure message={commentMutation.error} />
          <div className="discussion-actions">
            {pull.can_manage && (
              <Button
                type="button"
                variant={pull.state === "open" ? "destructive" : "outline"}
                disabled={stateMutation.pending}
                onClick={async () => {
                  const updated = await stateMutation.run<PullRequest>(
                    endpoint(repo, `pulls/${pull.number}`),
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
            )}
            <Button
              type="submit"
              variant="default"
              disabled={commentMutation.pending || !body.trim()}
            >
              {commentMutation.pending ? "Commenting…" : "Comment"}
            </Button>
          </div>
          <Failure message={stateMutation.error} />
        </form>
      )}
    </div>
  );
}
