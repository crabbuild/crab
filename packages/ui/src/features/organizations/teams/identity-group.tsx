import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";

type GroupStatus = {
  group: { id: string; name: string } | null;
  version: number;
  enabled: boolean;
  can_link: boolean;
};

export function IdentityGroupSettings({
  path,
  csrf,
  onChanged,
}: {
  path: string;
  csrf: string;
  onChanged: () => void;
}) {
  const state = useRequest<GroupStatus>(path);
  const mutation = useMutation(csrf, null);
  const [groupId, setGroupId] = useState("");
  const [unlinking, setUnlinking] = useState(false);

  async function save(group: string | null, version: number) {
    const applied = await mutation.run<{ applied: boolean }>(path, "PUT", {
      group,
      version,
    });
    state.retry();
    if (!applied?.applied) return;
    setGroupId("");
    setUnlinking(false);
    onChanged();
  }

  return (
    <section className="space-y-3" aria-labelledby="team-identity-group">
      <h3 id="team-identity-group" className="font-semibold">
        Identity provider group
      </h3>
      <Result state={state} timingLabel="Identity provider group">
        {({ group, version, enabled, can_link }) =>
          group ? (
            <div className="space-y-3">
              <p className="break-words">
                <strong>{group.name}</strong> manages this team’s membership.
              </p>
              {can_link &&
                (unlinking ? (
                  <div className="space-y-2">
                    <p className="text-sm text-muted-foreground">
                      Unlinking clears the synced roster and removes the
                      repository access it grants.
                    </p>
                    <div className="flex flex-wrap gap-2">
                      <Button
                        variant="destructive"
                        disabled={mutation.pending}
                        onClick={() => void save(null, version)}
                      >
                        Clear roster and unlink
                      </Button>
                      <Button
                        variant="outline"
                        onClick={() => setUnlinking(false)}
                      >
                        Cancel
                      </Button>
                    </div>
                  </div>
                ) : (
                  <Button variant="outline" onClick={() => setUnlinking(true)}>
                    Unlink group
                  </Button>
                ))}
            </div>
          ) : enabled && can_link ? (
            <form
              className="flex max-w-lg flex-col items-start gap-3"
              onSubmit={(event) => {
                event.preventDefault();
                void save(groupId.trim(), version);
              }}
            >
              <p className="text-sm text-muted-foreground">
                Linking replaces the roster with group members who already
                belong to this organization.
              </p>
              <Field>
                <FieldLabel htmlFor="team-scim-group">SCIM group ID</FieldLabel>
                <Input
                  id="team-scim-group"
                  required
                  maxLength={36}
                  pattern="[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}"
                  value={groupId}
                  onChange={(event) => setGroupId(event.target.value)}
                  disabled={mutation.pending}
                />
              </Field>
              <Button type="submit" disabled={mutation.pending}>
                Link group and replace roster
              </Button>
            </form>
          ) : (
            <p className="text-sm text-muted-foreground">
              SCIM provisioning must be configured on the server to link a
              group.
            </p>
          )
        }
      </Result>
      <Failure message={mutation.error} />
    </section>
  );
}
