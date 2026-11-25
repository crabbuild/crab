import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
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
  settings: "Profile",
  privileges: "Member privileges",
  "project-policy": "Project policy",
  rulesets: "Rulesets",
  properties: "Repository properties",
  "issue-types": "Issue types",
  automation: "Automation",
  audit: "Audit log",
  "deleted-repositories": "Deleted repositories",
} as const;

type Section = keyof typeof sections;

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
    <div className="settings-layout">
      <nav
        aria-label="Organization settings"
        className="settings-nav flex-wrap pb-2 md:pb-0"
      >
        {Object.entries(sections).map(([key, label]) => (
          <Link
            key={key}
            className={tab === key ? "active" : ""}
            aria-current={tab === key ? "page" : undefined}
            href={organizationHref(organization, `?tab=${key}`)}
          >
            {label}
          </Link>
        ))}
      </nav>
      <section aria-label={sections[tab]} className="settings-content">
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
    <form
      className="max-w-2xl"
      onSubmit={(event) => {
        event.preventDefault();
        void mutation
          .run(endpoint, "PATCH", {
            version,
            name,
            description,
          })
          .then((value) => {
            if (value) onSaved();
          });
      }}
    >
      <h2 className="mb-6 text-lg font-semibold">Organization profile</h2>
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="organization-name">Display name</FieldLabel>
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
                    Saved profile: <strong>{saved.organization.name}</strong>
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
                      You no longer have permission to edit this organization.
                    </p>
                  )}
                </AlertDescription>
              </Alert>
            )}
          </Result>
        )}
        <Button type="submit" className="w-fit" disabled={mutation.pending}>
          {mutation.pending ? "Saving…" : "Save changes"}
        </Button>
      </FieldGroup>
    </form>
  );
}
