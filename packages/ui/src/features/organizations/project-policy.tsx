import { useEffect, useRef, useState } from "react";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { FieldSelect } from "@/components/ui/field-select";
import { CommandError, useProjectCommands } from "@/features/projects/commands";
import { projectAccessRoles } from "@/features/projects/workspace/model";
import { useRequest } from "@/lib/api";
import type {
  OrganizationProjectPolicy,
  ProjectAccessRole,
} from "@/lib/contracts";

export function ProjectPolicySettings({
  organization,
}: {
  organization: string;
}) {
  const commands = useProjectCommands();
  const resource = useRequest<OrganizationProjectPolicy>(
    `/api/organizations/${organization}/project-policy`,
  );
  const generation = useRef(commands.generation);
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      resource.retry();
    }
  }, [commands.generation, resource.retry]);
  return (
    <section
      aria-labelledby="organization-project-policy"
      className="grid max-w-2xl gap-4 border-t pt-6"
    >
      <h2 id="organization-project-policy" className="text-lg font-semibold">
        Project policy
      </h2>
      <CommandError />
      <Result state={resource} timingLabel="Project policy">
        {(data) => (
          <PolicyForm
            key={`${data.version}:${data.scope}`}
            policy={data}
            disabled={commands.locked}
            onSave={async (role) => {
              const result = await commands.submitOrganization(organization, {
                kind: "set_default_access",
                version: data.version,
                role,
                scope: data.scope,
              });
              if (result?.outcome === "rejected") resource.retry();
            }}
          />
        )}
      </Result>
    </section>
  );
}

function PolicyForm({
  policy,
  disabled,
  onSave,
}: {
  policy: OrganizationProjectPolicy;
  disabled: boolean;
  onSave: (role: ProjectAccessRole) => Promise<void>;
}) {
  const [role, setRole] = useState(policy.role);
  const count = policy.affected_projects;
  return (
    <Dialog>
      <div className="grid gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <FieldSelect
            label="Default Project access"
            value={role}
            options={projectAccessRoles}
            disabled={disabled}
            onValueChange={(value) => setRole(value as ProjectAccessRole)}
          />
          <DialogTrigger asChild>
            <Button type="button" disabled={disabled || role === policy.role}>
              Review change
            </Button>
          </DialogTrigger>
        </div>
        <p className="text-sm text-muted-foreground">
          Applies to new Projects and existing Projects that use the
          organization default.
        </p>
      </div>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Change default Project access?</DialogTitle>
          <DialogDescription>
            Set the default from {projectAccessRoles[policy.role]} to{" "}
            {projectAccessRoles[role]}. This affects {count} existing{" "}
            {count === 1 ? "Project" : "Projects"} and sets the default for new
            Projects.
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <DialogClose asChild>
            <Button type="button" variant="outline">
              Cancel
            </Button>
          </DialogClose>
          <DialogClose asChild>
            <Button
              type="button"
              disabled={disabled}
              onClick={() => void onSave(role)}
            >
              Change default to {projectAccessRoles[role]}
            </Button>
          </DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
