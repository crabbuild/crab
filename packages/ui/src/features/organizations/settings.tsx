import { useState } from "react";
import {
  GearIcon,
  HubotIcon,
  IssueOpenedIcon,
  LogIcon,
  PeopleIcon,
  RepoDeletedIcon,
  ShieldLockIcon,
  TableIcon,
  TagIcon,
} from "@primer/octicons-react";
import { SettingsNav } from "@/components/navigation/settings-nav";
import { Result } from "@/components/request/result";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { ProfileAvatarPicker } from "@/features/profile/avatar-picker";
import { OrganizationRulesets } from "@/features/settings/rulesets";
import { navigate, useRequest } from "@/lib/api";
import type {
  AvatarConfig,
  Organization,
  OrganizationView,
} from "@/lib/contracts";
import { OrganizationAuditLog } from "./audit-log";
import { DeletedRepositories } from "./deleted-repositories";
import { OrganizationAutomation } from "./automation";
import {
  organizationHref,
  validOrganizationHandle,
} from "@/lib/organization-route";
import { MutationError } from "./feedback";
import { OrganizationIssueTypes } from "./issue-types";
import { ProjectPolicySettings } from "./project-policy";
import { RepositoryPolicySettings } from "./repository-policy";
import { OrganizationRepositoryProperties } from "./repository-properties";

const sections = {
  settings: ["General", GearIcon],
  privileges: ["Member privileges", PeopleIcon],
  rulesets: ["Rulesets", ShieldLockIcon],
  properties: ["Repository properties", TagIcon],
  "issue-types": ["Issue types", IssueOpenedIcon],
  "project-policy": ["Projects", TableIcon],
  automation: ["Automation", HubotIcon],
  audit: ["Audit log", LogIcon],
  "deleted-repositories": ["Deleted repositories", RepoDeletedIcon],
} as const;

type Section = keyof typeof sections;

// GitHub's organization settings groups, limited to what Crab provides.
const groups: { label?: string; items: Section[] }[] = [
  { items: ["settings"] },
  { label: "Access", items: ["privileges"] },
  {
    label: "Code, planning, and automation",
    items: [
      "rulesets",
      "properties",
      "issue-types",
      "project-policy",
      "automation",
    ],
  },
  { label: "Archive", items: ["audit", "deleted-repositories"] },
];

export function isOrganizationSettings(tab: string): tab is Section {
  return Object.hasOwn(sections, tab);
}

export function OrganizationSettings({
  tab,
  organization,
  endpoint,
  csrf,
  url,
  onProfileSaved,
}: {
  tab: Section;
  organization: Organization;
  endpoint: string;
  csrf: string;
  url: URL;
  onProfileSaved: () => void;
}) {
  const id = organization.id;
  const content = {
    settings: (
      <ProfileSettings
        key={organization.version}
        organization={organization}
        endpoint={endpoint}
        csrf={csrf}
        onSaved={onProfileSaved}
      />
    ),
    privileges: <RepositoryPolicySettings organization={id} csrf={csrf} />,
    "project-policy": <ProjectPolicySettings organization={id} />,
    rulesets: <OrganizationRulesets key={id} organization={id} csrf={csrf} />,
    properties: (
      <OrganizationRepositoryProperties
        key={id}
        organization={id}
        csrf={csrf}
      />
    ),
    "issue-types": (
      <OrganizationIssueTypes key={id} organization={id} csrf={csrf} />
    ),
    automation: <OrganizationAutomation organization={id} csrf={csrf} />,
    audit: <OrganizationAuditLog id={id} url={url} />,
    "deleted-repositories": <DeletedRepositories id={id} csrf={csrf} />,
  };
  return (
    <div className="account-settings-body">
      <SettingsNav
        label="Organization settings"
        active={tab}
        groups={groups.map(({ label, items }) => ({
          label,
          items: items.map((id) => ({
            id,
            label: sections[id][0],
            Icon: sections[id][1],
            href: organizationHref(organization, `?tab=${id}`),
          })),
        }))}
      />
      <section
        aria-label={sections[tab][0]}
        className="settings-content min-w-0"
      >
        {content[tab]}
      </section>
    </div>
  );
}

function ProfileSettings({
  organization,
  endpoint,
  csrf,
  onSaved,
}: {
  organization: Organization;
  endpoint: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [handle, setHandle] = useState(organization.handle);
  const [name, setName] = useState(organization.name);
  const [description, setDescription] = useState(organization.description);
  const [website, setWebsite] = useState(organization.website ?? "");
  const [email, setEmail] = useState(organization.email ?? "");
  const [location, setLocation] = useState(organization.location ?? "");
  const [avatar, setAvatar] = useState<AvatarConfig | null>(
    organization.avatar ?? null,
  );
  const mutation = useMutation(csrf, null);
  const [version, setVersion] = useState(organization.version);
  const latest = useRequest<OrganizationView>(
    mutation.conflict ? endpoint : null,
  );
  const trimmedHandle = handle.trim().toLowerCase();
  const handleValid = validOrganizationHandle(trimmedHandle);
  const previewAddress = new URL(
    organizationHref({
      id: organization.id,
      handle: trimmedHandle || organization.handle,
      name,
    }),
    window.location.origin,
  ).href;

  async function saveOrganizationAvatar(nextAvatar: AvatarConfig | null) {
    const response = await fetch(endpoint, {
      method: "PATCH",
      credentials: "same-origin",
      headers: {
        "Content-Type": "application/json",
        "X-CSRF-Token": csrf,
      },
      body: JSON.stringify({
        version,
        handle: organization.handle,
        name: organization.name,
        description: organization.description,
        website: organization.website ?? "",
        email: organization.email ?? "",
        location: organization.location ?? "",
        avatar: nextAvatar,
      }),
    });
    if (!response.ok) {
      const payload = (await response.json().catch(() => null)) as {
        error?: string;
        message?: string;
      } | null;
      throw new Error(
        payload?.error ||
          payload?.message ||
          `Request failed with status ${response.status}.`,
      );
    }
    const updated = (await response.json()) as Organization;
    setVersion(updated.version);
    return updated.avatar ?? null;
  }

  return (
    <div className="grid min-w-0 gap-6">
      <h2 className="m-0 border-b pb-2 text-2xl font-semibold tracking-tight">
        Organization profile
      </h2>
      <div className="grid items-start gap-8 lg:grid-cols-[minmax(0,1fr)_12.5rem]">
        <form
          className="max-w-2xl min-w-0"
          onSubmit={(event) => {
            event.preventDefault();
            if (!name.trim() || !handleValid) return;
            void mutation
              .run<Organization>(endpoint, "PATCH", {
                version,
                handle: trimmedHandle,
                name: name.trim(),
                description,
                website: website.trim(),
                email: email.trim(),
                location: location.trim(),
                avatar,
              })
              .then((updated) => {
                if (!updated) return;
                if (updated.handle !== organization.handle) {
                  navigate(organizationHref(updated, "?tab=settings"));
                }
                onSaved();
              });
          }}
        >
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="organization-handle">
                Organization handle
              </FieldLabel>
              <Input
                id="organization-handle"
                value={handle}
                onChange={(event) =>
                  setHandle(event.target.value.toLowerCase())
                }
                required
                maxLength={39}
                pattern="[a-z0-9](?:[a-z0-9-]{0,37}[a-z0-9])?"
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                aria-describedby="organization-handle-help"
                disabled={mutation.pending}
              />
              <FieldDescription id="organization-handle-help">
                Your organization URL is{" "}
                <code className="break-all">{previewAddress}</code> and
                repositories live under{" "}
                <code>{trimmedHandle || organization.handle}/&lt;repo&gt;</code>
                .
              </FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="organization-name">
                Organization display name
              </FieldLabel>
              <Input
                id="organization-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                required
                maxLength={100}
                disabled={mutation.pending}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="organization-description">
                Description
              </FieldLabel>
              <Textarea
                id="organization-description"
                value={description}
                onChange={(event) => setDescription(event.target.value)}
                maxLength={4000}
                disabled={mutation.pending}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="organization-website">Website</FieldLabel>
              <Input
                id="organization-website"
                type="url"
                placeholder="https://example.com"
                value={website}
                onChange={(event) => setWebsite(event.target.value)}
                maxLength={256}
                disabled={mutation.pending}
              />
            </Field>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field>
                <FieldLabel htmlFor="organization-email">Email</FieldLabel>
                <Input
                  id="organization-email"
                  type="email"
                  placeholder="hello@example.com"
                  value={email}
                  onChange={(event) => setEmail(event.target.value)}
                  maxLength={254}
                  disabled={mutation.pending}
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="organization-location">
                  Location
                </FieldLabel>
                <Input
                  id="organization-location"
                  placeholder="San Francisco, CA"
                  value={location}
                  onChange={(event) => setLocation(event.target.value)}
                  maxLength={120}
                  disabled={mutation.pending}
                />
              </Field>
            </div>
            <MutationError error={mutation.error} />
            {mutation.conflict && (
              <Result state={latest} timingLabel="Current organization profile">
                {(saved) => (
                  <Alert>
                    <AlertDescription>
                      <p>
                        Saved profile:{" "}
                        <strong>{saved.organization.name}</strong> (
                        <code>@{saved.organization.handle}</code>)
                      </p>
                      <p className="whitespace-pre-wrap">
                        {saved.organization.description || "No description"}
                      </p>
                      {saved.role === "owner" ? (
                        <div className="mt-3 flex flex-wrap gap-2">
                          <Button
                            type="button"
                            variant="outline"
                            onClick={() => {
                              setVersion(saved.organization.version);
                              mutation.reset();
                            }}
                          >
                            Keep draft with current version
                          </Button>
                          <Button
                            type="button"
                            variant="outline"
                            onClick={() => {
                              setHandle(saved.organization.handle);
                              setName(saved.organization.name);
                              setDescription(saved.organization.description);
                              setWebsite(saved.organization.website ?? "");
                              setEmail(saved.organization.email ?? "");
                              setLocation(saved.organization.location ?? "");
                              setAvatar(saved.organization.avatar ?? null);
                              setVersion(saved.organization.version);
                              mutation.reset();
                            }}
                          >
                            Use saved profile
                          </Button>
                        </div>
                      ) : (
                        <p>
                          You no longer have permission to edit this
                          organization.
                        </p>
                      )}
                    </AlertDescription>
                  </Alert>
                )}
              </Result>
            )}
            <Button
              type="submit"
              className="w-fit"
              disabled={mutation.pending || !name.trim() || !handleValid}
            >
              {mutation.pending ? "Updating profile…" : "Update profile"}
            </Button>
          </FieldGroup>
        </form>
        <section aria-labelledby="organization-picture" className="grid gap-3">
          <ProfileAvatarPicker
            fallbackName={organization.name || organization.handle}
            avatar={avatar}
            csrf={csrf}
            square
            heading="Profile picture"
            description="Customize the style, seed, and colors for this organization across Crab."
            onSaveAvatar={saveOrganizationAvatar}
            onSaved={(next) => {
              setAvatar(next);
              onSaved();
            }}
          />
        </section>
      </div>
    </div>
  );
}
