import { useEffect, useRef, type ReactNode } from "react";
import type { Repository } from "@/lib/api";
import { RequestWidget } from "@/components/request/result";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { OverlayVisibility } from "@/components/ui/use-overlay-state";
import { Spinner } from "@/components/ui/spinner";
import { ActivityItem } from "./activity";
import { CommentCard } from "./comment";
import { ReviewCard } from "./review";
import { useConversation } from "./history";
import { entryId, entryKey, type Comment } from "./model";
import { useContributionFocus, useDiscussionPermalink } from "../permalink";

export function Conversation({
  repo,
  target,
  href,
  csrf,
  revision,
  active,
  children,
  onChanged,
}: {
  repo: Repository;
  target: string;
  href: string;
  csrf: string;
  revision: string;
  active: boolean;
  onChanged?: () => void;
  children: (busy: boolean, posted: (comment: Comment) => void) => ReactNode;
}) {
  const focus = useDiscussionPermalink();
  const history = useConversation(repo, target, focus, active);
  const continuation = useRef<HTMLHeadingElement>(null);
  const paging = useRef(false);
  useEffect(() => {
    if (history.loading || !paging.current) return;
    paging.current = false;
    if (document.activeElement === document.body) continuation.current?.focus();
  }, [history.loading]);
  const ready =
    active &&
    !history.loading &&
    history.entries.some(
      (entry) =>
        history.window.has(entryKey(entry)) && entryId(entry) === focus?.id,
    );
  useContributionFocus(focus?.id, ready);
  const observed = useRef(revision);
  useEffect(() => {
    if (observed.current !== revision) {
      observed.current = revision;
      if (active) history.refresh();
    }
  }, [revision, active, history.refresh]);
  return (
    <RequestWidget timing={history.timing} label="Conversation">
      <section
        className="conversation-history"
        aria-label="Conversation history"
      >
        <div className="section-heading">
          <h3 ref={continuation} tabIndex={-1}>
            Conversation
          </h3>
          <Button
            variant="outline"
            size="sm"
            aria-disabled={history.loading}
            aria-busy={history.loading}
            onClick={() => {
              if (!history.loading) history.refresh();
            }}
          >
            Refresh conversation
          </Button>
        </div>
        {history.older && (
          <Button
            variant="outline"
            className="self-start"
            disabled={history.loading}
            onClick={() => {
              paging.current = true;
              history.earlier();
            }}
          >
            Load earlier activity
          </Button>
        )}
        {history.loading && (
          <p
            role="status"
            className="flex items-center gap-2 text-muted-foreground"
          >
            <Spinner aria-hidden="true" />
            Loading conversation…
          </p>
        )}
        {history.error && (
          <Alert variant="destructive">
            <AlertTitle>Unable to load conversation</AlertTitle>
            <AlertDescription>
              <p>{history.error}</p>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" size="sm" onClick={history.retry}>
                  Try again
                </Button>
                <Button variant="outline" size="sm" onClick={history.refresh}>
                  Reload conversation
                </Button>
              </div>
            </AlertDescription>
          </Alert>
        )}
        <ol
          className="conversation-events discussion-thread pull-timeline"
          aria-label="Conversation, oldest first"
        >
          {history.entries.map((entry) => {
            const key = entryKey(entry),
              hidden = !history.window.has(key);
            if (entry.kind === "activity")
              return (
                <OverlayVisibility key={key} value={active && !hidden}>
                  <ActivityItem
                    repo={repo}
                    event={entry.value}
                    hidden={hidden}
                    href={`${href}#${entryId(entry)}`}
                  />
                </OverlayVisibility>
              );
            // Keep loaded editors mounted while moving between bounded windows.
            // Their unsaved body/version and pending mutation belong to the card.
            return (
              <OverlayVisibility key={key} value={active && !hidden}>
                <li hidden={hidden}>
                  {entry.kind === "comment" ? (
                    <CommentCard
                      comment={entry.value}
                      repo={repo}
                      target={target}
                      href={href}
                      csrf={csrf}
                      onSaved={(value) =>
                        history.upsert({ kind: "comment", value })
                      }
                    />
                  ) : (
                    <ReviewCard
                      onSaved={() => {
                        history.refresh();
                        onChanged?.();
                      }}
                      review={{
                        ...entry.value,
                        current: history.head
                          ? entry.value.commit_oid === history.head
                          : null,
                      }}
                      repo={repo}
                      target={target}
                      href={href}
                      csrf={csrf}
                    />
                  )}
                </li>
              </OverlayVisibility>
            );
          })}
        </ol>
        {history.newer && (
          <Button
            variant="outline"
            className="self-start"
            disabled={history.loading}
            onClick={() => {
              paging.current = true;
              history.later();
            }}
          >
            Load newer activity
          </Button>
        )}
        {history.initialized && children(history.loading, history.posted)}
      </section>
    </RequestWidget>
  );
}
