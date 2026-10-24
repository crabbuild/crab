import { useState, useRef, useEffect } from "react";
import { PlusIcon, LayoutGridIcon, SearchIcon } from "lucide-react";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogTrigger,
  DialogFooter,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { useProjectCommands, CommandError } from "./commands";
import {
  projectHref,
  type Listing,
  type Template,
  type Definition,
} from "./model";
export function TemplatePreview({ definition }: { definition: Definition }) {
  return (
    <div className="project-template-preview" aria-hidden="true">
      <div className="project-preview-toolbar">
        <LayoutGridIcon /> {definition.views[0].name}
      </div>
      <div className="project-preview-columns">
        {definition.statuses.slice(0, 3).map((status) => (
          <div key={status}>
            <span>{status}</span>
            <i />
            <i />
          </div>
        ))}
      </div>
    </div>
  );
}
export function CreateProject({ repo }: { repo: Repository }) {
  const [open, setOpen] = useState(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const [name, setName] = useState("");
  const [choice, setChoice] = useState("builtin:team-planning");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const templates = useRequest<{ items: Template[] }>(
    open ? endpoint(repo, "projects/templates") : null,
  );
  const custom = useRequest<Listing>(
    open ? endpoint(repo, "projects", { templates: "true", after }) : null,
  );
  const commands = useProjectCommands();
  async function create() {
    let command;
    if (choice.startsWith("builtin:"))
      command = {
        kind: "create" as const,
        id: crypto.randomUUID(),
        name: name.trim(),
        template: choice.slice(8),
      };
    else {
      const source = custom.data?.items.find((p) => p.id === choice);
      if (!source) return;
      command = {
        kind: "copy" as const,
        id: crypto.randomUUID(),
        source: source.id,
        version: source.version,
        name: name.trim(),
      };
    }
    const result = await commands.submit(repo, command);
    if (mounted.current && result?.outcome === "applied" && result.project) {
      setOpen(false);
      navigate(projectHref(repo, result.project));
    }
  }
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!commands.locked) setOpen(value);
      }}
    >
      <DialogTrigger asChild>
        <Button>
          <PlusIcon data-icon="inline-start" />
          New project
        </Button>
      </DialogTrigger>
      <DialogContent
        className="project-template-dialog"
        onEscapeKeyDown={(e) => {
          if (commands.locked) e.preventDefault();
        }}
        onInteractOutside={(e) => {
          if (commands.locked) e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>Create a project</DialogTitle>
          <DialogDescription>
            Bring issues, sub-issues, and pull requests into one plan.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void create();
          }}
          className="flex min-h-0 flex-col gap-5"
        >
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="project-name">Project name</FieldLabel>
              <Input
                id="project-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                maxLength={100}
                disabled={commands.locked}
                placeholder="e.g. Next release"
                required
              />
            </Field>
          </FieldGroup>
          <InputGroup>
            <InputGroupAddon>
              <SearchIcon />
            </InputGroupAddon>
            <InputGroupInput
              aria-label="Search project templates"
              placeholder="Search templates…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </InputGroup>
          <Tabs defaultValue="featured" className="min-h-0">
            <TabsList>
              <TabsTrigger value="featured">Templates</TabsTrigger>
              <TabsTrigger value="custom">From this repository</TabsTrigger>
            </TabsList>
            <TabsContent value="featured" className="project-template-scroll">
              <Result state={templates}>
                {(data) => (
                  <RadioGroup
                    value={choice}
                    onValueChange={setChoice}
                    disabled={commands.locked}
                    className="project-template-grid"
                  >
                    {data.items
                      .filter((t) =>
                        `${t.definition.name} ${t.description}`
                          .toLowerCase()
                          .includes(search.toLowerCase()),
                      )
                      .map((t) => (
                        <label className="project-template-choice" key={t.id}>
                          <TemplatePreview definition={t.definition} />
                          <div>
                            <RadioGroupItem
                              value={`builtin:${t.id}`}
                              aria-label={t.definition.name}
                            />
                            <strong>{t.definition.name}</strong>
                            <p>{t.description}</p>
                          </div>
                        </label>
                      ))}
                  </RadioGroup>
                )}
              </Result>
            </TabsContent>
            <TabsContent value="custom" className="project-template-scroll">
              <Result state={custom}>
                {(data) => (
                  <>
                    <RadioGroup
                      value={choice}
                      onValueChange={setChoice}
                      disabled={commands.locked}
                      className="project-template-grid"
                    >
                      {data.items
                        .filter((p) =>
                          p.definition.name
                            .toLowerCase()
                            .includes(search.toLowerCase()),
                        )
                        .map((p) => (
                          <label className="project-template-choice" key={p.id}>
                            <TemplatePreview definition={p.definition} />
                            <div>
                              <RadioGroupItem
                                value={p.id}
                                aria-label={p.definition.name}
                              />
                              <strong>{p.definition.name}</strong>
                              <p>{p.definition.description}</p>
                            </div>
                          </label>
                        ))}
                    </RadioGroup>
                    {!data.items.length && (
                      <p className="muted p-4">
                        Mark a project as a template in its settings to reuse
                        its workflow, fields, and views.
                      </p>
                    )}
                    {data.next && (
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() => setAfter(data.next!)}
                      >
                        More templates
                      </Button>
                    )}
                  </>
                )}
              </Result>
            </TabsContent>
          </Tabs>
          <CommandError />
          <DialogFooter>
            <Button type="submit" disabled={!name.trim() || commands.locked}>
              {commands.locked ? "Creating…" : "Create project"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
