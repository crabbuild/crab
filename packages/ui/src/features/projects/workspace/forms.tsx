import { useEffect, useId, useRef, useState } from "react";
import { navigate, useRequest } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { DatePicker } from "@/components/ui/date-picker";
import { CommandError, useProjectCommands } from "../commands";
import { ProjectDraftReview } from "../review";
import { FieldSelect } from "@/components/ui/field-select";
import { statuses, health, workspaceHref, type Detail } from "./model";
import type {
  Membership,
  OrganizationPage,
} from "@/features/organizations/model";
import { useProjectDraft } from "../drafts";

export function NewProject({
  organization,
  onClose,
}: {
  organization: string;
  onClose: () => void;
}) {
  const saved = useProjectDraft(
    { target: { organization }, project: null },
    {
      kind: "create",
      name: "",
      template: "board",
      sourceVersion: null as number | null,
    },
  );
  const { name, template } = saved.value;
  const setName = (name: string) => saved.update({ name });
  const setTemplate = (template: string) => saved.update({ template });
  const commands = useProjectCommands();
  const id = useId();
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New project</DialogTitle>
          <DialogDescription>
            Bring work from multiple repositories into one plan.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-5"
          onSubmit={async (event) => {
            event.preventDefault();
            const project = crypto.randomUUID();
            const checkpoint = saved.checkpoint();
            if (!checkpoint) return;
            const result = await commands.submitOrganization(
              organization,
              {
                kind: "create",
                id: project,
                name: name.trim(),
                template,
              },
              checkpoint,
            );
            if (mounted.current && result?.outcome === "applied")
              navigate(workspaceHref(organization, project));
          }}
        >
          <fieldset disabled={commands.locked}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor={`${id}-name`}>Project name</FieldLabel>
                <Input
                  id={`${id}-name`}
                  autoFocus
                  required
                  maxLength={100}
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  placeholder="What are you building?"
                />
              </Field>
              <Field>
                <FieldLabel htmlFor={`${id}-layout`}>Starting view</FieldLabel>
                <FieldSelect
                  id={`${id}-layout`}
                  label="Starting view"
                  value={template}
                  onValueChange={setTemplate}
                  options={{
                    board: "Board",
                    table: "Table",
                    roadmap: "Roadmap",
                  }}
                />
              </Field>
            </FieldGroup>
          </fieldset>
          {saved.feedback}
          <CommandError />
          <DialogFooter>
            <Button
              variant="outline"
              disabled={commands.locked}
              onClick={onClose}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={!name.trim() || commands.locked}>
              {commands.pending ? "Creating…" : "Create project"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function ProjectProperties({
  organization,
  detail,
  onClose,
}: {
  organization: string;
  detail: Detail;
  onClose: () => void;
}) {
  const saved = useProjectDraft(
    { target: { organization }, project: detail.project.id },
    {
      kind: "properties",
      properties: detail.project.properties,
      version: detail.project.version,
    },
  );
  const { properties: draft, version } = saved.value;
  const setDraft = (properties: typeof detail.project.properties) =>
    saved.update({ properties });
  const setVersion = (version: number) => saved.update({ version });
  const [after, setAfter] = useState<string>();
  const commands = useProjectCommands();
  const id = useId();
  const members = useRequest<OrganizationPage<Membership>>(
    `/api/organizations/${organization}/members?${new URLSearchParams(after ? { after } : {})}`,
  );
  const leads: Record<string, string> = { none: "No lead" };
  if (draft.lead)
    leads[draft.lead] =
      draft.lead === detail.lead?.id
        ? detail.lead.name
        : "Selected project lead";
  for (const member of members.data?.items ?? [])
    leads[member.actor] =
      member.profile?.name ||
      member.profile?.handle ||
      `Member ${member.actor.slice(0, 8)}`;
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Project properties</DialogTitle>
          <DialogDescription>
            Set the project lead, delivery status, and planned timeframe.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-5"
          onSubmit={async (event) => {
            event.preventDefault();
            const checkpoint = saved.checkpoint();
            if (!checkpoint) return;
            const result = await commands.submitOrganization(
              organization,
              {
                kind: "planning",
                project: detail.project.id,
                version,
                properties: draft,
              },
              checkpoint,
            );
            if (result?.outcome === "applied") onClose();
          }}
        >
          <fieldset disabled={commands.locked}>
            <FieldGroup>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor={`${id}-status`}>Status</FieldLabel>
                  <FieldSelect
                    id={`${id}-status`}
                    label="Project status"
                    options={statuses}
                    value={draft.status}
                    onValueChange={(status) =>
                      setDraft({
                        ...draft,
                        status: status as typeof draft.status,
                      })
                    }
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={`${id}-health`}>Health</FieldLabel>
                  <FieldSelect
                    id={`${id}-health`}
                    label="Project health"
                    options={health}
                    value={draft.health}
                    onValueChange={(health) =>
                      setDraft({
                        ...draft,
                        health: health as typeof draft.health,
                      })
                    }
                  />
                </Field>
              </div>
              <Field>
                <FieldLabel htmlFor={`${id}-lead`}>Project lead</FieldLabel>
                <FieldSelect
                  id={`${id}-lead`}
                  label="Project lead"
                  options={leads}
                  value={draft.lead ?? "none"}
                  onValueChange={(lead) =>
                    setDraft({ ...draft, lead: lead === "none" ? null : lead })
                  }
                />
                <Result state={members}>
                  {(page) => (
                    <div className="flex gap-2">
                      {after && (
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => setAfter(undefined)}
                        >
                          First members
                        </Button>
                      )}
                      {page.next && (
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => setAfter(page.next!)}
                        >
                          More members
                        </Button>
                      )}
                    </div>
                  )}
                </Result>
              </Field>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor={`${id}-start`}>Start date</FieldLabel>
                  <DatePicker
                    id={`${id}-start`}
                    label="Start date"
                    max={draft.target ?? undefined}
                    value={draft.start}
                    disabled={commands.locked}
                    onValueChange={(start) => setDraft({ ...draft, start })}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={`${id}-target`}>Target date</FieldLabel>
                  <DatePicker
                    id={`${id}-target`}
                    label="Target date"
                    min={draft.start ?? undefined}
                    value={draft.target}
                    disabled={commands.locked}
                    onValueChange={(target) => setDraft({ ...draft, target })}
                  />
                </Field>
              </div>
            </FieldGroup>
          </fieldset>
          {saved.feedback}
          <CommandError />
          <ProjectDraftReview
            target={{ organization }}
            project={detail.project.id}
            onReviewed={(project) => setVersion(project.version)}
          />
          <DialogFooter>
            <Button
              variant="outline"
              disabled={commands.locked}
              onClick={onClose}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={commands.locked}>
              {commands.pending ? "Saving…" : "Save properties"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
