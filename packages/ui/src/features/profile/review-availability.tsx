import { useEffect, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { useRequest } from "@/lib/api";
import type {
  ReviewAvailability,
  ReviewAvailabilityChoice,
} from "@/lib/contracts";

const path = "/api/profile/review-availability";
const choices: { value: ReviewAvailabilityChoice; label: string }[] = [
  { value: "available", label: "Available" },
  { value: "thirty_minutes", label: "Busy for 30 minutes" },
  { value: "one_hour", label: "Busy for 1 hour" },
  { value: "four_hours", label: "Busy for 4 hours" },
  { value: "one_day", label: "Busy for 1 day" },
  { value: "one_week", label: "Busy for 1 week" },
  { value: "until_cleared", label: "Busy until cleared" },
];

export function ReviewAvailabilitySettings({ csrf }: { csrf: string }) {
  const state = useRequest<ReviewAvailability>(path);
  return (
    <section className="min-w-0">
      <h1 className="mb-3 text-2xl font-semibold tracking-tight">
        Review availability
      </h1>
      <p className="mb-6 border-b pb-6 text-sm text-muted-foreground">
        Pause automatic team review assignments while you’re busy. People can
        still request your review directly.
      </p>
      <Result state={state} timingLabel="Review availability">
        {(value) => (
          <AvailabilityForm initial={value} csrf={csrf} reload={state.retry} />
        )}
      </Result>
    </section>
  );
}

function AvailabilityForm({
  initial,
  csrf,
  reload,
}: {
  initial: ReviewAvailability;
  csrf: string;
  reload: () => void;
}) {
  const [current, setCurrent] = useState(initial);
  const [choice, setChoice] = useState<ReviewAvailabilityChoice | "">("");
  const [saved, setSaved] = useState(false);
  const mutation = useMutation(csrf, null);
  const expiry = current.expires_at_ms;
  useEffect(() => {
    if (expiry === null || choice) return;
    const remaining = expiry - Date.now();
    if (remaining <= 0) return;
    const timer = window.setTimeout(reload, remaining + 250);
    return () => window.clearTimeout(timer);
  }, [choice, expiry, reload]);

  return (
    <form
      className="flex flex-col items-start gap-4"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!choice) return;
        setSaved(false);
        const result = await mutation.run<ReviewAvailability>(path, "PUT", {
          version: current.version,
          choice,
        });
        if (!result) return;
        setCurrent(result);
        setChoice("");
        setSaved(true);
      }}
    >
      <p className="text-sm">
        Current status <strong>{current.busy ? "Busy" : "Available"}</strong>
        {expiry !== null && (
          <span className="text-muted-foreground">
            {" until "}
            <time dateTime={new Date(expiry).toISOString()}>
              {new Date(expiry).toLocaleString()}
            </time>
          </span>
        )}
      </p>
      <div className="flex flex-col gap-2">
        <Label htmlFor="review-availability-choice">Set status</Label>
        <NativeSelect
          id="review-availability-choice"
          value={choice}
          onChange={(event) => {
            setChoice(event.target.value as ReviewAvailabilityChoice);
            setSaved(false);
          }}
          disabled={mutation.pending || mutation.conflict}
        >
          <NativeSelectOption value="" disabled>
            Choose a status
          </NativeSelectOption>
          {choices.map(({ value, label }) => (
            <NativeSelectOption key={value} value={value}>
              {label}
            </NativeSelectOption>
          ))}
        </NativeSelect>
      </div>
      <Button
        type="submit"
        disabled={!choice || mutation.pending || mutation.conflict}
      >
        Save availability
      </Button>
      {saved && <p role="status">Review availability saved.</p>}
      {mutation.error && <p role="alert">{mutation.error}</p>}
      {mutation.conflict && (
        <Button type="button" variant="outline" onClick={reload}>
          Reload availability
        </Button>
      )}
    </form>
  );
}
