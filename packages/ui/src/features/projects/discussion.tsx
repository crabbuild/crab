import { useEffect, useState } from "react";
import { FolderKanbanIcon, PlusIcon, ListTreeIcon } from "lucide-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "@/components/ui/dialog";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { useProjectCommands, CommandError } from "./commands";
import { WorkPicker } from "./item";
import {
  blankItem,
  projectHref,
  workHref,
  type Work,
  type Project,
  type Item,
  type Listing,
  type Subject,
} from "./model";
export function DiscussionProjects({
  repo,
  path,
}: {
  repo: Repository;
  path: string;
}) {
  const resource = useRequest<{
    items: { project: Project; item: Item }[];
    can_manage: boolean;
  }>(endpoint(repo, `${path}/projects`));
  const [open, setOpen] = useState(false);
  const commands = useProjectCommands();
  useEffect(() => {
    if (commands.generation) resource.retry();
  }, [commands.generation, resource.retry]);
  const subject: Subject = {
    kind: path.startsWith("issues/") ? "issue" : "pull",
    number: Number(path.split("/")[1]),
  };
  return (
    <section className="discussion-projects">
      <header>
        <strong>Projects</strong>
        {resource.data?.can_manage && !repo.archived && (
          <Button
            size="icon-sm"
            variant="ghost"
            aria-label="Add to a project"
            onClick={() => setOpen(true)}
          >
            <PlusIcon />
          </Button>
        )}
      </header>
      <Result state={resource} timingLabel="Discussion projects">
        {(data) =>
          data.items.length ? (
            <ul>
              {data.items.map(({ project, item }) => (
                <li key={project.id}>
                  <Link href={projectHref(repo, project.id)}>
                    <FolderKanbanIcon />
                    {project.definition.name}
                  </Link>
                  <Badge variant="secondary">{item.status}</Badge>
                </li>
              ))}
            </ul>
          ) : (
            <p className="muted">Not in a project</p>
          )
        }
      </Result>
      {open && (
        <AddMembership
          repo={repo}
          subject={subject}
          onClose={() => setOpen(false)}
        />
      )}
    </section>
  );
}
function AddMembership({
  repo,
  subject,
  onClose,
}: {
  repo: Repository;
  subject: Subject;
  onClose: () => void;
}) {
  const [after, setAfter] = useState<string>();
  const [selected, setSelected] = useState<string>("");
  const projects = useRequest<Listing>(endpoint(repo, "projects", { after }));
  const commands = useProjectCommands();
  const project = projects.data?.items.find((p) => p.id === selected);
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent
        onEscapeKeyDown={(e) => {
          if (commands.locked) e.preventDefault();
        }}
        onInteractOutside={(e) => {
          if (commands.locked) e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>Add to a project</DialogTitle>
          <DialogDescription>
            The project will track the current issue or pull request.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-5"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!project) return;
            const result = await commands.submit(repo, {
              kind: "item",
              project: project.id,
              version: project.version,
              item: { ...blankItem(project.definition.statuses[0]), subject },
            });
            if (result?.outcome === "applied") onClose();
          }}
        >
          <Result state={projects}>
            {(data) => (
              <>
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="discussion-project">
                      Project
                    </FieldLabel>
                    <NativeSelect
                      id="discussion-project"
                      value={selected}
                      onChange={(e) => setSelected(e.target.value)}
                      disabled={commands.locked}
                    >
                      <NativeSelectOption value="">
                        Choose a project
                      </NativeSelectOption>
                      {data.items.map((p) => (
                        <NativeSelectOption key={p.id} value={p.id}>
                          {p.definition.name}
                        </NativeSelectOption>
                      ))}
                    </NativeSelect>
                  </Field>
                </FieldGroup>
                {data.next && (
                  <Button
                    type="button"
                    variant="outline"
                    disabled={commands.locked}
                    onClick={() => {
                      setSelected("");
                      setAfter(data.next!);
                    }}
                  >
                    More projects
                  </Button>
                )}
                {after && (
                  <Button
                    type="button"
                    variant="ghost"
                    onClick={() => {
                      setSelected("");
                      setAfter(undefined);
                    }}
                  >
                    First page
                  </Button>
                )}
                {!data.items.length && (
                  <p className="muted">
                    Create a project from the Projects tab first.
                  </p>
                )}
              </>
            )}
          </Result>
          <CommandError />
          <DialogFooter>
            <Button disabled={!project || commands.locked} type="submit">
              Add to project
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
interface Tree {
  parent: Work | null;
  version: number;
  progress: { total: number; closed: number };
  children: Work[];
  next: number | null;
}
export function SubIssues({
  repo,
  number,
}: {
  repo: Repository;
  number: number;
}) {
  const [after, setAfter] = useState<string>();
  const resource = useRequest<{ hierarchy: Tree; can_manage: boolean }>(
    endpoint(repo, `issues/${number}/hierarchy`, { after }),
  );
  const [mode, setMode] = useState<"parent" | "child">();
  const commands = useProjectCommands();
  useEffect(() => {
    if (commands.generation) {
      setAfter(undefined);
      resource.retry();
    }
  }, [commands.generation, resource.retry]);
  return (
    <Result state={resource} timingLabel="Sub-issues">
      {({ hierarchy: tree, can_manage }) => (
        <section className="project-sub-issues">
          <header>
            <div>
              <ListTreeIcon />
              <h2>Sub-issues</h2>
              <Badge variant="secondary">
                {tree.progress.closed}/{tree.progress.total}
              </Badge>
            </div>
            {can_manage && !repo.archived && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => setMode("child")}
                disabled={commands.locked}
              >
                <PlusIcon data-icon="inline-start" />
                Add sub-issue
              </Button>
            )}
          </header>
          {!!tree.progress.total && (
            <progress
              max={tree.progress.total}
              value={tree.progress.closed}
              aria-label="Closed sub-issues"
            />
          )}
          <div className="project-parent-row">
            {tree.parent ? (
              <span>
                Parent:{" "}
                <Link href={workHref(repo, tree.parent.subject)}>
                  #{tree.parent.subject.number} {tree.parent.title}
                </Link>
              </span>
            ) : (
              <span className="muted">No parent issue</span>
            )}
            {can_manage && !repo.archived && (
              <div className="flex gap-1">
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setMode("parent")}
                  disabled={commands.locked}
                >
                  {tree.parent ? "Change parent" : "Set parent"}
                </Button>
                {tree.parent && (
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={commands.locked}
                    onClick={() =>
                      void commands.submit(repo, {
                        kind: "parent",
                        child: number,
                        parent: null,
                        version: tree.version,
                      })
                    }
                  >
                    Remove parent
                  </Button>
                )}
              </div>
            )}
          </div>
          <ul>
            {tree.children.map((work) => (
              <li key={work.subject.number}>
                <Link href={workHref(repo, work.subject)}>
                  #{work.subject.number} {work.title}
                </Link>
                <Badge variant="outline">{work.state}</Badge>
              </li>
            ))}
          </ul>
          {after && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setAfter(undefined)}
            >
              First sub-issues
            </Button>
          )}
          {tree.next !== null && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setAfter(String(tree.next))}
            >
              More sub-issues
            </Button>
          )}
          {mode && (
            <HierarchyEditor
              repo={repo}
              number={number}
              tree={tree}
              mode={mode}
              onClose={() => setMode(undefined)}
            />
          )}
        </section>
      )}
    </Result>
  );
}
function HierarchyEditor({
  repo,
  number,
  tree,
  mode,
  onClose,
}: {
  repo: Repository;
  number: number;
  tree: Tree;
  mode: "parent" | "child";
  onClose: () => void;
}) {
  const [work, setWork] = useState<Work>();
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const commands = useProjectCommands();
  const selected = useRequest<{ hierarchy: Tree; can_manage: boolean }>(
    work && mode === "child"
      ? endpoint(repo, `issues/${work.subject.number}/hierarchy`)
      : null,
  );
  async function link() {
    if (!work) return;
    const child = mode === "child" ? work.subject.number : number;
    const parent = mode === "child" ? number : work.subject.number;
    const version =
      mode === "child" ? selected.data?.hierarchy.version : tree.version;
    if (version === undefined) return;
    const result = await commands.submit(repo, {
      kind: "parent",
      child,
      parent,
      version,
    });
    if (result?.outcome === "applied") onClose();
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent
        className="project-item-dialog"
        onEscapeKeyDown={(e) => {
          if (commands.locked) e.preventDefault();
        }}
        onInteractOutside={(e) => {
          if (commands.locked) e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>
            {mode === "child" ? "Add a sub-issue" : "Set parent issue"}
          </DialogTitle>
          <DialogDescription>
            Connect work under issue #{number}. Each issue has one parent.
          </DialogDescription>
        </DialogHeader>
        <Tabs defaultValue={mode === "child" ? "new" : "existing"}>
          {mode === "child" && (
            <TabsList>
              <TabsTrigger value="new">Create an issue</TabsTrigger>
              <TabsTrigger value="existing">Existing issue</TabsTrigger>
            </TabsList>
          )}
          <TabsContent value="new">
            <form
              className="flex flex-col gap-5"
              onSubmit={async (e) => {
                e.preventDefault();
                const result = await commands.submit(repo, {
                  kind: "sub_issue",
                  parent: number,
                  title: title.trim(),
                  body,
                });
                if (result?.outcome === "applied") onClose();
              }}
            >
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor="sub-issue-title">Title</FieldLabel>
                  <Input
                    id="sub-issue-title"
                    value={title}
                    onChange={(e) => setTitle(e.target.value)}
                    maxLength={256}
                    required
                    disabled={commands.locked}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor="sub-issue-description">
                    Description
                  </FieldLabel>
                  <Textarea
                    id="sub-issue-description"
                    value={body}
                    onChange={(e) => setBody(e.target.value)}
                    maxLength={16384}
                    disabled={commands.locked}
                  />
                </Field>
              </FieldGroup>
              <CommandError />
              <DialogFooter>
                <Button
                  type="submit"
                  disabled={commands.locked || !title.trim()}
                >
                  Create sub-issue
                </Button>
              </DialogFooter>
            </form>
          </TabsContent>
          <TabsContent value="existing">
            {!work ? (
              <WorkPicker repo={repo} kind="issue" onSelect={setWork} />
            ) : (
              <div className="flex flex-col gap-4">
                <p>
                  Selected{" "}
                  <strong>
                    #{work.subject.number} {work.title}
                  </strong>
                </p>
                {mode === "child" && (
                  <Result state={selected}>
                    {(data) => (
                      <p className="muted">
                        {data.hierarchy.parent
                          ? `This issue is currently under #${data.hierarchy.parent.subject.number}. Saving will move it under #${number}.`
                          : "This issue has no parent."}
                      </p>
                    )}
                  </Result>
                )}
                <Button
                  variant="outline"
                  disabled={commands.locked}
                  onClick={() => setWork(undefined)}
                >
                  Choose another issue
                </Button>
                <CommandError />
                <Button
                  disabled={
                    commands.locked || (mode === "child" && !selected.data)
                  }
                  onClick={() => void link()}
                >
                  {mode === "child"
                    ? "Save sub-issue relationship"
                    : "Save parent"}
                </Button>
              </div>
            )}
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
