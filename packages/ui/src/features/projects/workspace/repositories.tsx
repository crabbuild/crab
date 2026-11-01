import { useState } from "react";
import { FolderGit2Icon, LinkIcon } from "lucide-react";
import { useRequest, type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  InputGroup,
  InputGroupInput,
  InputGroupAddon,
} from "@/components/ui/input-group";
import { SearchIcon } from "lucide-react";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { CommandError, useProjectCommands } from "../commands";
import type { Detail } from "./model";

export function Repositories({
  organization,
  detail,
  disabled,
}: {
  organization: string;
  detail: Detail;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const catalog = useRequest<{ repositories: Repository[] }>(
    open ? "/api/repos" : null,
  );
  const commands = useProjectCommands();
  return (
    <section
      className="workspace-repositories"
      aria-label="Linked repositories"
    >
      <header>
        <h2>Repositories</h2>
        {detail.can_manage && !detail.project.archived && (
          <Button
            size="sm"
            variant="ghost"
            disabled={commands.locked || disabled}
            onClick={() => setOpen(true)}
          >
            <LinkIcon data-icon="inline-start" />
            Manage
          </Button>
        )}
      </header>
      {detail.repositories.map((repo) => (
        <Link
          className="workspace-repository"
          key={repo.id}
          href={repoHref(repo)}
        >
          <FolderGit2Icon aria-hidden="true" />
          <span>
            {repo.owner}/<strong>{repo.name}</strong>
          </span>
        </Link>
      ))}
      {!detail.repositories.length && (
        <p className="text-sm text-muted-foreground">
          Link repositories to bring their issues and pull requests into this
          project.
        </p>
      )}
      {!!detail.restricted_repositories && (
        <p className="text-sm text-muted-foreground">
          {detail.restricted_repositories} linked{" "}
          {detail.restricted_repositories === 1
            ? "repository requires"
            : "repositories require"}{" "}
          additional access.
        </p>
      )}
      <Dialog
        open={open}
        onOpenChange={(open) => {
          if (!commands.locked) setOpen(open);
        }}
      >
        <DialogContent className="sm:max-w-xl">
          <DialogHeader>
            <DialogTitle>Link repositories</DialogTitle>
            <DialogDescription>
              Plan work across repositories. Linking a repository preserves its
              existing access rules.
            </DialogDescription>
          </DialogHeader>
          <InputGroup>
            <InputGroupAddon>
              <SearchIcon />
            </InputGroupAddon>
            <InputGroupInput
              aria-label="Search repositories"
              placeholder="Find a repository…"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </InputGroup>
          <Result state={catalog}>
            {({ repositories }) => {
              const matches = repositories.filter((repo) =>
                `${repo.owner}/${repo.name}`
                  .toLocaleLowerCase()
                  .includes(query.toLocaleLowerCase()),
              );
              return (
                <div className="max-h-80 overflow-y-auto">
                  {matches.map((repo) => {
                    const linked = detail.repositories.some(
                      (current) => current.id === repo.id,
                    );
                    return (
                      <Field
                        orientation="horizontal"
                        key={repo.id}
                        className="workspace-repository-choice"
                      >
                        <Checkbox
                          id={`link-${repo.id}`}
                          checked={linked}
                          disabled={commands.locked || disabled}
                          onCheckedChange={(checked) =>
                            void commands.submitOrganization(organization, {
                              kind: "repository",
                              project: detail.project.id,
                              version: detail.project.version,
                              repository: repo.id,
                              linked: checked === true,
                            })
                          }
                        />
                        <FieldLabel htmlFor={`link-${repo.id}`}>
                          <FolderGit2Icon aria-hidden="true" />
                          {repo.owner}/{repo.name}
                        </FieldLabel>
                      </Field>
                    );
                  })}
                  {!matches.length && (
                    <p className="p-3 text-sm text-muted-foreground">
                      No accessible repositories match this search.
                    </p>
                  )}
                </div>
              );
            }}
          </Result>
          <CommandError />
          <Button
            variant="outline"
            disabled={commands.locked}
            onClick={() => setOpen(false)}
          >
            Done
          </Button>
        </DialogContent>
      </Dialog>
    </section>
  );
}
