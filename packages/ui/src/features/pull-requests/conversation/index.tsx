import { useEffect } from "react";
import { Author } from "@/components/discussion/author";
import { type Repository } from "@/lib/api";
import { ActivityTimeline } from "@/features/activity";
import { Reactions } from "@/components/discussion/reactions";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import {
  HistoryControls,
  useDiscussionHistory,
} from "@/components/discussion/history";
import {
  useContributionFocus,
  useDiscussionPermalink,
} from "@/components/discussion/permalink";
import { Button } from "@/components/ui/button";
import { RequestWidget } from "@/components/request/result";
import { timestamp } from "@/components/discussion/time";
import { MergePanel } from "../merge";
import type {
  PullComment,
  PullRequest,
  PullReview,
  PullReviewPage,
} from "../model";
import { PullCommentForm, type usePullComment } from "./compose";
import { PullTimeline } from "./timeline";

export function PullConversation({
  repo,
  pull,
  csrf,
  refresh,
  comment,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
  comment: ReturnType<typeof usePullComment>;
}) {
  const focus = useDiscussionPermalink();
  const comments = useDiscussionHistory<PullComment>(
    repo,
    `pulls/${pull.number}/comments`,
    focus?.kind === "issuecomment" ? focus.number : undefined,
  );
  const reviews = useDiscussionHistory<PullReview, PullReviewPage>(
    repo,
    `pulls/${pull.number}/reviews`,
    focus?.kind === "pullrequestreview" ? focus.number : undefined,
  );
  const ready = Boolean(
    focus &&
    !comments.loading &&
    !reviews.loading &&
    !comments.linked.loading &&
    !reviews.linked.loading &&
    (focus.kind === "issuecomment" ? comments.items : reviews.items).some(
      (item) => item.number === focus.number,
    ),
  );
  useContributionFocus(focus?.id, ready);
  const upsert = comments.upsert;
  useEffect(() => {
    if (comment.created) upsert([comment.created]);
  }, [comment.created, upsert]);
  // All retained pages follow the same observed head; an old page cannot
  // keep a green approval after newer code has been observed.
  const currentReviews = reviews.items.map((review) => ({
    ...review,
    current: reviews.data?.head_oid
      ? review.commit_oid === reviews.data.head_oid
      : null,
  }));
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
      <RequestWidget timing={comments.timing} label="Conversation comments">
        <RequestWidget timing={reviews.timing} label="Pull request reviews">
          <div className="flex flex-col gap-3">
            <div className="section-heading">
              <h3>Conversation</h3>
              <Button
                variant="outline"
                size="sm"
                disabled={comments.loading || reviews.loading}
                onClick={() => {
                  comments.refresh();
                  reviews.refresh();
                }}
              >
                Refresh conversation
              </Button>
            </div>
            <HistoryControls label="comments" history={comments} />
            <HistoryControls label="reviews" history={reviews} />
            <PullTimeline
              repo={repo}
              pull={pull.number}
              csrf={csrf}
              comments={comments.items}
              reviews={currentReviews}
            />
          </div>
        </RequestWidget>
      </RequestWidget>
      <ActivityTimeline
        key={`${pull.version}:${comment.created?.number ?? 0}`}
        repo={repo}
        target={`pulls/${pull.number}`}
      />
      <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
      {!repo.archived && (
        <PullCommentForm
          repo={repo}
          pull={pull}
          csrf={csrf}
          refresh={refresh}
          comment={comment}
        />
      )}
    </div>
  );
}
