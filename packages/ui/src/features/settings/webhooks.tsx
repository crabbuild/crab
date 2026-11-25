import { Disclosure } from "@/components/ui/collapsible";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldSelect } from "@/components/ui/field-select";
import { useState, type FormEvent } from "react";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";

const eventOptions = [
  ["push", "Git pushes"],
  ["pull_request", "Pull requests"],
  ["issues", "Issues"],
  ["issue_comment", "Issue and pull request comments"],
  ["pull_request_review", "Pull request reviews"],
] as const;
type WebhookEvent = (typeof eventOptions)[number][0];
type Subscription = { id: string } & Record<WebhookEvent, boolean>;

const deliveryLabels: Record<WebhookEvent, string> = {
  push: "Push",
  pull_request: "Pull request",
  issues: "Issue",
  issue_comment: "Comment",
  pull_request_review: "Review",
};

type Delivery = {
  id: string;
  event: WebhookEvent;
  action: string | null;
  issue: number | null;
  pull: number | null;
  comment: number | null;
  review: { number: number; commit_oid: string } | null;
  head_change: { before: string | null; after: string } | null;
  queued_at: number;
  status: "queued" | "sending" | "delivered" | "failed" | "cancelled";
  attempts: number;
  version: number;
  can_redeliver: boolean;
  recent_attempts: { at: number; http_status: number | null }[];
};

type Webhooks = {
  destinations: { id: string; url: string | null }[];
  selected: string | null;
  destination: string | null;
  subscription: Subscription | null;
  active: boolean;
  deliveries: Delivery[];
};

const timeFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "medium",
});

export function OutboundWebhooks({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const [destinationKey, setDestinationKey] = useState("");
  const selectedPath = (key: string) =>
    endpoint(repo, "settings/webhooks", { destination: key || undefined });
  const settings = useRequest<Webhooks>(selectedPath(destinationKey), repo.id);
  const mutation = useMutation(csrf, repo);
  const [secret, setSecret] = useState<string>();

  async function change(key: string, body: object) {
    const result = await mutation.run<{ secret?: string | null }>(
      selectedPath(key),
      "POST",
      body,
    );
    if (result) {
      setSecret(result.secret ?? undefined);
      settings.retry();
    }
  }

  return (
    <section className="settings-content" aria-labelledby="webhooks-title">
      <div className="settings-title">
        <div>
          <h2 id="webhooks-title">Webhooks</h2>
          <p>Send repository events to approved receivers.</p>
        </div>
      </div>
      {secret && (
        <section
          className="settings-section grid gap-3 p-4"
          aria-labelledby="webhook-secret-title"
        >
          <h3 id="webhook-secret-title" className="font-semibold">
            Signing secret
          </h3>
          <label className="grid gap-1 text-sm">
            Copy this secret to your receiver.
            <Input readOnly value={secret} className="font-mono" />
          </label>
          <Button
            className="w-fit"
            size="sm"
            variant="outline"
            onClick={() => setSecret(undefined)}
          >
            Hide secret
          </Button>
        </section>
      )}
      {mutation.error && (
        <div
          className="settings-section flex flex-col items-start gap-3 p-4 text-sm text-destructive"
          role="alert"
        >
          <p>{mutation.error}</p>
          {mutation.status === 409 && (
            <Button
              variant="outline"
              onClick={() => {
                mutation.reset();
                settings.retry();
              }}
            >
              Reload webhook settings
            </Button>
          )}
        </div>
      )}
      <Result state={settings} timingLabel="Outgoing webhooks">
        {({
          destinations,
          selected,
          destination,
          subscription,
          active,
          deliveries,
        }) => (
          <>
            <section
              className="settings-section"
              aria-labelledby="webhook-destination"
            >
              <header>
                <div className="min-w-0">
                  <h3 id="webhook-destination" className="font-semibold">
                    Destination
                  </h3>
                  <p className="mt-1 text-sm wrap-anywhere">
                    {destination ??
                      (selected
                        ? "This destination is no longer approved."
                        : "Ask your Crab operator to configure a receiver for this repository.")}
                  </p>
                </div>
                {selected && subscription && (
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={mutation.pending}
                    onClick={() =>
                      void change(selected, {
                        kind: "disable",
                        expected: subscription.id,
                      })
                    }
                  >
                    Disable webhook
                  </Button>
                )}
              </header>
              <div className="divide-y">
                {destinations.length > 1 && (
                  <Field className="min-w-0 p-4">
                    <FieldLabel htmlFor="webhook-destination-picker">
                      Webhook destination
                    </FieldLabel>
                    <FieldSelect
                      id="webhook-destination-picker"
                      className="w-full max-w-xl"
                      value={selected ?? ""}
                      disabled={mutation.pending}
                      onValueChange={(nextValue) => {
                        setSecret(undefined);
                        mutation.reset();
                        setDestinationKey(nextValue);
                      }}
                      options={[
                        ...destinations.map(({ id, url }) => ({
                          value: id,
                          label: url ?? `Former destination ${short(id)}`,
                        })),
                      ]}
                    />
                  </Field>
                )}
                {selected && destination && !repo.archived && (
                  <EventsForm
                    key={`${selected}:${subscription?.id ?? ""}`}
                    subscription={subscription}
                    pending={mutation.pending}
                    change={(body) => change(selected, body)}
                  />
                )}
              </div>
            </section>
            <section
              className="settings-section"
              aria-labelledby="webhook-deliveries"
            >
              <header>
                <div>
                  <h3 id="webhook-deliveries" className="font-semibold">
                    Recent deliveries
                  </h3>
                </div>
                <Button size="sm" variant="outline" onClick={settings.retry}>
                  Refresh deliveries
                </Button>
              </header>
              {deliveries.length ? (
                <ul className="divide-y">
                  {deliveries.map((delivery) => {
                    const number = delivery.pull ?? delivery.issue;
                    const issue = delivery.pull === null;
                    const anchor = delivery.review
                      ? `#pullrequestreview-${delivery.review.number}`
                      : delivery.comment != null
                        ? `#issuecomment-${delivery.comment}`
                        : "";
                    const commit =
                      delivery.review?.commit_oid ??
                      delivery.head_change?.after;
                    const href =
                      number == null
                        ? null
                        : repoHref(repo, {
                            view: issue ? "issues" : "pulls",
                            issue: issue ? String(number) : undefined,
                            pull: issue ? undefined : String(number),
                            repository_id: repo.id,
                          });
                    return (
                      <li className="grid min-w-0 gap-3 p-4" key={delivery.id}>
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <div className="grid gap-1">
                            <strong className="text-sm capitalize">
                              {delivery.action === "synchronize"
                                ? "Pull request head updated"
                                : [
                                    deliveryLabels[delivery.event],
                                    delivery.action?.replaceAll("_", " "),
                                  ]
                                    .filter(Boolean)
                                    .join(" ")}
                            </strong>
                            <span className="text-sm text-muted-foreground">
                              <span className="capitalize">
                                {delivery.status}
                              </span>{" "}
                              · {timeFormat.format(delivery.queued_at)}
                            </span>
                          </div>
                          {selected && active && delivery.can_redeliver && (
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={mutation.pending}
                              onClick={() =>
                                void change(selected, {
                                  kind: "redeliver",
                                  id: delivery.id,
                                  version: delivery.version,
                                })
                              }
                            >
                              Redeliver
                            </Button>
                          )}
                        </div>
                        {href && (
                          <Link
                            href={`${href}${anchor}`}
                            className="w-fit text-sm underline underline-offset-2"
                          >
                            {issue ? "Issue" : "Pull request"} #{number}
                            {delivery.comment != null &&
                              ` · Comment #${delivery.comment}`}
                            {delivery.review &&
                              ` · Review #${delivery.review.number}`}
                          </Link>
                        )}
                        {commit && (
                          <p className="text-sm">
                            {delivery.review
                              ? "Reviewed commit"
                              : "Head commit"}{" "}
                            <code title={commit}>{short(commit)}</code>
                          </p>
                        )}
                        <Disclosure
                          className="text-sm"
                          summaryClassName="w-fit cursor-pointer font-medium"
                          summary={
                            <>
                              Delivery details · {delivery.attempts}{" "}
                              {delivery.attempts === 1 ? "attempt" : "attempts"}
                            </>
                          }
                        >
                          <p className="mt-2 wrap-anywhere font-mono text-xs">
                            {delivery.id}
                          </p>
                          <ul className="mt-2 grid gap-1 text-xs text-muted-foreground">
                            {delivery.recent_attempts.map((attempt, index) => (
                              <li key={`${attempt.at}-${index}`}>
                                {timeFormat.format(attempt.at)} ·{" "}
                                {attempt.http_status === null
                                  ? "No HTTP response"
                                  : `HTTP ${attempt.http_status}`}
                              </li>
                            ))}
                          </ul>
                        </Disclosure>
                      </li>
                    );
                  })}
                </ul>
              ) : (
                <p className="p-4 text-sm text-muted-foreground">
                  No outgoing deliveries yet.
                </p>
              )}
            </section>
          </>
        )}
      </Result>
    </section>
  );
}

function EventsForm({
  subscription,
  pending,
  change,
}: {
  subscription: Subscription | null;
  pending: boolean;
  change: (payload: object) => Promise<void>;
}) {
  const requestId = useSubmission();
  const [events, setEvents] = useState({
    push: subscription?.push ?? true,
    pull_request: subscription?.pull_request ?? true,
    issues: subscription?.issues ?? false,
    issue_comment: subscription?.issue_comment ?? false,
    pull_request_review: subscription?.pull_request_review ?? false,
  });
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const intent = {
      kind: "configure",
      expected: subscription?.id ?? null,
      ...events,
    };
    void change({ ...intent, id: requestId(intent) });
  }
  return (
    <form className="grid gap-4 p-4" onSubmit={submit}>
      <FieldSet className="gap-3">
        <FieldLegend variant="label">Events</FieldLegend>
        {eventOptions.map(([event, label]) => (
          <div key={event} className="grid gap-1">
            <label className="flex items-center gap-2 text-sm">
              <Checkbox
                checked={events[event]}
                onCheckedChange={(checked) =>
                  setEvents((previous) => ({
                    ...previous,
                    [event]: checked === true,
                  }))
                }
              />{" "}
              {label}
            </label>
            {event === "push" && (
              <FieldDescription className="ps-5">
                Includes branch and tag changes made in Crab.
              </FieldDescription>
            )}
          </div>
        ))}
      </FieldSet>
      {subscription && (
        <p className="text-sm text-muted-foreground">
          Saving replaces the signing secret and cancels outstanding deliveries
          for this subscription.
        </p>
      )}
      <Button
        className="w-fit"
        type="submit"
        disabled={pending || !Object.values(events).some(Boolean)}
      >
        {subscription ? "Save with new secret" : "Enable webhook"}
      </Button>
    </form>
  );
}
