import { GearIcon } from "@primer/octicons-react";
import { useId, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
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
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { useRequest, type Repository } from "@/lib/api";

type Attributes = {
  homepage?: string;
  license?: string;
  is_template?: boolean;
  version: number;
};

function readAttributes(raw: Attributes) {
  return {
    homepage: raw.homepage ?? "",
    license: raw.license ?? "",
    isTemplate: raw.is_template ?? false,
    version: raw.version,
  };
}

type Description = {
  description?: string;
  version: number;
};

const DESCRIPTION_MAX = 350;
const HOMEPAGE_MAX = 256;
const LICENSE_MAX = 64;
const NONE_OPTION = "__none__";
const CUSTOM_OPTION = "__custom__";

const COMMON_LICENSES = [
  "MIT",
  "Apache-2.0",
  "GPL-3.0-only",
  "BSD-3-Clause",
  "BSD-2-Clause",
  "ISC",
  "MPL-2.0",
  "LGPL-3.0-only",
  "AGPL-3.0-only",
  "Unlicense",
  "CC0-1.0",
] as const;

const LICENSE_OPTIONS = [
  { value: NONE_OPTION, label: "None" },
  ...COMMON_LICENSES.map((spdx) => ({ value: spdx, label: spdx })),
  { value: CUSTOM_OPTION, label: "Custom SPDX identifier" },
];

export function formatHomepageLabel(homepage: string): string {
  return homepage.replace(/^https?:\/\//i, "").replace(/\/+$/, "");
}

function validHomepage(value: string): boolean {
  if (!value) return true;
  return (
    (value.startsWith("https://") || value.startsWith("http://")) &&
    Array.from(value).length <= HOMEPAGE_MAX &&
    !/[\s\u0000-\u001f\u007f]/.test(value)
  );
}

function validLicense(value: string): boolean {
  if (!value) return true;
  return (
    Array.from(value).length <= LICENSE_MAX &&
    /^[A-Za-z0-9.\-_+ ]+$/.test(value)
  );
}

function attributesSummary(repo: Repository): string {
  const parts: string[] = [];
  if (repo.homepage) parts.push(formatHomepageLabel(repo.homepage));
  if (repo.license) parts.push(`${repo.license} license`);
  if (repo.is_template) parts.push("Template repository");
  return parts.length > 0
    ? parts.join(" · ")
    : "No website, license, or template setting";
}

function AttributeFields({
  homepage,
  license,
  isTemplate,
  disabled,
  onHomepageChange,
  onLicenseChange,
  onTemplateChange,
}: {
  homepage: string;
  license: string;
  isTemplate: boolean;
  disabled: boolean;
  onHomepageChange: (value: string) => void;
  onLicenseChange: (value: string) => void;
  onTemplateChange: (value: boolean) => void;
}) {
  const homepageId = useId();
  const presetId = useId();
  const licenseId = useId();
  const templateId = useId();

  const normalizedHomepage = homepage.trim();
  const normalizedLicense = license.trim();
  const homepageValid = validHomepage(normalizedHomepage);
  const licenseValid = validLicense(normalizedLicense);

  const presetValue = !normalizedLicense
    ? NONE_OPTION
    : COMMON_LICENSES.includes(
          normalizedLicense as (typeof COMMON_LICENSES)[number],
        )
      ? normalizedLicense
      : CUSTOM_OPTION;

  return (
    <FieldGroup>
      <Field>
        <FieldLabel htmlFor={homepageId}>Website</FieldLabel>
        <Input
          id={homepageId}
          type="url"
          value={homepage}
          maxLength={HOMEPAGE_MAX}
          placeholder="https://example.com"
          disabled={disabled}
          aria-invalid={!homepageValid}
          onChange={(event) => onHomepageChange(event.target.value)}
        />
        {!homepageValid ? (
          <p role="status" className="text-xs text-destructive">
            Enter an http:// or https:// URL without spaces (up to 256
            characters).
          </p>
        ) : (
          <FieldDescription>
            Link shown in the repository About sidebar.
          </FieldDescription>
        )}
      </Field>

      <Field>
        <FieldLabel htmlFor={licenseId}>License</FieldLabel>
        <div className="grid gap-2 sm:grid-cols-2">
          <FieldSelect
            id={presetId}
            aria-label="License preset"
            options={LICENSE_OPTIONS}
            value={presetValue}
            disabled={disabled}
            onValueChange={(value) => {
              if (value === NONE_OPTION) {
                onLicenseChange("");
              } else if (value !== CUSTOM_OPTION) {
                onLicenseChange(value);
              }
            }}
          />
          <Input
            id={licenseId}
            value={license}
            maxLength={LICENSE_MAX}
            placeholder="SPDX ID (e.g. MIT, Apache-2.0)"
            disabled={disabled}
            aria-invalid={!licenseValid}
            onChange={(event) => onLicenseChange(event.target.value)}
          />
        </div>
        {!licenseValid ? (
          <p role="status" className="text-xs text-destructive">
            Use up to 64 characters: letters, digits, spaces, ., -, _, or +.
          </p>
        ) : (
          <FieldDescription>
            Choose a common SPDX license or enter a custom identifier.
          </FieldDescription>
        )}
      </Field>

      <Field orientation="horizontal">
        <Checkbox
          id={templateId}
          checked={isTemplate}
          disabled={disabled}
          onCheckedChange={(checked) => onTemplateChange(checked === true)}
        />
        <FieldContent>
          <FieldLabel htmlFor={templateId}>Template repository</FieldLabel>
          <FieldDescription>
            Mark this repository as a template for new projects.
          </FieldDescription>
        </FieldContent>
      </Field>
    </FieldGroup>
  );
}

export function AttributesSettings({
  repo,
  csrf,
  onRepositoryChanged,
}: {
  repo: Repository;
  csrf: string;
  onRepositoryChanged: () => void;
}) {
  const [open, setOpen] = useState(false);

  return (
    <section className="settings-section" aria-labelledby="repository-details">
      <header>
        <div className="min-w-0">
          <h3 id="repository-details">Repository details</h3>
          <p className="break-words">{attributesSummary(repo)}</p>
        </div>
        {!repo.archived && (
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button type="button" variant="outline" size="sm">
                Edit details
              </Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Edit repository details</DialogTitle>
                <DialogDescription>
                  Set website URL, SPDX license, and template repository flag.
                </DialogDescription>
              </DialogHeader>
              {open && (
                <AttributesForm
                  repo={repo}
                  csrf={csrf}
                  onSaved={() => {
                    setOpen(false);
                    onRepositoryChanged();
                  }}
                />
              )}
            </DialogContent>
          </Dialog>
        )}
      </header>
    </section>
  );
}

function AttributesForm({
  repo,
  csrf,
  onSaved,
}: {
  repo: Repository;
  csrf: string;
  onSaved: () => void;
}) {
  const path = `/api/repositories/${repo.id}/attributes`;
  const settings = useRequest<Attributes>(path);
  const mutation = useMutation(csrf, repo);
  const [homepageDraft, setHomepageDraft] = useState<string>();
  const [licenseDraft, setLicenseDraft] = useState<string>();
  const [templateDraft, setTemplateDraft] = useState<boolean>();
  const stale = mutation.code === "repository_attributes_changed";

  return (
    <Result state={settings} timingLabel="Repository details">
      {(loaded) => {
        const current = readAttributes(loaded);
        const homepageValue = homepageDraft ?? current.homepage;
        const licenseValue = licenseDraft ?? current.license;
        const homepage = homepageValue.trim();
        const license = licenseValue.trim();
        const isTemplate = templateDraft ?? current.isTemplate;
        const dirty =
          homepage !== current.homepage ||
          license !== current.license ||
          isTemplate !== current.isTemplate;
        const valid = validHomepage(homepage) && validLicense(license);
        const canSave = !mutation.pending && !stale && dirty && valid;

        return (
          <form
            className="grid gap-4"
            onSubmit={async (event) => {
              event.preventDefault();
              if (!canSave) return;
              const result = await mutation.run<Attributes>(path, "PUT", {
                homepage,
                license,
                is_template: isTemplate,
                version: current.version,
              });
              if (result) onSaved();
            }}
          >
            <AttributeFields
              homepage={homepageValue}
              license={licenseValue}
              isTemplate={isTemplate}
              disabled={mutation.pending}
              onHomepageChange={setHomepageDraft}
              onLicenseChange={setLicenseDraft}
              onTemplateChange={setTemplateDraft}
            />
            {mutation.error && (
              <p role="alert" className="text-sm text-destructive">
                {mutation.error}
              </p>
            )}
            <div className="flex flex-wrap items-center justify-end gap-2">
              {stale && (
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => {
                    setHomepageDraft(undefined);
                    setLicenseDraft(undefined);
                    setTemplateDraft(undefined);
                    mutation.reset();
                    settings.retry();
                  }}
                >
                  Reload details
                </Button>
              )}
              <Button type="submit" disabled={!canSave}>
                {mutation.pending ? "Saving…" : "Save changes"}
              </Button>
            </div>
          </form>
        );
      }}
    </Result>
  );
}

export function EditAboutDialog({
  repo,
  csrf,
  onRepositoryChanged,
}: {
  repo: Repository;
  csrf: string;
  onRepositoryChanged: () => void;
}) {
  const [open, setOpen] = useState(false);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Edit repository details"
          title="Edit repository details"
        >
          <GearIcon size={16} />
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Edit repository details</DialogTitle>
          <DialogDescription>
            Update the description, website, license, and template settings.
          </DialogDescription>
        </DialogHeader>
        {open && (
          <EditAboutForm
            repo={repo}
            csrf={csrf}
            onRepositoryChanged={onRepositoryChanged}
            onSaved={() => {
              setOpen(false);
              onRepositoryChanged();
            }}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

function EditAboutForm({
  repo,
  csrf,
  onRepositoryChanged,
  onSaved,
}: {
  repo: Repository;
  csrf: string;
  onRepositoryChanged: () => void;
  onSaved: () => void;
}) {
  const descriptionPath = `/api/repositories/${repo.id}/description`;
  const attributesPath = `/api/repositories/${repo.id}/attributes`;
  const descriptionState = useRequest<Description>(descriptionPath);
  const attributesState = useRequest<Attributes>(attributesPath);
  const mutation = useMutation(csrf, repo);
  const descriptionId = useId();

  const [descriptionDraft, setDescriptionDraft] = useState<string>();
  const [savedDescription, setSavedDescription] = useState<Description>();
  const [homepageDraft, setHomepageDraft] = useState<string>();
  const [licenseDraft, setLicenseDraft] = useState<string>();
  const [templateDraft, setTemplateDraft] = useState<boolean>();

  const stale =
    mutation.code === "repository_description_changed" ||
    mutation.code === "repository_attributes_changed";

  return (
    <Result state={descriptionState} timingLabel="Repository description">
      {(loadedDescription) => (
        <Result state={attributesState} timingLabel="Repository details">
          {(loadedAttributes) => {
            const currentAttributes = readAttributes(loadedAttributes);
            const currentDescription = savedDescription ?? loadedDescription;
            const savedDescriptionText = currentDescription.description ?? "";
            const descriptionRaw = descriptionDraft ?? savedDescriptionText;
            const descriptionNormalized = descriptionRaw.trim();
            const descriptionValid =
              !descriptionNormalized.includes("\n") &&
              !descriptionNormalized.includes("\r") &&
              Array.from(descriptionNormalized).length <= DESCRIPTION_MAX;

            const homepageValue = homepageDraft ?? currentAttributes.homepage;
            const licenseValue = licenseDraft ?? currentAttributes.license;
            const homepage = homepageValue.trim();
            const license = licenseValue.trim();
            const isTemplate = templateDraft ?? currentAttributes.isTemplate;
            const attributesValid =
              validHomepage(homepage) && validLicense(license);

            const descriptionDirty =
              descriptionNormalized !== savedDescriptionText;
            const attributesDirty =
              homepage !== currentAttributes.homepage ||
              license !== currentAttributes.license ||
              isTemplate !== currentAttributes.isTemplate;

            const canSave =
              !mutation.pending &&
              !stale &&
              (descriptionDirty || attributesDirty) &&
              descriptionValid &&
              attributesValid;

            return (
              <form
                className="grid gap-4"
                onSubmit={async (event) => {
                  event.preventDefault();
                  if (!canSave) return;
                  let changed = false;

                  if (descriptionDirty) {
                    const nextDescription = await mutation.run<Description>(
                      descriptionPath,
                      "PUT",
                      {
                        description: descriptionNormalized,
                        version: currentDescription.version,
                      },
                    );
                    if (!nextDescription) return;
                    setSavedDescription(nextDescription);
                    changed = true;
                  }

                  if (attributesDirty) {
                    const nextAttributes = await mutation.run<Attributes>(
                      attributesPath,
                      "PUT",
                      {
                        homepage,
                        license,
                        is_template: isTemplate,
                        version: currentAttributes.version,
                      },
                    );
                    if (!nextAttributes) {
                      if (changed) onRepositoryChanged();
                      return;
                    }
                    changed = true;
                  }

                  if (changed) onSaved();
                }}
              >
                <Field>
                  <FieldLabel htmlFor={descriptionId}>Description</FieldLabel>
                  <Input
                    id={descriptionId}
                    value={descriptionRaw}
                    maxLength={DESCRIPTION_MAX}
                    placeholder="Short description of this repository"
                    disabled={mutation.pending}
                    aria-invalid={!descriptionValid}
                    onChange={(event) =>
                      setDescriptionDraft(event.target.value)
                    }
                  />
                  {!descriptionValid && (
                    <p role="status" className="text-xs text-destructive">
                      Use one line of up to 350 characters.
                    </p>
                  )}
                </Field>

                <AttributeFields
                  homepage={homepageValue}
                  license={licenseValue}
                  isTemplate={isTemplate}
                  disabled={mutation.pending}
                  onHomepageChange={setHomepageDraft}
                  onLicenseChange={setLicenseDraft}
                  onTemplateChange={setTemplateDraft}
                />

                {mutation.error && (
                  <p role="alert" className="text-sm text-destructive">
                    {mutation.error}
                  </p>
                )}
                <div className="flex flex-wrap items-center justify-end gap-2">
                  {stale && (
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => {
                        setDescriptionDraft(undefined);
                        setSavedDescription(undefined);
                        setHomepageDraft(undefined);
                        setLicenseDraft(undefined);
                        setTemplateDraft(undefined);
                        mutation.reset();
                        descriptionState.retry();
                        attributesState.retry();
                      }}
                    >
                      Reload details
                    </Button>
                  )}
                  <Button type="submit" disabled={!canSave}>
                    {mutation.pending ? "Saving…" : "Save changes"}
                  </Button>
                </div>
              </form>
            );
          }}
        </Result>
      )}
    </Result>
  );
}
