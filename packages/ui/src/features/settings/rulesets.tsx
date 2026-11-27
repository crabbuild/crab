import { Checkbox } from "@/components/ui/checkbox";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
import { useState, type FormEvent, type ReactNode } from "react";
import { Link } from "@/components/navigation/link";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import {
  propertySchemaPath,
  type PropertyDefinition,
  type PropertySchema,
} from "@/features/organizations/property-fields";
import { type Connections } from "./integrations";
import {
  RulesetBypassEditor,
  type BypassGrant,
  type BypassOptions,
} from "./ruleset-bypass";
import {
  ReviewDismissalEditor,
  type ReviewDismissalActor,
} from "./ruleset-review-dismissal";

import {
  allRepositories,
  RepositoryTargetEditor,
  RepositoryTargetSummary,
  type RepositoryTarget,
} from "./ruleset-target";

interface Ruleset {
  id: string;
  name: string;
  target: "branch" | "tag";
  enforcement: "active" | "disabled";
  include: string[];
  exclude: string[];
  restrict_creations: boolean;
  restrict_updates?: boolean;
  restrict_deletions: boolean;
  force_pushes?: "allow" | "block";
  require_pull_request: boolean;
  required_approvals: number;
  required_code_owner_reviews: boolean;
  require_last_push_approval?: boolean;
  require_conversation_resolution: boolean;
  require_up_to_date?: boolean;
  require_merge_queue?: boolean;
  require_linear_history?: boolean;
  required_checks: string[];
  required_integrations: string[];
  bypass_actors?: BypassGrant[];
  review_dismissal?: ReviewDismissalActor[];
  repositories?: RepositoryTarget;
}

interface RulesetsState {
  version: number;
  rulesets: Ruleset[];
  can_manage: boolean;
  bypass_options?: BypassOptions;
  inherited?: { organization: string; rulesets: Ruleset[] };
}

interface OrganizationRuleCollection {
  version: number;
  rulesets: { ruleset: Ruleset; repositories: RepositoryTarget }[];
}

function organizationRulesets(
  value: OrganizationRuleCollection,
): RulesetsState {
  return {
    version: value.version,
    can_manage: true,
    rulesets: value.rulesets.map(({ ruleset, repositories }) => ({
      ...ruleset,
      repositories,
    })),
  };
}

export function OrganizationRulesets({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const path = `/api/organizations/${encodeURIComponent(organization)}/rulesets`;
  const request = useRequest<OrganizationRuleCollection>(path);
  const properties = useRequest<PropertySchema>(
    propertySchemaPath(organization),
  );
  return (
    <section className="grid gap-4" aria-labelledby="organization-rulesets">
      <h2 id="organization-rulesets" className="text-lg font-semibold">
        Organization rulesets
      </h2>
      <Result state={request} timingLabel="Organization rulesets">
        {(value) => (
          <RulesetsList
            key={value.version}
            organization={organization}
            definitions={properties.data?.definitions}
            initial={organizationRulesets(value)}
            csrf={csrf}
            editable
            path={path}
            reload={request.retry}
          />
        )}
      </Result>
    </section>
  );
}

const pullRules = {
  force_pushes: undefined,
  require_pull_request: false,
  required_approvals: 0,
  required_code_owner_reviews: false,
  require_last_push_approval: false,
  require_conversation_resolution: false,
  require_up_to_date: false,
  require_merge_queue: false,
  required_checks: [],
  required_integrations: [],
  review_dismissal: undefined,
};

function newRuleset(): Ruleset {
  return {
    id: crypto.randomUUID(),
    name: "",
    target: "branch",
    enforcement: "active",
    include: ["~DEFAULT_BRANCH"],
    exclude: [],
    restrict_creations: false,
    restrict_deletions: true,
    ...pullRules,
  };
}

export function RepositoryRulesets({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf?: string;
}) {
  const path = endpoint(repo, "rulesets");
  const request = useRequest<RulesetsState>(path);
  return (
    <section className="settings-content" aria-labelledby="rulesets-heading">
      <div className="settings-title">
        <div>
          <h2 id="rulesets-heading">Rulesets</h2>
          <p>Set requirements for matching branches and tags.</p>
        </div>
        {csrf === undefined && repo.can_admin && (
          <Link
            href={repoHref(repo, { view: "settings", section: "rulesets" })}
          >
            Manage rulesets
          </Link>
        )}
        {csrf === undefined &&
          repo.organization_id &&
          (repo.can_admin || repo.can_maintain) && (
            <Link
              href={repoHref(repo, { view: "settings", section: "properties" })}
            >
              Repository properties
            </Link>
          )}
      </div>
      <Result state={request} timingLabel="Repository rulesets">
        {(data) => (
          <RulesetsList
            key={data.version}
            repo={repo}
            initial={data}
            csrf={csrf ?? ""}
            editable={csrf !== undefined && data.can_manage && !repo.archived}
            path={path}
            reload={request.retry}
          />
        )}
      </Result>
    </section>
  );
}

function RulesetsList({
  repo,
  organization,
  definitions,
  initial,
  csrf,
  editable,
  path,
  reload,
}: {
  repo?: Repository;
  organization?: string;
  definitions?: PropertyDefinition[];
  initial: RulesetsState;
  csrf: string;
  editable: boolean;
  path: string;
  reload: () => void;
}) {
  const [state, setState] = useState(initial);
  const [editing, setEditing] = useState<Ruleset>();
  const [deleting, setDeleting] = useState<Ruleset>();
  const mutation = useMutation(csrf, repo ?? null);
  const rulesets = editable
    ? state.rulesets
    : state.rulesets.filter((rule) => rule.enforcement === "active");
  const workflows = useRequest<Connections>(
    repo && editable && editing?.target === "branch"
      ? endpoint(repo, "settings/integrations")
      : null,
  );

  async function save(rulesets: Ruleset[]) {
    const payload = organization
      ? rulesets.map(({ repositories, ...ruleset }) => ({
          ruleset,
          repositories: repositories ?? allRepositories,
        }))
      : rulesets;
    const result = await mutation.run<
      RulesetsState | OrganizationRuleCollection
    >(path, "PUT", {
      expected_version: state.version,
      rulesets: payload,
    });
    const response =
      result &&
      ("can_manage" in result ? result : organizationRulesets(result));
    if (response) {
      setState(response);
      setEditing(undefined);
      setDeleting(undefined);
    }
  }

  return (
    <div className="grid gap-3">
      {editable && !editing && !deleting && (
        <Button
          type="button"
          className="justify-self-start"
          onClick={() =>
            setEditing({
              ...newRuleset(),
              ...(organization ? { repositories: allRepositories } : {}),
            })
          }
        >
          New ruleset
        </Button>
      )}
      {mutation.error && (
        <div className="grid justify-items-start gap-2">
          <Failure message={mutation.error} />
          <Button type="button" variant="outline" onClick={reload}>
            Reload rulesets
          </Button>
        </div>
      )}
      {editing && (
        <RulesetForm
          key={editing.id}
          initial={editing}
          repo={repo}
          organization={organization}
          definitions={definitions}
          workflows={workflows.data?.items}
          workflowsError={workflows.error}
          bypassOptions={state.bypass_options}
          saving={mutation.pending}
          onCancel={() => setEditing(undefined)}
          onSave={(rule) =>
            save(
              state.rulesets.some(({ id }) => id === rule.id)
                ? state.rulesets.map((item) =>
                    item.id === rule.id ? rule : item,
                  )
                : [...state.rulesets, rule],
            )
          }
        />
      )}
      {deleting && (
        <section
          className="settings-section p-4 grid gap-3"
          aria-label="Delete ruleset"
        >
          <p>Delete the ruleset “{deleting.name}”?</p>
          <div className="flex gap-2">
            <Button
              type="button"
              variant="destructive"
              disabled={mutation.pending}
              onClick={() =>
                void save(state.rulesets.filter(({ id }) => id !== deleting.id))
              }
            >
              Delete ruleset
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={mutation.pending}
              onClick={() => setDeleting(undefined)}
            >
              Cancel
            </Button>
          </div>
        </section>
      )}
      {rulesets.length === 0 &&
        !state.inherited?.rulesets.length &&
        !editing && <p className="notice">No rulesets to display.</p>}
      {rulesets.map((rule) => (
        <RulesetSummary key={rule.id} rule={rule} definitions={definitions}>
          {editable && !editing && !deleting && (
            <div className="flex gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                aria-label={`Edit ${rule.name}`}
                onClick={() => setEditing(rule)}
              >
                Edit
              </Button>
              <Button
                type="button"
                size="sm"
                variant="outline"
                aria-label={`Delete ${rule.name}`}
                onClick={() => setDeleting(rule)}
              >
                Delete
              </Button>
            </div>
          )}
        </RulesetSummary>
      ))}
      {!!state.inherited?.rulesets.length && (
        <section
          className="grid gap-3"
          aria-label="Inherited organization rulesets"
        >
          <h3 className="font-semibold">Inherited from organization</h3>
          {state.inherited.rulesets.map((rule) => (
            <RulesetSummary key={rule.id} rule={rule} />
          ))}
        </section>
      )}
    </div>
  );
}

function RulesetSummary({
  rule,
  children,
  definitions,
}: {
  rule: Ruleset;
  children?: ReactNode;
  definitions?: PropertyDefinition[];
}) {
  return (
    <article className="settings-section grid gap-2 p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="font-semibold break-all">{rule.name}</h3>
        <div className="flex gap-2">
          <Badge
            variant={rule.enforcement === "active" ? "default" : "secondary"}
          >
            {rule.enforcement === "active" ? "Active" : "Disabled"}
          </Badge>
          <Badge variant="outline">
            {rule.target === "branch" ? "Branches" : "Tags"}
          </Badge>
        </div>
      </div>
      <p className="break-all">
        Include <code>{rule.include.join(", ")}</code>
      </p>
      {rule.exclude.length > 0 && (
        <p className="break-all">
          Exclude <code>{rule.exclude.join(", ")}</code>
        </p>
      )}
      <ul className="ml-5 list-disc text-sm">
        {!!rule.bypass_actors?.length && (
          <li>
            {rule.bypass_actors.length} bypass{" "}
            {rule.bypass_actors.length === 1 ? "grant" : "grants"}
          </li>
        )}
        {rule.restrict_creations && <li>Restrict creations</li>}
        {rule.restrict_updates && <li>Restrict updates</li>}
        {rule.restrict_deletions && <li>Restrict deletions</li>}
        {rule.force_pushes === "allow" && <li>Allow force pushes</li>}
        {rule.force_pushes === "block" && <li>Block force pushes</li>}
        {rule.require_linear_history && <li>Require linear history</li>}
        {(rule.require_pull_request ||
          rule.require_merge_queue ||
          rule.required_approvals > 0 ||
          rule.required_code_owner_reviews ||
          rule.require_last_push_approval ||
          rule.require_conversation_resolution ||
          rule.required_checks.length > 0 ||
          rule.required_integrations.length > 0) && (
          <li>Require a pull request before updating</li>
        )}
        {rule.required_approvals > 0 && (
          <li>
            Require {rule.required_approvals} approving{" "}
            {rule.required_approvals === 1 ? "review" : "reviews"}
          </li>
        )}
        {rule.required_code_owner_reviews && <li>Require code owner review</li>}
        {rule.require_last_push_approval && (
          <li>Require approval of the most recent reviewable push</li>
        )}
        {rule.require_conversation_resolution && (
          <li>Require conversation resolution</li>
        )}
        {rule.require_up_to_date && (
          <li>Require branches to be up to date before merging</li>
        )}
        {rule.require_merge_queue && <li>Require merge queue</li>}
        {rule.review_dismissal !== undefined && (
          <li>Restrict review dismissal</li>
        )}
        {rule.required_checks.length > 0 && (
          <li>
            Required checks{" "}
            <span className="break-all">{rule.required_checks.join(", ")}</span>
          </li>
        )}
        {rule.required_integrations.length > 0 && (
          <li>
            Require {rule.required_integrations.length} GitHub Actions{" "}
            {rule.required_integrations.length === 1 ? "workflow" : "workflows"}
          </li>
        )}
      </ul>
      {rule.repositories && (
        <RepositoryTargetSummary
          target={rule.repositories}
          definitions={definitions}
        />
      )}
      {children}
    </article>
  );
}

const lineValues = (text: string) =>
  text
    .split("\n")
    .map((value) => value.trim())
    .filter(Boolean);

function RulesetForm({
  initial,
  repo,
  organization,
  definitions,
  workflows,
  workflowsError,
  bypassOptions,
  saving,
  onCancel,
  onSave,
}: {
  initial: Ruleset;
  repo?: Repository;
  organization?: string;
  definitions?: PropertyDefinition[];
  workflows: Connections["items"] | undefined;
  workflowsError?: string | null;
  bypassOptions?: BypassOptions;
  saving: boolean;
  onCancel: () => void;
  onSave: (rule: Ruleset) => Promise<void>;
}) {
  const [rule, setRule] = useState(initial);
  const [include, setInclude] = useState(initial.include.join("\n"));
  const [exclude, setExclude] = useState(initial.exclude.join("\n"));
  const [checks, setChecks] = useState(initial.required_checks.join("\n"));
  const [approvals, setApprovals] = useState(
    String(initial.required_approvals),
  );
  const branch = rule.target === "branch";
  const requiredChecks = lineValues(checks);
  const hasRequiredChecks =
    requiredChecks.length > 0 || rule.required_integrations.length > 0;
  const requiresPull =
    rule.require_pull_request ||
    rule.require_merge_queue ||
    Number(approvals) > 0 ||
    rule.required_code_owner_reviews ||
    rule.require_last_push_approval ||
    rule.require_conversation_resolution ||
    hasRequiredChecks;
  const restrictDismissal = rule.review_dismissal !== undefined;
  const invalidDismissal = branch && restrictDismissal && !requiresPull;
  const repositories = rule.repositories;
  const invalidRepositories =
    (repositories?.kind === "selected" && !repositories.ids.length) ||
    (repositories?.kind === "properties" &&
      (!repositories.conditions.length ||
        repositories.conditions.some(
          (condition) =>
            condition.value === null ||
            ("system" in condition
              ? condition.system === "fork"
                ? typeof condition.value !== "boolean"
                : typeof condition.value !== "string" || !condition.value.trim()
              : !definitions?.some(
                  (property) => property.id === condition.property,
                )),
        )));
  const invalidBypass =
    !branch && rule.bypass_actors?.some(({ mode }) => mode === "pull_request");
  function change(update: Partial<Ruleset>) {
    setRule((current) => ({ ...current, ...update }));
  }
  function submit(event: FormEvent) {
    event.preventDefault();
    const requirements = branch
      ? {
          required_approvals: Number(approvals),
          required_checks: requiredChecks,
        }
      : pullRules;
    void onSave({
      ...rule,
      ...requirements,
      name: rule.name.trim(),
      include: lineValues(include),
      exclude: lineValues(exclude),
    });
  }
  const fieldClass = "w-full rounded-md border bg-background px-3 py-2 text-sm";
  const missingWorkflows = workflows
    ? rule.required_integrations.filter(
        (id) => !workflows.some((workflow) => workflow.id === id),
      )
    : [];
  return (
    <form
      aria-label="Ruleset editor"
      className="settings-section p-4"
      onSubmit={submit}
    >
      <fieldset disabled={saving} className="grid gap-3">
        <legend className="sr-only">Ruleset details</legend>
        <label className="grid gap-1">
          Name
          <Input
            value={rule.name}
            maxLength={100}
            required
            onChange={(event) => change({ name: event.target.value })}
          />
        </label>
        <div className="grid gap-3 sm:grid-cols-2">
          <label className="grid gap-1">
            Target
            <FieldSelect
              className="w-full"
              value={rule.target}
              onValueChange={(nextValue) => {
                const target = nextValue === "tag" ? "tag" : "branch";
                if (target === "tag") {
                  if (include === "~DEFAULT_BRANCH") setInclude("~ALL");
                  if (exclude === "~DEFAULT_BRANCH") setExclude("");
                }
                change({ target });
              }}
              options={[
                { value: "branch", label: "Branches" },
                { value: "tag", label: "Tags" },
              ]}
            />
          </label>
          <label className="grid gap-1">
            Enforcement
            <FieldSelect
              className="w-full"
              value={rule.enforcement}
              onValueChange={(nextValue) =>
                change({
                  enforcement: nextValue === "disabled" ? "disabled" : "active",
                })
              }
              options={[
                { value: "active", label: "Active" },
                { value: "disabled", label: "Disabled" },
              ]}
            />
          </label>
        </div>
        {organization && rule.repositories && (
          <RepositoryTargetEditor
            organization={organization}
            definitions={definitions}
            target={rule.repositories}
            onChange={(repositories) => change({ repositories })}
          />
        )}
        <p className="text-sm text-muted-foreground" id="ruleset-pattern-help">
          Enter one short ref pattern per line. Use <code>~ALL</code> for every
          target
          {branch && (
            <>
              {" "}
              or <code>~DEFAULT_BRANCH</code> for the current default branch
            </>
          )}
          . Use <code>*</code> within a path segment and <code>**</code> across
          segments.
        </p>
        <div className="grid gap-3 sm:grid-cols-2">
          <label className="grid gap-1">
            Include patterns
            <Textarea
              className={fieldClass}
              value={include}
              required
              aria-describedby="ruleset-pattern-help"
              onChange={(event) => setInclude(event.target.value)}
            />
          </label>
          <label className="grid gap-1">
            Exclude patterns
            <Textarea
              className={fieldClass}
              value={exclude}
              onChange={(event) => setExclude(event.target.value)}
            />
          </label>
        </div>
        <label className="flex items-center gap-2">
          <Checkbox
            checked={rule.restrict_creations}
            onCheckedChange={(checked) =>
              change({ restrict_creations: checked === true })
            }
          />{" "}
          Restrict creations
        </label>
        <label className="flex items-center gap-2">
          <Checkbox
            checked={Boolean(rule.restrict_updates)}
            onCheckedChange={(checked) =>
              change({ restrict_updates: checked === true })
            }
          />{" "}
          Restrict updates
        </label>
        <label className="flex items-center gap-2">
          <Checkbox
            checked={rule.restrict_deletions}
            onCheckedChange={(checked) =>
              change({ restrict_deletions: checked === true })
            }
          />{" "}
          Restrict deletions
        </label>
        {branch && (
          <div className="grid gap-1">
            <label htmlFor="ruleset-force-pushes">Force pushes</label>
            <FieldSelect
              id="ruleset-force-pushes"
              aria-describedby="ruleset-force-push-help"
              value={rule.force_pushes ?? "default"}
              options={{
                default: "Use repository policy",
                allow: "Allow force pushes",
                block: "Block force pushes",
              }}
              onValueChange={(value) =>
                change({
                  force_pushes:
                    value === "allow" || value === "block" ? value : undefined,
                })
              }
            />
            <p
              id="ruleset-force-push-help"
              className="text-sm text-muted-foreground"
            >
              Allow history rewrites on matching branches. A matching block rule
              takes precedence unless a bypass applies.
            </p>
          </div>
        )}
        <label className="flex items-center gap-2">
          <Checkbox
            checked={Boolean(rule.require_linear_history)}
            onCheckedChange={(checked) =>
              change({ require_linear_history: checked === true })
            }
          />{" "}
          Require linear history
        </label>
        {branch && (
          <>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={rule.require_pull_request}
                onCheckedChange={(checked) =>
                  change({ require_pull_request: checked === true })
                }
              />{" "}
              Require a pull request before updating
            </label>
            <label className="grid gap-1">
              Required approving reviews
              <Input
                type="number"
                min={0}
                max={20}
                required
                value={approvals}
                onChange={(event) => setApprovals(event.target.value)}
              />
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={rule.required_code_owner_reviews}
                onCheckedChange={(checked) =>
                  change({ required_code_owner_reviews: checked === true })
                }
              />{" "}
              Require code owner review
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={Boolean(rule.require_last_push_approval)}
                onCheckedChange={(checked) =>
                  change({ require_last_push_approval: checked === true })
                }
              />
              Require approval of the most recent reviewable push
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={rule.require_conversation_resolution}
                onCheckedChange={(checked) =>
                  change({
                    require_conversation_resolution: checked === true,
                  })
                }
              />{" "}
              Require conversation resolution
            </label>
            <label className="grid gap-1">
              Required check names
              <Textarea
                className={fieldClass}
                placeholder="One check per line"
                value={checks}
                onChange={(event) => setChecks(event.target.value)}
              />
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={Boolean(rule.require_up_to_date)}
                disabled={!hasRequiredChecks && !rule.require_up_to_date}
                onCheckedChange={(checked) =>
                  change({ require_up_to_date: checked === true })
                }
              />
              Require branches to be up to date before merging
            </label>
            <label className="flex items-center gap-2">
              <Checkbox
                checked={Boolean(rule.require_merge_queue)}
                disabled={!hasRequiredChecks && !rule.require_merge_queue}
                onCheckedChange={(checked) =>
                  change({ require_merge_queue: checked === true })
                }
              />
              Require merge queue
            </label>
            {!hasRequiredChecks && (
              <p className="text-sm text-muted-foreground">
                {organization
                  ? "Add a required check to enable either setting."
                  : "Add a required check or workflow to enable either setting."}
              </p>
            )}
            {workflowsError && (
              <p role="status">GitHub Actions workflows could not be loaded.</p>
            )}
            {[
              ...(workflows ?? []),
              ...missingWorkflows.map((id) => ({
                id,
                name: `Disconnected workflow ${id}`,
              })),
            ].map(({ id, name }) => (
              <label className="flex items-center gap-2" key={id}>
                <Checkbox
                  checked={rule.required_integrations.includes(id)}
                  onCheckedChange={(checked) =>
                    change({
                      required_integrations:
                        checked === true
                          ? [...rule.required_integrations, id]
                          : rule.required_integrations.filter(
                              (value) => value !== id,
                            ),
                    })
                  }
                />{" "}
                Require {name}
              </label>
            ))}
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={restrictDismissal}
                disabled={!requiresPull && !restrictDismissal}
                onChange={(event) =>
                  change({
                    review_dismissal: event.target.checked ? [] : undefined,
                  })
                }
              />
              Restrict who can dismiss pull request reviews
            </label>
            {(!requiresPull || invalidDismissal) && (
              <p className="text-sm text-muted-foreground">
                Add a pull request requirement to configure review dismissal.
              </p>
            )}
            {rule.review_dismissal !== undefined && (
              <ReviewDismissalEditor
                actors={rule.review_dismissal}
                onChange={(review_dismissal) => change({ review_dismissal })}
                repo={repo}
                organization={organization}
                teams={bypassOptions?.teams}
              />
            )}
          </>
        )}
        <RulesetBypassEditor
          branch={branch}
          grants={rule.bypass_actors ?? []}
          options={bypassOptions}
          organization={organization}
          onChange={(bypass_actors) => change({ bypass_actors })}
        />
        {invalidBypass && (
          <p role="status">
            Tag rulesets require Always bypass mode. Update or remove
            pull-request-only grants.
          </p>
        )}
        <div className="flex gap-2">
          <Button
            type="submit"
            disabled={
              invalidBypass ||
              invalidDismissal ||
              invalidRepositories ||
              (branch &&
                (rule.require_up_to_date || rule.require_merge_queue) &&
                !hasRequiredChecks)
            }
          >
            {saving ? "Saving…" : "Save ruleset"}
          </Button>
          <Button type="button" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
        </div>
      </fieldset>
    </form>
  );
}
