import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { Result } from "@/components/request/result";
import { useRequest } from "@/lib/api";
import { CommandError, useProjectCommands } from "../commands";
import { projectEndpoint } from "../model";
import type { WorkspaceListing } from "./model";

export function LinkTeamProject({
  organization,
  team,
  onClose,
}: {
  organization: string;
  team: string;
  onClose: () => void;
}) {
  const commands = useProjectCommands();
  const [input, setInput] = useState("");
  const [search, setSearch] = useState("");
  const [selection, setSelection] = useState("");
  const projects = useRequest<WorkspaceListing>(
    search
      ? projectEndpoint({ organization }, "", {
          search,
          team,
          linkable: "true",
        })
      : null,
  );
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Link a project</DialogTitle>
          <DialogDescription>
            Linking grants the team Read access. Members keep any higher access
            they already have.
          </DialogDescription>
        </DialogHeader>
        <CommandError />
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            setSelection("");
            const next = input.trim();
            if (next && next === search) projects.retry();
            setSearch(next);
          }}
        >
          <Input
            type="search"
            aria-label="Find a project to link"
            placeholder="Search by project name"
            maxLength={100}
            value={input}
            className="min-w-32 flex-1"
            onChange={(event) => setInput(event.target.value)}
            disabled={commands.locked}
          />
          <Button type="submit" variant="outline" disabled={commands.locked}>
            Search
          </Button>
        </form>
        {search && (
          <Result state={projects} timingLabel="Projects">
            {({ items, next }) => {
              const project = items.find((item) => item.id === selection);
              return items.length ? (
                <form
                  className="grid gap-3"
                  onSubmit={async (event) => {
                    event.preventDefault();
                    if (!project || commands.locked) return;
                    const result = await commands.submitOrganization(
                      organization,
                      {
                        kind: "link_team",
                        project: project.id,
                        version: project.version,
                        team,
                      },
                    );
                    if (result?.outcome === "applied") onClose();
                    if (result?.outcome === "rejected") {
                      setSelection("");
                      projects.retry();
                    }
                  }}
                >
                  <FieldSelect
                    label="Project"
                    value={selection}
                    options={Object.fromEntries([
                      ["", "Choose a project"],
                      ...items.map(({ id, definition }) => [
                        id,
                        definition.name,
                      ]),
                    ])}
                    onValueChange={setSelection}
                    disabled={commands.locked}
                  />
                  {next && (
                    <p className="text-sm text-muted-foreground">
                      Enter more of the project name to narrow the results.
                    </p>
                  )}
                  <Button
                    type="submit"
                    className="w-fit"
                    disabled={!project || commands.locked}
                  >
                    Link project
                  </Button>
                </form>
              ) : (
                <p className="text-sm text-muted-foreground">
                  No matching projects you can link.
                </p>
              );
            }}
          </Result>
        )}
      </DialogContent>
    </Dialog>
  );
}
