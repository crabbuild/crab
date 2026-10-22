import { Author } from "@/components/discussion/author";
import { type Repository } from "@/lib/api";
import { Reactions } from "@/components/discussion/reactions";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { short } from "@/lib/format";
import {
  CheckCircleFillIcon,
  CodeReviewIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";

import { timestamp } from "@/components/discussion/time";
import type { PullComment, PullReview } from "./model";

export function PullTimeline({
  repo,
  pull,
  csrf,
  comments,
  reviews,
}: {
  repo: Repository;
  pull: number;
  csrf: string;
  comments: PullComment[];
  reviews: PullReview[];
}) {
  const events = [
    ...comments.map((comment) => ({
      kind: "comment" as const,
      value: comment,
    })),
    ...reviews.map((review) => ({ kind: "review" as const, value: review })),
  ].sort((left, right) => left.value.created_at - right.value.created_at);
  return (
    <div className="discussion-thread pull-timeline">
      {events.map((event) =>
        event.kind === "comment" ? (
          <article
            className="panel discussion-card"
            key={`comment-${event.value.number}`}
          >
            <header>
              <strong>
                <Author author={event.value.author} />
              </strong>
              <span className="muted">
                commented {timestamp(event.value.created_at)}
              </span>
            </header>
            <DiscussionMarkdown>{event.value.body}</DiscussionMarkdown>
            <Reactions
              repo={repo}
              csrf={csrf}
              target={`pulls/${pull}/comments/${event.value.number}`}
              summary={event.value.reactions}
            />
          </article>
        ) : (
          <ReviewEvent
            key={`review-${event.value.number}`}
            repo={repo}
            pull={pull}
            csrf={csrf}
            review={event.value}
          />
        ),
      )}
    </div>
  );
}

function ReviewEvent({
  repo,
  pull,
  csrf,
  review,
}: {
  repo: Repository;
  pull: number;
  csrf: string;
  review: PullReview;
}) {
  const action =
    review.state === "approved"
      ? "approved these changes"
      : review.state === "changes_requested"
        ? "requested changes"
        : "left a review";
  const Icon =
    review.state === "approved"
      ? CheckCircleFillIcon
      : review.state === "changes_requested"
        ? XCircleFillIcon
        : CodeReviewIcon;
  return (
    <article className={`panel review-event ${review.state}`}>
      <header>
        <span className="review-icon" aria-hidden="true">
          <Icon />
        </span>
        <strong>
          <Author author={review.author} />
        </strong>
        <span> {action}</span>
        {review.current !== true && (
          <span className="review-outdated">
            {review.current === null ? "Head unavailable" : "Outdated"}
          </span>
        )}
        <span className="muted">{timestamp(review.created_at)}</span>
      </header>
      {review.body && <DiscussionMarkdown>{review.body}</DiscussionMarkdown>}
      <Reactions
        repo={repo}
        csrf={csrf}
        target={`pulls/${pull}/reviews/${review.number}`}
        summary={review.reactions}
      />
      <footer className="muted">
        Reviewed commit <code>{short(review.commit_oid)}</code>
      </footer>
    </article>
  );
}
