import { useState } from "react";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import type {
  ReviewRoutingAlgorithm,
  ReviewRoutingSettings,
  TeamRoster,
} from "@/lib/contracts";
import type { Commands } from "./commands";
import { TeamPages } from "./members";
import { useTeamRead } from "./read";

type RoutingProps = {
  organization: string;
  actor: string;
  team: string;
  commands: Commands;
};

export function TeamReviewRouting(props: RoutingProps) {
  const { organization, team, commands } = props;
  const state = useTeamRead<ReviewRoutingSettings>(
    `/api/organizations/${organization}/teams/${team}/review-routing`,
    commands.generation,
  );
  return (
    <section className="flex flex-col gap-3">
      <h3 className="font-semibold">Automatic review assignment</h3>
      <p className="text-sm text-muted-foreground">
        Assign team review requests to members with write access. Load balance
        considers outstanding requests and reviews requested in the last 30 days
        across the organization. Round robin rotates within each repository.
      </p>
      <Result state={state} timingLabel="Review assignment settings">
        {(settings) => (
          <ReviewRoutingForm
            key={`${team}:${settings.version}`}
            {...props}
            settings={settings}
          />
        )}
      </Result>
    </section>
  );
}

function ReviewRoutingForm({
  organization,
  actor,
  team,
  commands,
  settings,
}: RoutingProps & { settings: ReviewRoutingSettings }) {
  const [excluded, setExcluded] = useState(() => new Set(settings.excluded));
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams(after ? { after } : {});
  const members = useTeamRead<TeamRoster>(
    `/api/organizations/${organization}/teams/${team}/members?${params}`,
    commands.generation,
  );

  return (
    <form
      className="flex flex-col items-start gap-4"
      onSubmit={(event) => {
        event.preventDefault();
        const fields = new FormData(event.currentTarget);
        void commands.submit(
          organization,
          actor,
          {
            kind: "review_routing",
            team,
            version: settings.version,
            algorithm: fields.get("algorithm") as ReviewRoutingAlgorithm,
            reviewers: Number(fields.get("reviewers")),
            excluded: [...excluded].sort(),
            options: {
              count_existing: fields.has("count-existing"),
              keep_team: fields.has("keep-team"),
            },
          },
          "Update review assignment",
        );
      }}
    >
      <div className="flex flex-col gap-2">
        <Label htmlFor="review-routing-algorithm">Assignment method</Label>
        <NativeSelect
          id="review-routing-algorithm"
          name="algorithm"
          defaultValue={settings.algorithm}
          disabled={commands.locked}
        >
          <NativeSelectOption value="disabled">Disabled</NativeSelectOption>
          <NativeSelectOption value="round_robin">
            Round robin
          </NativeSelectOption>
          <NativeSelectOption value="fewest_open">
            Load balance
          </NativeSelectOption>
        </NativeSelect>
      </div>
      <div className="flex flex-col gap-2">
        <Label htmlFor="review-routing-count">Reviewers per request</Label>
        <Input
          id="review-routing-count"
          name="reviewers"
          type="number"
          min={1}
          max={10}
          defaultValue={settings.reviewers}
          className="w-24"
          disabled={commands.locked}
          required
        />
      </div>
      <div className="flex flex-col gap-3">
        <Label htmlFor="review-routing-existing" className="min-h-6">
          <Checkbox
            id="review-routing-existing"
            name="count-existing"
            defaultChecked={settings.options.count_existing}
            disabled={commands.locked}
          />
          Count existing review requests
        </Label>
        <Label htmlFor="review-routing-keep-team" className="min-h-6">
          <Checkbox
            id="review-routing-keep-team"
            name="keep-team"
            defaultChecked={settings.options.keep_team}
            disabled={commands.locked}
          />
          Keep the team requested after assignment
        </Label>
      </div>
      <fieldset className="flex w-full min-w-0 flex-col gap-2">
        <legend className="font-medium">Never auto-assign</legend>
        <p className="text-sm text-muted-foreground">
          Excluded members can still be requested individually. If no eligible
          members are available, the request stays with the team.
        </p>
        <Result state={members} timingLabel="Members for review assignment">
          {(data) => (
            <>
              {data.items.length ? (
                <ul className="divide-y rounded-md border">
                  {data.items.map(({ actor: member, profile }) => (
                    <li key={member} className="p-3">
                      <Label
                        htmlFor={`review-routing-exclude-${member}`}
                        className="flex items-start gap-3"
                      >
                        <Checkbox
                          id={`review-routing-exclude-${member}`}
                          checked={excluded.has(member)}
                          disabled={commands.locked}
                          onCheckedChange={(checked) =>
                            setExcluded((current) => {
                              const next = new Set(current);
                              if (checked === true) next.add(member);
                              else next.delete(member);
                              return next;
                            })
                          }
                        />
                        <span className="min-w-0 break-words">
                          {profile
                            ? `${profile.name} (@${profile.handle})`
                            : member}
                        </span>
                      </Label>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-sm text-muted-foreground">
                  This team has no members.
                </p>
              )}
              {(after || data.next) && (
                <TeamPages after={after} next={data.next} onPage={setAfter} />
              )}
            </>
          )}
        </Result>
      </fieldset>
      <Button type="submit" disabled={commands.locked}>
        Save review assignment
      </Button>
    </form>
  );
}
