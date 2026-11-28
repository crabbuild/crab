import { useEffect, useState, type ReactNode } from "react";
import {
  CheckIcon,
  GlobeIcon,
  InfoIcon,
  LockIcon,
  OrganizationIcon,
  SearchIcon,
  TriangleDownIcon,
  XIcon,
} from "@primer/octicons-react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSeparator,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { UserAvatar } from "@/components/ui/user-avatar";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import { useOrganizationDirectory } from "@/features/organizations/directory";
import {
  RepositoryPropertyFields,
  needsExplicitPropertyValues,
  type PropertyDefinition,
  type PropertyOverrides,
} from "@/features/organizations/property-fields";
import {
  navigate,
  useRequest,
  type Repository,
  type RepositoryVisibility,
} from "@/lib/api";
import type { AvatarConfig, Session } from "@/lib/contracts";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";
import { GITIGNORE_TEMPLATES, LICENSE_TEMPLATES } from "./init-templates";

type Destination = {
  owner: string;
  default_branch: string;
  visibilities: [RepositoryVisibility, ...RepositoryVisibility[]];
  properties?: PropertyDefinition[];
};
type RepositoryAddress = { id: string; owner: string; name: string };
type Submission = {
  request_id: string;
  name: string;
  description: string;
  visibility: RepositoryVisibility;
  properties?: PropertyOverrides;
  add_readme?: boolean;
  gitignore_template?: string;
  license_template?: string;
};
type Draft = { name: string; description: string };

const DESCRIPTION_LIMIT = 350;

const visibilityOptions = {
  public: {
    Icon: GlobeIcon,
    label: "Public",
    help: "Anyone can see and clone this repository. You choose who can commit.",
  },
  private: {
    Icon: LockIcon,
    label: "Private",
    help: "You choose who can see and commit to this repository.",
  },
} as const;

const flavors = ["brave", "bright", "cosmic", "crisp", "fuzzy", "lucky"];
const places = ["harbor", "kelp", "lagoon", "pebble", "reef", "tide"];

function suggestName() {
  const [flavor, place] = crypto.getRandomValues(new Uint32Array(2));
  return `${flavors[flavor % flavors.length]}-${places[place % places.length]}`;
}

// Like GitHub, replace characters the server rejects instead of refusing them.
function repositoryName(value: string) {
  return value.trim().replace(/[^A-Za-z0-9._-]+/g, "-");
}

function validName(name: string) {
  return name.length > 0 && name.length <= 100 && name !== "." && name !== "..";
}

function recover(key: string): Submission | undefined {
  try {
    const data = JSON.parse(
      sessionStorage.getItem(key) ?? "null",
    ) as Partial<Submission> | null;
    if (
      typeof data?.request_id === "string" &&
      typeof data?.name === "string" &&
      typeof data?.description === "string"
    )
      return {
        request_id: data.request_id,
        name: data.name,
        description: data.description,
        visibility: data.visibility === "public" ? "public" : "private",
        properties: data.properties,
        ...(data.add_readme === true ? { add_readme: true } : {}),
        ...(typeof data.gitignore_template === "string" &&
        data.gitignore_template
          ? { gitignore_template: data.gitignore_template }
          : {}),
        ...(typeof data.license_template === "string" && data.license_template
          ? { license_template: data.license_template }
          : {}),
      };
  } catch {
    // Browsers can disable session storage.
  }
}

function OrganizationAvatar({ size }: { size: number }) {
  return (
    <span
      className="inline-flex shrink-0 items-center justify-center rounded-md border bg-muted text-muted-foreground"
      style={{ width: size, height: size }}
      aria-hidden="true"
    >
      <OrganizationIcon size={Math.round(size * 0.6)} />
    </span>
  );
}

function OwnerPicker({
  personal,
  avatar,
  organizations,
  selected,
  onSelect,
}: {
  personal: string;
  avatar: AvatarConfig | null;
  organizations: OrganizationMembershipEntry[];
  selected?: OrganizationMembershipEntry;
  onSelect: (organization?: string) => void;
}) {
  const label = selected?.organization.name ?? personal;
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button
          id="new-repository-owner"
          type="button"
          variant="outline"
          className="h-8 max-w-64 justify-start gap-2 px-2.5"
        >
          {selected ? (
            <OrganizationAvatar size={18} />
          ) : (
            <UserAvatar name={personal} avatar={avatar} size={18} />
          )}
          <span className="truncate">{label}</span>
          <TriangleDownIcon aria-hidden="true" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-64">
        <DropdownMenuLabel>Choose an owner</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => onSelect(undefined)}>
          <UserAvatar name={personal} avatar={avatar} size={20} />
          <span className="min-w-0 flex-1 truncate">{personal}</span>
          {!selected && <CheckIcon aria-label="Selected" />}
        </DropdownMenuItem>
        {organizations.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuLabel>Organizations</DropdownMenuLabel>
            {organizations.map((entry) => (
              <DropdownMenuItem
                key={entry.organization.id}
                onSelect={() => onSelect(entry.organization.id)}
              >
                <OrganizationAvatar size={20} />
                <span className="min-w-0 flex-1 truncate">
                  {entry.organization.name}
                </span>
                {selected?.organization.id === entry.organization.id && (
                  <CheckIcon aria-label="Selected" />
                )}
              </DropdownMenuItem>
            ))}
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function TemplatePicker({
  id,
  labelId,
  emptyLabel,
  pickerTitle,
  searchPlaceholder,
  selected,
  options,
  disabled,
  onSelect,
}: {
  id: string;
  labelId: string;
  emptyLabel: string;
  pickerTitle: string;
  searchPlaceholder: string;
  selected?: string;
  options: ReadonlyArray<{ value: string; label: string }>;
  disabled: boolean;
  onSelect: (value?: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const needle = filter.trim().toLowerCase();
  const filtered = needle
    ? options.filter(
        (item) =>
          item.label.toLowerCase().includes(needle) ||
          item.value.toLowerCase().includes(needle),
      )
    : options;
  const active = options.find((item) => item.value === selected);

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setFilter("");
      }}
    >
      <PopoverTrigger asChild>
        <Button
          id={id}
          type="button"
          variant="outline"
          size="default"
          className="h-8 max-w-64 shrink-0 justify-between gap-2 px-3 font-normal"
          aria-labelledby={labelId}
          aria-expanded={open}
          disabled={disabled}
        >
          <span className="truncate">{active?.label ?? emptyLabel}</span>
          <TriangleDownIcon aria-hidden="true" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-72 gap-0 p-0">
        <div className="border-b px-3 py-2 text-xs font-semibold text-foreground">
          {pickerTitle}
        </div>
        <div className="relative border-b p-2">
          <SearchIcon
            size={14}
            className="pointer-events-none absolute top-1/2 left-4.5 -translate-y-1/2 text-muted-foreground"
            aria-hidden="true"
          />
          <Input
            value={filter}
            placeholder={searchPlaceholder}
            aria-label={searchPlaceholder}
            className="h-7 pl-7 text-xs"
            autoFocus
            onChange={(event) => setFilter(event.target.value)}
          />
        </div>
        <div
          className="max-h-56 overflow-y-auto p-1"
          role="listbox"
          aria-label={pickerTitle}
        >
          {!needle && (
            <button
              type="button"
              role="option"
              aria-selected={!selected}
              className={cn(
                "flex w-full items-center gap-2 rounded-sm px-2 py-1.5 text-left text-sm transition-colors hover:bg-accent hover:text-accent-foreground",
                !selected && "font-medium",
              )}
              onClick={() => {
                onSelect(undefined);
                setOpen(false);
                setFilter("");
              }}
            >
              <span className="inline-flex w-4 shrink-0 items-center justify-center">
                {!selected && <CheckIcon size={14} aria-hidden="true" />}
              </span>
              <span className="truncate">{emptyLabel}</span>
            </button>
          )}
          {filtered.map((item) => {
            const isSelected = selected === item.value;
            return (
              <button
                key={item.value}
                type="button"
                role="option"
                aria-selected={isSelected}
                className={cn(
                  "flex w-full items-center gap-2 rounded-sm px-2 py-1.5 text-left text-sm transition-colors hover:bg-accent hover:text-accent-foreground",
                  isSelected && "bg-accent/50 font-medium",
                )}
                onClick={() => {
                  onSelect(item.value);
                  setOpen(false);
                  setFilter("");
                }}
              >
                <span className="inline-flex w-4 shrink-0 items-center justify-center">
                  {isSelected && <CheckIcon size={14} aria-hidden="true" />}
                </span>
                <span className="min-w-0 flex-1 wrap-anywhere">
                  {item.label}
                </span>
              </button>
            );
          })}
          {filtered.length === 0 && (
            <p className="px-2 py-3 text-center text-xs text-muted-foreground">
              No matching options
            </p>
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}

function OwnerField({ children }: { children: ReactNode }) {
  return (
    <Field className="w-auto">
      <FieldLabel htmlFor="new-repository-owner">
        Owner <span aria-hidden="true">*</span>
      </FieldLabel>
      {children}
    </Field>
  );
}

function Availability({
  owner,
  label,
  name,
}: {
  owner: string;
  label: string;
  name: string;
}) {
  const [checked, setChecked] = useState(name);
  useEffect(() => {
    const timer = setTimeout(() => setChecked(name), 300);
    return () => clearTimeout(timer);
  }, [name]);
  const lookup = useRequest<{ repositories: Repository[] }>(
    validName(checked)
      ? `/api/repos?${new URLSearchParams({ owner, name: checked })}`
      : null,
  );
  if (checked !== name || !lookup.data) return null;
  const taken = lookup.data.repositories.length > 0;
  return (
    <p
      role="status"
      className={cn(
        "flex items-center gap-1.5 text-sm",
        taken ? "text-destructive" : "text-(--success)",
      )}
    >
      {taken ? <XIcon aria-hidden="true" /> : <CheckIcon aria-hidden="true" />}
      {taken ? (
        <span>
          The repository{" "}
          <strong>
            {label}/{name}
          </strong>{" "}
          already exists on this account.
        </span>
      ) : (
        <span>
          <strong>
            {label}/{name}
          </strong>{" "}
          is available.
        </span>
      )}
    </p>
  );
}

function RepositoryForm({
  destination,
  owner,
  organization,
  endpoint,
  account,
  csrf,
  draft,
  onDraft,
}: {
  destination: Destination;
  owner: ReactNode;
  organization?: string;
  endpoint: string;
  account: string;
  csrf: string;
  draft: Draft;
  onDraft: (draft: Draft) => void;
}) {
  const key = `crab:repository-creation:${destination.owner}:${account}`;
  const [attempt, setAttempt] = useState(() => recover(key));
  const [name, setName] = useState(attempt?.name ?? draft.name);
  const [description, setDescription] = useState(
    attempt?.description ?? draft.description,
  );
  const [visibility, setVisibility] = useState(
    attempt?.visibility ?? destination.visibilities[0],
  );
  const [addReadme, setAddReadme] = useState(attempt?.add_readme ?? false);
  const [gitignoreTemplate, setGitignoreTemplate] = useState(
    attempt?.gitignore_template,
  );
  const [licenseTemplate, setLicenseTemplate] = useState(
    attempt?.license_template,
  );
  const [properties, setProperties] = useState(attempt?.properties ?? {});
  const [suggestion] = useState(suggestName);
  const definitions = destination.properties ?? [];
  const propertyIds = Object.keys(properties);
  const availableProperties = Object.fromEntries(
    Object.entries(properties).filter(([id]) =>
      definitions.some((definition) => definition.id === id),
    ),
  );
  const hasUnavailable =
    propertyIds.length > Object.keys(availableProperties).length;
  const available = destination.visibilities.includes(visibility);
  const fields = {
    name: repositoryName(name),
    description: description.trim(),
    visibility,
    ...(addReadme ? { add_readme: true } : {}),
    ...(gitignoreTemplate ? { gitignore_template: gitignoreTemplate } : {}),
    ...(licenseTemplate ? { license_template: licenseTemplate } : {}),
  };
  const savedProperties = attempt?.properties ?? {};
  const retry =
    attempt?.name === fields.name &&
    attempt.description === fields.description &&
    attempt.visibility === visibility &&
    Boolean(attempt.add_readme) === addReadme &&
    attempt.gitignore_template === gitignoreTemplate &&
    attempt.license_template === licenseTemplate &&
    propertyIds.length === Object.keys(savedProperties).length &&
    propertyIds.every(
      (id) =>
        JSON.stringify(properties[id]) === JSON.stringify(savedProperties[id]),
    );
  const incomplete =
    !retry && needsExplicitPropertyValues(definitions, availableProperties);
  const named = validName(fields.name);
  const label = organization ?? destination.owner;
  const mutation = useMutation(csrf, null);
  const edit = (next: Draft) => {
    setName(next.name);
    setDescription(next.description);
    onDraft(next);
  };
  return (
    <form
      className="mt-6"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!named || !available || incomplete || mutation.pending) return;
        const next = retry
          ? attempt
          : {
              ...fields,
              request_id: crypto.randomUUID(),
              ...(Object.keys(availableProperties).length
                ? { properties: availableProperties }
                : {}),
            };
        setAttempt(next);
        setProperties(next.properties ?? {});
        try {
          sessionStorage.setItem(key, JSON.stringify(next));
        } catch {
          // The form still retains its submission during this browser session.
        }
        const created = await mutation.run<RepositoryAddress>(
          endpoint,
          "POST",
          next,
        );
        if (created) {
          try {
            sessionStorage.removeItem(key);
          } catch {
            // Storage was already unavailable.
          }
          navigate(repoHref(created));
        }
      }}
    >
      <FieldGroup>
        <div className="grid gap-2">
          <div className="flex flex-wrap items-end gap-x-2 gap-y-3">
            {owner}
            <span
              className="hidden pb-1 text-xl leading-6 text-muted-foreground sm:block"
              aria-hidden="true"
            >
              /
            </span>
            <Field className="min-w-56 flex-1">
              <FieldLabel htmlFor="new-repository-name">
                Repository name <span aria-hidden="true">*</span>
              </FieldLabel>
              <Input
                id="new-repository-name"
                required
                autoComplete="off"
                autoFocus
                maxLength={100}
                value={name}
                disabled={mutation.pending}
                aria-describedby="new-repository-name-help"
                onChange={(event) =>
                  edit({ name: event.target.value, description })
                }
              />
            </Field>
          </div>
          {named && fields.name !== name.trim() && (
            <p className="flex items-center gap-1.5 text-sm text-muted-foreground">
              <InfoIcon aria-hidden="true" />
              <span>
                Your new repository will be created as{" "}
                <strong>{fields.name}</strong>.
              </span>
            </p>
          )}
          {named && (
            <Availability
              owner={destination.owner}
              label={label}
              name={fields.name}
            />
          )}
          {/^user-[0-9a-f]{32}$/i.test(destination.owner) && (
            <p className="flex items-start gap-1.5 text-sm text-muted-foreground">
              <InfoIcon className="mt-0.5" aria-hidden="true" />
              <span>
                Set a username in{" "}
                <Link
                  href="/-/settings/profile"
                  className="font-medium text-foreground underline underline-offset-4"
                >
                  Settings › Profile
                </Link>{" "}
                to create repositories under your username.
              </span>
            </p>
          )}
          <p
            id="new-repository-name-help"
            className="text-sm text-muted-foreground"
          >
            Great repository names are short and memorable. Need inspiration?
            How about{" "}
            <button
              type="button"
              className="font-semibold text-(--success) hover:underline"
              disabled={mutation.pending}
              onClick={() => edit({ name: suggestion, description })}
            >
              {suggestion}
            </button>
            ?
          </p>
        </div>
        <Field>
          <FieldLabel htmlFor="new-repository-description">
            Description (optional)
          </FieldLabel>
          <Input
            id="new-repository-description"
            maxLength={DESCRIPTION_LIMIT}
            value={description}
            disabled={mutation.pending}
            aria-describedby="new-repository-description-count"
            onChange={(event) =>
              edit({ name, description: event.target.value })
            }
          />
          <FieldDescription id="new-repository-description-count">
            {description.length}/{DESCRIPTION_LIMIT} characters
          </FieldDescription>
        </Field>
        <FieldSeparator />
        <FieldSet>
          <FieldLegend id="new-repository-visibility" variant="label">
            Visibility <span aria-hidden="true">*</span>
          </FieldLegend>
          <RadioGroup
            aria-labelledby="new-repository-visibility"
            value={visibility}
            disabled={mutation.pending}
            className="gap-0 divide-y overflow-hidden rounded-md border"
            onValueChange={(value) => {
              setVisibility(value === "public" ? "public" : "private");
            }}
          >
            {destination.visibilities.map((value) => {
              const id = `new-repository-${value}`;
              const { Icon, label, help } = visibilityOptions[value];
              return (
                <label
                  key={value}
                  htmlFor={id}
                  className={cn(
                    "flex cursor-pointer items-start gap-3 px-3 py-3 transition-colors",
                    visibility === value
                      ? "bg-accent/50"
                      : "hover:bg-accent/30",
                  )}
                >
                  <RadioGroupItem
                    id={id}
                    value={value}
                    className="mt-1"
                    aria-labelledby={`${id}-label`}
                    aria-describedby={`${id}-help`}
                  />
                  <Icon
                    size={24}
                    className="shrink-0 text-muted-foreground"
                    aria-hidden="true"
                  />
                  <span className="grid gap-0.5">
                    <span id={`${id}-label`} className="text-sm font-semibold">
                      {label}
                    </span>
                    <span
                      id={`${id}-help`}
                      className="text-sm text-muted-foreground"
                    >
                      {help}
                    </span>
                  </span>
                </label>
              );
            })}
          </RadioGroup>
          {!available && (
            <FieldError>
              Your saved visibility is no longer available. Choose an allowed
              visibility to continue.
            </FieldError>
          )}
        </FieldSet>
        <FieldSet>
          <FieldLegend variant="label">
            Initialize this repository with
          </FieldLegend>
          <div className="divide-y rounded-md border">
            <div className="flex flex-wrap items-center justify-between gap-3 px-3 py-3">
              <div className="grid min-w-0 gap-0.5">
                <span
                  id="new-repository-readme-label"
                  className="text-sm font-semibold"
                >
                  Add README
                </span>
                <span className="text-sm text-muted-foreground">
                  READMEs can be used as longer descriptions.
                </span>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <span className="text-xs font-medium text-muted-foreground">
                  {addReadme ? "On" : "Off"}
                </span>
                <button
                  type="button"
                  role="switch"
                  aria-checked={addReadme}
                  aria-labelledby="new-repository-readme-label"
                  disabled={mutation.pending}
                  className={cn(
                    "relative inline-flex h-6 w-11 shrink-0 cursor-pointer items-center rounded-full border transition-colors outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/25 disabled:pointer-events-none disabled:opacity-50",
                    addReadme
                      ? "border-primary bg-primary"
                      : "border-border bg-muted",
                  )}
                  onClick={() => setAddReadme((value) => !value)}
                >
                  <span
                    className={cn(
                      "pointer-events-none inline-block size-4 rounded-full shadow-xs transition-transform",
                      addReadme
                        ? "translate-x-5.5 bg-primary-foreground"
                        : "translate-x-0.5 bg-foreground/60",
                    )}
                  />
                </button>
              </div>
            </div>
            <div className="flex flex-wrap items-center justify-between gap-3 px-3 py-3">
              <div className="grid min-w-0 gap-0.5">
                <span
                  id="new-repository-gitignore-label"
                  className="text-sm font-semibold"
                >
                  Add .gitignore
                </span>
                <span className="text-sm text-muted-foreground">
                  .gitignore tells git which files not to track.
                </span>
              </div>
              <TemplatePicker
                id="new-repository-gitignore"
                labelId="new-repository-gitignore-label"
                emptyLabel="No .gitignore"
                pickerTitle="Choose a .gitignore template"
                searchPlaceholder="Search templates"
                selected={gitignoreTemplate}
                options={GITIGNORE_TEMPLATES.map((name) => ({
                  value: name,
                  label: name,
                }))}
                disabled={mutation.pending}
                onSelect={setGitignoreTemplate}
              />
            </div>
            <div className="flex flex-wrap items-center justify-between gap-3 px-3 py-3">
              <div className="grid min-w-0 gap-0.5">
                <span
                  id="new-repository-license-label"
                  className="text-sm font-semibold"
                >
                  Add license
                </span>
                <span className="text-sm text-muted-foreground">
                  Licenses explain how others can use your code.
                </span>
              </div>
              <TemplatePicker
                id="new-repository-license"
                labelId="new-repository-license-label"
                emptyLabel="No license"
                pickerTitle="Choose a license"
                searchPlaceholder="Search licenses"
                selected={licenseTemplate}
                options={LICENSE_TEMPLATES.map(({ spdx, name }) => ({
                  value: spdx,
                  label: name,
                }))}
                disabled={mutation.pending}
                onSelect={setLicenseTemplate}
              />
            </div>
          </div>
        </FieldSet>
        {(definitions.length > 0 || hasUnavailable) && <FieldSeparator />}
        <RepositoryPropertyFields
          definitions={definitions}
          values={properties}
          onChange={setProperties}
          disabled={mutation.pending}
        />
        {hasUnavailable && (
          <Button
            type="button"
            variant="outline"
            className="self-start"
            disabled={mutation.pending}
            onClick={() => setProperties(availableProperties)}
          >
            Clear unavailable property values
          </Button>
        )}
        <FieldSeparator />
        <p className="flex items-start gap-2 text-sm text-muted-foreground">
          <InfoIcon className="mt-0.5" aria-hidden="true" />
          <span>
            You are creating a {visibility} repository{" "}
            {organization ? (
              <>
                in the <strong>{organization}</strong> organization
              </>
            ) : (
              "in your personal account"
            )}
            . Its default branch will be{" "}
            <code>{destination.default_branch}</code>
            {(addReadme || gitignoreTemplate || licenseTemplate) && (
              <>
                , initialized with{" "}
                {[
                  addReadme && "README.md",
                  gitignoreTemplate && `.gitignore (${gitignoreTemplate})`,
                  licenseTemplate && `LICENSE (${licenseTemplate})`,
                ]
                  .filter(Boolean)
                  .join(", ")}
              </>
            )}
            .
          </span>
        </p>
        {mutation.error && <FieldError>{mutation.error}</FieldError>}
        <div className="flex justify-end">
          <Button
            type="submit"
            disabled={!named || !available || incomplete || mutation.pending}
          >
            {mutation.pending ? "Creating…" : "Create repository"}
          </Button>
        </div>
      </FieldGroup>
    </form>
  );
}

/** GitHub-style page for creating a repository owned by the viewer or an organization. */
export function NewRepository({
  session,
  avatar,
  url,
}: {
  session: Session;
  avatar: AvatarConfig | null;
  url: URL;
}) {
  const directory = useOrganizationDirectory();
  const organizations = (directory.data?.items ?? []).filter(
    (entry): entry is OrganizationMembershipEntry =>
      entry.kind === "member" && entry.can_create,
  );
  const [chosen, setChosen] = useState(
    () => url.searchParams.get("organization") ?? undefined,
  );
  const organization = organizations.find(
    (entry) => entry.organization.id === chosen,
  );
  // An organization from the URL waits for the directory. An ineligible
  // one falls back to the personal account instead of a failing request.
  const resolving = Boolean(chosen && !organization && directory.loading);
  const personal = useRequest<Destination>("/api/repos/creation");
  const endpoint = organization
    ? `/api/organizations/${encodeURIComponent(organization.organization.id)}/repositories/creation`
    : "/api/repos/creation";
  const owned = useRequest<Destination>(
    organization && !resolving ? endpoint : null,
  );
  const destination = organization ? owned : personal;
  const [draft, setDraft] = useState<Draft>({ name: "", description: "" });
  const personalName =
    personal.data?.owner ?? session.user?.name ?? "Personal account";
  const account =
    organization?.actor ??
    (session.user
      ? JSON.stringify([session.user.issuer, session.user.subject])
      : "local");
  const picker = (
    <OwnerField>
      <OwnerPicker
        personal={personalName}
        avatar={avatar}
        organizations={organizations}
        selected={organization}
        onSelect={setChosen}
      />
    </OwnerField>
  );
  return (
    <div className="mx-auto w-full max-w-3xl px-4 py-8 sm:px-6">
      <header className="border-b pb-4">
        <h1 className="text-2xl font-semibold">Create a new repository</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          A repository contains all project files, including the revision
          history. Already have a project repository elsewhere?{" "}
          <Link
            href="/-/repositories"
            className="font-medium text-foreground underline underline-offset-4"
          >
            Import a repository.
          </Link>
        </p>
      </header>
      <p className="mt-4 text-xs text-muted-foreground italic">
        Required fields are marked with an asterisk (*).
      </p>
      <Result
        state={resolving ? { loading: true, retry: () => {} } : destination}
        timingLabel="Repository owner"
      >
        {(data) => (
          <RepositoryForm
            key={`${data.owner}:${account}`}
            destination={data}
            owner={picker}
            organization={organization?.organization.name}
            endpoint={endpoint}
            account={account}
            csrf={session.csrf ?? ""}
            draft={draft}
            onDraft={setDraft}
          />
        )}
      </Result>
    </div>
  );
}
