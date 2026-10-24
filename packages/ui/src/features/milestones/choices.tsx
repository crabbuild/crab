import { useEffect, useId, useState, type RefObject } from "react";
import {
  Field,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Result } from "@/components/request/result";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type { Milestone } from "./model";

export type MilestoneChoice = Pick<Milestone, "number" | "title" | "state">;
interface Choices {
  items: MilestoneChoice[];
  truncated: boolean;
}

export function MilestoneChoices({
  repo,
  chosen,
  onChange,
  disabled,
  inputRef,
}: {
  repo: Repository;
  chosen: MilestoneChoice | null;
  onChange: (choice: MilestoneChoice | null) => void;
  disabled: boolean;
  inputRef: RefObject<HTMLInputElement | null>;
}) {
  const [query, setQuery] = useState("");
  const [requested, setRequested] = useState("");
  const [state, setState] = useState("open");
  const id = useId();
  useEffect(() => {
    const timer = setTimeout(() => setRequested(query), 150);
    return () => clearTimeout(timer);
  }, [query]);
  const catalog = useRequest<Choices>(
    endpoint(repo, "discussion-search/milestones", {
      search: requested,
      state,
    }),
  );
  return (
    <>
      <Field className="p-3">
        <FieldLabel htmlFor={id} className="sr-only">
          Filter milestones
        </FieldLabel>
        <Input
          id={id}
          ref={inputRef}
          placeholder="Filter milestones"
          maxLength={256}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
      </Field>
      <ToggleGroup
        type="single"
        value={state}
        size="sm"
        className="mx-3 mb-3"
        aria-label="Milestone state"
        onValueChange={(value) => {
          if (value) setState(value);
        }}
      >
        <ToggleGroupItem value="open" aria-label="Open milestones">
          Open
        </ToggleGroupItem>
        <ToggleGroupItem value="closed" aria-label="Closed milestones">
          Closed
        </ToggleGroupItem>
      </ToggleGroup>
      <Result
        state={{
          ...catalog,
          loading: catalog.loading || query !== requested,
        }}
        timingLabel="Milestone choices"
      >
        {(page) => {
          const choices =
            chosen &&
            !page.items.some((value) => value.number === chosen.number)
              ? [chosen, ...page.items]
              : page.items;
          return (
            <>
              <div
                role="region"
                aria-label="Milestone choices"
                tabIndex={0}
                className="max-h-64 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <FieldSet disabled={disabled} className="gap-0 px-3 pb-3">
                  <FieldLegend className="sr-only">
                    Repository milestones
                  </FieldLegend>
                  <RadioGroup
                    value={chosen ? String(chosen.number) : "none"}
                    aria-label="Milestone assignment"
                    className="gap-0"
                    onValueChange={(value) =>
                      onChange(
                        choices.find((item) => String(item.number) === value) ??
                          null,
                      )
                    }
                  >
                    <FieldLabel className="w-full cursor-pointer rounded-md p-2 hover:bg-accent">
                      <RadioGroupItem value="none" />
                      No milestone
                    </FieldLabel>
                    {choices.map((value) => (
                      <FieldLabel
                        key={value.number}
                        className="w-full cursor-pointer rounded-md p-2 hover:bg-accent"
                      >
                        <RadioGroupItem
                          value={String(value.number)}
                          aria-label={value.title}
                        />
                        <span className="min-w-0 break-words">
                          {value.title}
                          <span className="block text-muted-foreground">
                            {value.state === "closed" ? "Closed" : "Open"}
                            {chosen?.number === value.number
                              ? " · Selected"
                              : ""}
                          </span>
                        </span>
                      </FieldLabel>
                    ))}
                  </RadioGroup>
                  {page.items.length === 0 && (
                    <p className="p-2 text-muted-foreground" role="status">
                      No milestones match your search.
                    </p>
                  )}
                  {page.truncated && (
                    <p className="p-2 text-muted-foreground" role="status">
                      More milestones match. Refine your search.
                    </p>
                  )}
                </FieldSet>
              </div>
            </>
          );
        }}
      </Result>
    </>
  );
}
