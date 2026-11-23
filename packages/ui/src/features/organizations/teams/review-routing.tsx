import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import type {
  ReviewRoutingAlgorithm,
  ReviewRoutingSettings,
} from "@/lib/contracts";
import type { Commands } from "./commands";
import { useTeamRead } from "./read";

export function TeamReviewRouting({
  organization,
  actor,
  team,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  commands: Commands;
}) {
  const state = useTeamRead<ReviewRoutingSettings>(
    `/api/organizations/${organization}/teams/${team}/review-routing`,
    commands.generation,
  );
  return (
    <section className="flex flex-col gap-3">
      <h3 className="font-semibold">Automatic review assignment</h3>
      <p className="text-sm text-muted-foreground">
        Assign team review requests to members with write access. Rotation and
        open-request counts are tracked per repository.
      </p>
      <Result state={state} timingLabel="Review assignment settings">
        {(settings) => (
          <form
            key={settings.version}
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
                },
                "Update review assignment",
              );
            }}
          >
            <div className="flex flex-col gap-2">
              <Label htmlFor="review-routing-algorithm">
                Assignment method
              </Label>
              <NativeSelect
                id="review-routing-algorithm"
                name="algorithm"
                defaultValue={settings.algorithm}
                disabled={commands.locked}
              >
                <NativeSelectOption value="disabled">
                  Disabled
                </NativeSelectOption>
                <NativeSelectOption value="round_robin">
                  Round robin
                </NativeSelectOption>
                <NativeSelectOption value="fewest_open">
                  Fewest open requests
                </NativeSelectOption>
              </NativeSelect>
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="review-routing-count">
                Reviewers per request
              </Label>
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
            <Button type="submit" disabled={commands.locked}>
              Save review assignment
            </Button>
          </form>
        )}
      </Result>
    </section>
  );
}
