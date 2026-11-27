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
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { OrganizationAvatar } from "@/components/ui/user-avatar";
import { OrganizationRulesets } from "@/features/settings/rulesets";
import { useRequest } from "@/lib/api";
import type { Organization, OrganizationView } from "@/lib/contracts";
import { OrganizationAuditLog } from "./audit-log";
import { DeletedRepositories } from "./deleted-repositories";
import { OrganizationAutomation } from "./automation";
import { organizationHref } from "@/lib/organization-route";
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
  const [name, setName] = useState(organization.name);
  const [description, setDescription] = useState(organization.description);
  const mutation = useMutation(csrf, null);
  const [version, setVersion] = useState(organization.version);
  const latest = useRequest<OrganizationView>(
    mutation.conflict ? endpoint : null,
  );
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
            void mutation
              .run(endpoint, "PATCH", {
                version,
                name: name.trim(),
                description,
              })
              .then((value) => {
                if (value) onSaved();
              });
          }}
        >
          <FieldGroup>
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
            <MutationError error={mutation.error} />
            {mutation.conflict && (
              <Result state={latest} timingLabel="Current organization profile">
                {(saved) => (
                  <Alert>
                    <AlertDescription>
                      <p>
                        Saved profile:{" "}
                        <strong>{saved.organization.name}</strong>
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
                              setName(saved.organization.name);
                              setDescription(saved.organization.description);
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
              disabled={mutation.pending || !name.trim()}
            >
              {mutation.pending ? "Updating profile…" : "Update profile"}
            </Button>
          </FieldGroup>
        </form>
        <section aria-labelledby="organization-picture" className="grid gap-3">
          <h3 id="organization-picture" className="m-0 text-sm font-semibold">
            Profile picture
          </h3>
          <OrganizationAvatar
            id={organization.id}
            size={200}
            className="rounded-xl"
          />
          <p className="m-0 text-xs text-muted-foreground">
            Generated from the organization’s ID, so it stays the same when you
            rename the organization.
          </p>
        </section>
      </div>
    </div>
  );
}
