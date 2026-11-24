import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { navigate, useRequest } from "@/lib/api";
import type {
  OrganizationPage,
  OutsideCollaboratorGrant,
} from "@/lib/contracts";
import { repositoryRoleLabels } from "@/lib/repository-roles";
import { repoHref } from "@/lib/repository-route";

export function OutsideCollaborators({
  endpoint,
  url,
}: {
  endpoint: string;
  url: URL;
}) {
  const search = url.searchParams.get("q") ?? "";
  const after = url.searchParams.get("after");
  const params = new URLSearchParams();
  if (search) params.set("q", search);
  if (after) params.set("after", after);
  const state = useRequest<OrganizationPage<OutsideCollaboratorGrant>>(
    `${endpoint}/outside-collaborators?${params}`,
  );
  function showPage(query: string, cursor?: string | null) {
    const params = new URLSearchParams(url.searchParams);
    if (query) params.set("q", query);
    else params.delete("q");
    if (cursor) params.set("after", cursor);
    else params.delete("after");
    navigate(`${url.pathname}?${params}`);
  }
  return (
    <section aria-labelledby="outside-collaborators" className="grid gap-4">
      <div>
        <h2 id="outside-collaborators" className="text-lg font-semibold">
          Outside collaborators
        </h2>
        <p className="text-sm text-muted-foreground">
          Direct repository grants for people outside the organization.
        </p>
      </div>
      <form
        role="search"
        aria-label="Outside collaborator search"
        className="flex flex-wrap items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          const query = new FormData(event.currentTarget).get("q");
          if (typeof query === "string") showPage(query.trim());
        }}
      >
        <Field className="min-w-0 sm:max-w-xs">
          <FieldLabel htmlFor="outside-collaborator-search">
            Find outside collaborators
          </FieldLabel>
          <Input
            key={search}
            id="outside-collaborator-search"
            name="q"
            type="search"
            defaultValue={search}
            maxLength={100}
            placeholder="Grant name or repository"
          />
        </Field>
        <Button type="submit" variant="outline">
          Search
        </Button>
      </form>
      <Result state={state} timingLabel="Outside collaborators">
        {({ items, next }) =>
          items.length ? (
            <ul
              className="divide-y rounded-lg border"
              aria-label="Outside collaborator grants"
            >
              {items.map((item) => (
                <li
                  key={`${item.repository_id}:${item.name}`}
                  className="flex flex-wrap items-center gap-3 p-4"
                >
                  <div className="min-w-0 flex-1 basis-40">
                    {item.profile ? (
                      <Link
                        href={`/-/users/${item.profile.id}`}
                        className="font-semibold wrap-anywhere hover:underline"
                        dir="auto"
                      >
                        {item.name}{" "}
                        <span className="font-normal text-muted-foreground">
                          @{item.profile.handle}
                        </span>
                      </Link>
                    ) : (
                      <p className="font-semibold wrap-anywhere" dir="auto">
                        {item.name}
                      </p>
                    )}
                    <p className="text-sm wrap-anywhere text-muted-foreground">
                      {item.repository}
                    </p>
                  </div>
                  <Badge variant="outline">
                    {repositoryRoleLabels[item.access]}
                  </Badge>
                  <Button asChild size="sm" variant="outline">
                    <Link
                      href={repoHref(
                        { owner: item.owner, name: item.repository },
                        { view: "settings", section: "members" },
                      )}
                      aria-label={`Manage ${item.repository} access for ${item.name}`}
                    >
                      Manage access
                    </Link>
                  </Button>
                </li>
              ))}
            </ul>
          ) : (
            <p
              role="status"
              className="rounded-lg border p-6 text-sm text-muted-foreground"
            >
              {next || after
                ? "No outside collaborator grants on this page."
                : "No matching outside collaborator grants."}
            </p>
          )
        }
      </Result>
      {(after || state.data?.next) && (
        <div className="flex flex-wrap gap-2">
          <Button
            type="button"
            variant="outline"
            disabled={!after}
            onClick={() => showPage(search)}
          >
            First page
          </Button>
          <Button
            type="button"
            variant="outline"
            disabled={!state.data?.next}
            onClick={() => showPage(search, state.data?.next)}
          >
            Next page
          </Button>
        </div>
      )}
    </section>
  );
}
