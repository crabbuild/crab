import { useState, type ReactNode } from "react";
import { RepoIcon } from "@primer/octicons-react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  RepositoryPropertyFields,
  needsExplicitPropertyValues,
  type PropertyDefinition,
  type PropertyOverrides,
} from "@/features/organizations/property-fields";
import { useRequest, type RepositoryVisibility } from "@/lib/api";

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
};

const visibilityOptions = {
  private: ["Private", "Only people with access can view this repository."],
  public: ["Public", "Anyone can view and clone this repository."],
} as const;

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
      };
  } catch {
    // Browsers can disable session storage.
  }
}

function RepositoryForm({
  destination,
  endpoint,
  account,
  csrf,
  onCreated,
}: {
  destination: Destination;
  endpoint: string;
  account?: string;
  csrf: string;
  onCreated: (repository: RepositoryAddress) => void;
}) {
  const key = `crab:repository-creation:${destination.owner}${account ? `:${account}` : ""}`;
  const [attempt, setAttempt] = useState(() => recover(key));
  const [name, setName] = useState(attempt?.name ?? "");
  const [description, setDescription] = useState(attempt?.description ?? "");
  const [visibility, setVisibility] = useState(
    attempt?.visibility ?? destination.visibilities[0],
  );
  const [properties, setProperties] = useState(attempt?.properties ?? {});
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
    name: name.trim(),
    description: description.trim(),
    visibility,
  };
  const savedProperties = attempt?.properties ?? {};
  const retry =
    attempt?.name === fields.name &&
    attempt.description === fields.description &&
    attempt.visibility === visibility &&
    propertyIds.length === Object.keys(savedProperties).length &&
    propertyIds.every(
      (id) =>
        JSON.stringify(properties[id]) === JSON.stringify(savedProperties[id]),
    );
  const incomplete =
    !retry && needsExplicitPropertyValues(definitions, availableProperties);
  const mutation = useMutation(csrf, null);
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (!fields.name || !available || incomplete || mutation.pending)
          return;
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
          onCreated(created);
        }
      }}
    >
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="new-repository-name">Repository name</FieldLabel>
          <Input
            id="new-repository-name"
            required
            autoComplete="off"
            maxLength={100}
            pattern={String.raw`[A-Za-z0-9._\-]+`}
            value={name}
            disabled={mutation.pending}
            onChange={(event) => setName(event.target.value)}
          />
        </Field>
        <Field>
          <FieldLabel htmlFor="new-repository-description">
            Description (optional)
          </FieldLabel>
          <Input
            id="new-repository-description"
            maxLength={350}
            value={description}
            disabled={mutation.pending}
            onChange={(event) => setDescription(event.target.value)}
          />
        </Field>
        <FieldSet>
          <FieldLegend id="new-repository-visibility" variant="label">
            Visibility
          </FieldLegend>
          <RadioGroup
            aria-labelledby="new-repository-visibility"
            value={visibility}
            disabled={mutation.pending}
            onValueChange={(value) => {
              setVisibility(value === "public" ? "public" : "private");
            }}
          >
            {destination.visibilities.map((value) => {
              const id = `new-repository-${value}`;
              const [label, description] = visibilityOptions[value];
              return (
                <Field key={value} orientation="horizontal">
                  <RadioGroupItem
                    id={id}
                    value={value}
                    aria-describedby={`${id}-help`}
                  />
                  <FieldContent>
                    <FieldLabel htmlFor={id}>{label}</FieldLabel>
                    <FieldDescription id={`${id}-help`}>
                      {description}
                    </FieldDescription>
                  </FieldContent>
                </Field>
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
        <p className="text-sm wrap-anywhere text-muted-foreground">
          <code>
            {destination.owner}/{name || "repository"}
          </code>{" "}
          · Default branch <code>{destination.default_branch}</code>
        </p>
        {mutation.error && <FieldError>{mutation.error}</FieldError>}
        <div className="flex justify-end">
          <Button
            type="submit"
            disabled={
              !fields.name || !available || incomplete || mutation.pending
            }
          >
            {mutation.pending ? "Creating…" : "Create repository"}
          </Button>
        </div>
      </FieldGroup>
    </form>
  );
}

export function CreateRepository({
  organization,
  csrf,
  onCreated,
  open: controlledOpen,
  onOpenChange: controlledOnOpenChange,
  trigger,
}: {
  organization?: { id: string; name: string; actor: string };
  csrf: string;
  onCreated: (repository: RepositoryAddress) => void;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  trigger?: ReactNode | null;
}) {
  const [uncontrolledOpen, setUncontrolledOpen] = useState(false);
  const open = controlledOpen ?? uncontrolledOpen;
  const setOpen = controlledOnOpenChange ?? setUncontrolledOpen;
  const endpoint = organization
    ? `/api/organizations/${organization.id}/repositories/creation`
    : "/api/repos/creation";
  const destination = useRequest<Destination>(open ? endpoint : null);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      {trigger !== null && (
        <DialogTrigger asChild>
          {trigger ?? (
            <Button>
              <RepoIcon data-icon="inline-start" />
              New repository
            </Button>
          )}
        </DialogTrigger>
      )}
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Create a repository</DialogTitle>
          <DialogDescription>
            {organization
              ? `Create an empty Git repository owned by ${organization.name}.`
              : "Create an empty Git repository in your personal namespace."}
          </DialogDescription>
        </DialogHeader>
        <Result state={destination}>
          {(data) => (
            <RepositoryForm
              destination={data}
              endpoint={endpoint}
              account={organization?.actor}
              csrf={csrf}
              onCreated={(repository) => {
                setOpen(false);
                onCreated(repository);
              }}
            />
          )}
        </Result>
      </DialogContent>
    </Dialog>
  );
}
