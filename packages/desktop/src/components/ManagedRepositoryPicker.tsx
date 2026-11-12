import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import clsx from "clsx";
import {
  AlertCircle,
  Building2,
  Check,
  Cloud,
  Loader2,
  Plus,
  RefreshCw,
  Server,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  nextJobId,
  rpc,
  type ManagedOrganization,
  type ManagedProfilesResponse,
  type ManagedRepository,
} from "../ipc";
import {
  managedErrorDetails,
  managedRepositoryAccess,
  type ManagedErrorDetails,
} from "../lib/managed-service";
import { useJobLines } from "../lib/useJobLines";

interface Props {
  selectedUrl: string;
  onSelectRepository: (url: string) => void;
}

const ACCESS_TONE = {
  success: "border-success/30 bg-success/10 text-success",
  warn: "border-warning/30 bg-warning/10 text-warning",
  muted: "border-border bg-background text-muted-foreground",
  error: "border-destructive/30 bg-destructive/10 text-destructive",
} as const;

export function ManagedRepositoryPicker({ selectedUrl, onSelectRepository }: Props) {
  const [profiles, setProfiles] = useState<ManagedProfilesResponse | null>(null);
  const [authority, setAuthority] = useState("");
  const [organizations, setOrganizations] = useState<ManagedOrganization[]>([]);
  const [organizationCursor, setOrganizationCursor] = useState<string | null>(null);
  const [organization, setOrganization] = useState("");
  const [repositories, setRepositories] = useState<ManagedRepository[]>([]);
  const [repositoryCursor, setRepositoryCursor] = useState<string | null>(null);
  const [serviceOrigin, setServiceOrigin] = useState("https://crab.build");
  const [repositorySlug, setRepositorySlug] = useState("");
  const [loadingProfiles, setLoadingProfiles] = useState(true);
  const [loadingOrganizations, setLoadingOrganizations] = useState(false);
  const [loadingRepositories, setLoadingRepositories] = useState(false);
  const [signingIn, setSigningIn] = useState(false);
  const [creating, setCreating] = useState(false);
  const [loginJobId, setLoginJobId] = useState<string | null>(null);
  const [error, setError] = useState<ManagedErrorDetails | null>(null);
  const organizationRequest = useRef(0);
  const repositoryRequest = useRef(0);
  const loginLines = useJobLines(loginJobId);
  const loadingCatalog = loadingOrganizations || loadingRepositories;

  const applyProfiles = useCallback((result: ManagedProfilesResponse) => {
    setProfiles(result);
    const selected = result.active_authority ?? result.profiles[0]?.authority ?? "";
    setAuthority(selected);
  }, []);

  const loadProfiles = useCallback(async () => {
    setLoadingProfiles(true);
    setError(null);
    try {
      applyProfiles(await rpc.managedProfiles());
    } catch (cause) {
      setError(managedErrorDetails(cause));
    } finally {
      setLoadingProfiles(false);
    }
  }, [applyProfiles]);

  const loadOrganizations = useCallback(
    async (service: string, cursor?: string, append = false) => {
      const request = ++organizationRequest.current;
      setLoadingOrganizations(true);
      setError(null);
      try {
        const page = await rpc.managedOrganizations(service, cursor);
        if (request !== organizationRequest.current) return;
        setOrganizations((current) => append ? [...current, ...page.organizations] : page.organizations);
        setOrganizationCursor(page.next_cursor);
        if (!append) {
          setOrganization((current) =>
            page.organizations.some((item) => item.slug === current)
              ? current
              : (page.organizations[0]?.slug ?? ""),
          );
        }
      } catch (cause) {
      if (request === organizationRequest.current) {
        setError(managedErrorDetails(cause));
      }
    } finally {
      if (request === organizationRequest.current) {
        setLoadingOrganizations(false);
      }
    }
    },
    [],
  );

  const loadRepositories = useCallback(async (
    service: string,
    organizationSlug: string,
    cursor?: string,
    append = false,
  ) => {
    const request = ++repositoryRequest.current;
    setLoadingRepositories(true);
    setError(null);
    try {
      const page = await rpc.managedRepositories(service, organizationSlug, cursor);
      if (request !== repositoryRequest.current) return;
      setRepositories((current) => append ? [...current, ...page.repositories] : page.repositories);
      setRepositoryCursor(page.next_cursor);
    } catch (cause) {
      if (request === repositoryRequest.current) {
        setError(managedErrorDetails(cause));
      }
    } finally {
      if (request === repositoryRequest.current) {
        setLoadingRepositories(false);
      }
    }
  }, []);

  useEffect(() => {
    void loadProfiles();
  }, [loadProfiles]);

  useEffect(() => {
    setOrganizations([]);
    setOrganization("");
    setRepositories([]);
    setOrganizationCursor(null);
    setRepositoryCursor(null);
    onSelectRepository("");
    if (authority) {
      void loadOrganizations(authority);
    } else {
      organizationRequest.current += 1;
      setLoadingOrganizations(false);
    }
  }, [authority, loadOrganizations, onSelectRepository]);

  useEffect(() => {
    setRepositories([]);
    setRepositoryCursor(null);
    onSelectRepository("");
    if (authority && organization) {
      void loadRepositories(authority, organization);
    } else {
      repositoryRequest.current += 1;
      setLoadingRepositories(false);
    }
  }, [authority, organization, loadRepositories, onSelectRepository]);

  const activeOrganization = useMemo(
    () => organizations.find((item) => item.slug === organization),
    [organization, organizations],
  );

  async function selectProfile(nextAuthority: string) {
    setError(null);
    try {
      const result = await rpc.managedSelectProfile(nextAuthority);
      applyProfiles(result);
      setAuthority(nextAuthority);
    } catch (cause) {
      setError(managedErrorDetails(cause));
    }
  }

  async function signIn() {
    const origin = serviceOrigin.trim();
    if (!origin) return;
    const jobId = nextJobId("managed-login");
    setLoginJobId(jobId);
    setSigningIn(true);
    setError(null);
    try {
      applyProfiles(await rpc.managedLogin(origin, jobId));
    } catch (cause) {
      setError(managedErrorDetails(cause));
    } finally {
      setSigningIn(false);
      setLoginJobId(null);
    }
  }

  async function createRepository() {
    const slug = repositorySlug.trim();
    if (!authority || !organization || !slug) return;
    setCreating(true);
    setError(null);
    try {
      const created = await rpc.managedCreateRepository(
        authority,
        `${organization}/${slug}`,
      );
      setRepositorySlug("");
      setRepositories((current) => [created, ...current]);
      if (managedRepositoryAccess(created).canClone) {
        onSelectRepository(created.canonical_url);
      }
    } catch (cause) {
      setError(managedErrorDetails(cause));
    } finally {
      setCreating(false);
    }
  }

  if (loadingProfiles && profiles === null) {
    return (
      <div className="flex items-center justify-center rounded-lg border border-border bg-background py-10 text-sm text-muted-foreground">
        <Loader2 size={16} className="mr-2 animate-spin" />
        Loading managed services…
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <section className="rounded-lg border border-border bg-background p-3">
        <div className="flex items-start gap-3">
          <div className="rounded-md bg-primary/10 p-2 text-primary">
            <Cloud size={18} />
          </div>
          <div className="min-w-0 flex-1">
            <h3 className="text-sm font-medium text-foreground">
              {profiles?.profiles.length ? "Managed service" : "Connect a managed service"}
            </h3>
            <p className="mt-0.5 text-[11px] text-muted-foreground">
              Sign in to Crab Cloud or an enterprise deployment. Credentials
              stay in the shared Crab token store.
            </p>
          </div>
        </div>

        {profiles?.profiles.length ? (
          <div className="mt-3 grid gap-2 sm:grid-cols-[1fr_auto]">
            <label className="min-w-0">
              <span className="mb-1 block text-[11px] text-muted-foreground">
                Service profile
              </span>
              <Select
                value={":" + String(authority)}
                onValueChange={(selectedValue) => {
                  const value = selectedValue.slice(1);
                  return void selectProfile(value);
                }}
              >
                <SelectTrigger className="w-full text-sm">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectGroup>
                    {profiles.profiles.map((profile) => (
                      <SelectItem
                        key={profile.authority}
                        value={":" + String(profile.authority)}
                      >
                        {profile.authority}
                        {profile.discovered ? "" : " (not discovered)"}
                      </SelectItem>
                    ))}
                  </SelectGroup>
                </SelectContent>
              </Select>
            </label>
            <Button variant="outline"
              type="button"
              className="self-end"
              onClick={() => authority && void loadOrganizations(authority)}
              disabled={!authority || loadingCatalog}
            >
              <RefreshCw size={13} className={loadingCatalog ? "animate-spin" : ""} />
              Refresh
            </Button>
          </div>
        ) : null}

        <div className="mt-3 flex gap-2">
          <Input
            className="min-w-0 flex-1 font-mono text-xs"
            aria-label="Managed service origin"
            value={serviceOrigin}
            onChange={(event) => setServiceOrigin(event.target.value)}
            placeholder="https://crab.build"
          />
          <Button variant="default"
            type="button"
            className="shrink-0"
            onClick={() => void signIn()}
            disabled={signingIn || !serviceOrigin.trim()}
          >
            {signingIn ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <Server size={13} />
            )}
            {profiles?.profiles.length ? "Add service" : "Sign in"}
          </Button>
        </div>
        {signingIn && loginLines.length > 0 && (
          <p className="mt-2 truncate font-mono text-[10px] text-muted-foreground">
            {loginLines[loginLines.length - 1]?.line}
          </p>
        )}
      </section>

      {profiles?.profiles.length && authority ? (
        <>
          <section className="rounded-lg border border-border p-3">
            <div className="flex items-end gap-2">
              <label className="min-w-0 flex-1">
                <span className="mb-1 flex items-center gap-1 text-[11px] text-muted-foreground">
                  <Building2 size={12} /> Organization
                </span>
                <Select
                  value={":" + String(organization)}
                  onValueChange={(selectedValue) => {
                    const value = selectedValue.slice(1);
                    return setOrganization(value);
                  }}
                  disabled={organizations.length === 0}
                >
                  <SelectTrigger className="w-full text-sm">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      {organizations.length === 0 && (
                        <SelectItem value={":" + String("")}>
                          No organizations
                        </SelectItem>
                      )}
                      {organizations.map((item) => (
                        <SelectItem
                          key={item.id}
                          value={":" + String(item.slug)}
                        >
                          {item.slug}
                          {item.state === "active" ? "" : ` (${item.state})`}
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  </SelectContent>
                </Select>
              </label>
              {organizationCursor && (
                <Button variant="outline"
                  type="button"

                  onClick={() => void loadOrganizations(authority, organizationCursor, true)}
                  disabled={loadingCatalog}
                >
                  More
                </Button>
              )}
            </div>

            <div className="mt-3 flex gap-2 border-t border-border pt-3">
              <Input
                className="min-w-0 flex-1 font-mono text-xs"
                aria-label="New repository name"
                value={repositorySlug}
                onChange={(event) => setRepositorySlug(event.target.value)}
                placeholder="new-repository"
                disabled={activeOrganization?.state !== "active"}
              />
              <Button variant="outline"
                type="button"
                className="shrink-0"
                onClick={() => void createRepository()}
                disabled={
                  creating ||
                  activeOrganization?.state !== "active" ||
                  !repositorySlug.trim()
                }
              >
                {creating ? (
                  <Loader2 size={13} className="animate-spin" />
                ) : (
                  <Plus size={13} />
                )}
                Create
              </Button>
            </div>
          </section>

          <section aria-label="Managed repository catalog">
            <div className="mb-2 flex items-center justify-between">
              <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
                Repositories
              </h3>
              {repositoryCursor && (
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="text-[11px] text-primary hover:text-primary-hover"
                  onClick={() => void loadRepositories(
                    authority,
                    organization,
                    repositoryCursor,
                    true,
                  )}
                  disabled={loadingCatalog}
                >
                  Load more
                </Button>
              )}
            </div>
            <div className="space-y-2">
              {!loadingCatalog && repositories.length === 0 && (
                <div className="rounded-lg border border-dashed border-border px-4 py-7 text-center text-xs text-muted-foreground">
                  No repositories are visible in this organization.
                </div>
              )}
              {repositories.map((repository) => {
                const access = managedRepositoryAccess(repository);
                const selected = selectedUrl === repository.canonical_url;
                return (
                  <Button
                    variant="ghost"
                    size="compact"
                    key={repository.repository_id}
                    type="button"
                    className={clsx(
                      "w-full rounded-lg border p-3 text-left transition",
                      selected
                        ? "border-primary bg-primary/10"
                        : "border-border bg-background hover:border-primary/50",
                      !access.canClone && "cursor-not-allowed opacity-75",
                    )}
                    onClick={() => access.canClone && onSelectRepository(repository.canonical_url)}
                    disabled={!access.canClone}
                  >
                    <div className="flex items-start gap-2">
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="truncate font-mono text-xs text-foreground">
                            {repository.canonical_url}
                          </span>
                          {selected && (
                            <Check size={13} className="shrink-0 text-primary" />
                          )}
                        </div>
                        <p className="mt-1 text-[10px] text-muted-foreground">
                          {access.detail}
                        </p>
                      </div>
                      <span className={clsx(
                        "shrink-0 rounded border px-1.5 py-0.5 text-[10px]",
                        ACCESS_TONE[access.tone],
                      )}>
                        {access.label}
                      </span>
                    </div>
                  </Button>
                );
              })}
            </div>
          </section>
        </>
      ) : null}

      {loadingCatalog && repositories.length === 0 && authority && (
        <div className="flex items-center justify-center py-4 text-xs text-muted-foreground">
          <Loader2 size={14} className="mr-2 animate-spin" /> Loading catalog…
        </div>
      )}

      {error && (
        <div className="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
          <AlertCircle size={14} className="mt-0.5 shrink-0" />
          <div>
            <p>{error.message}</p>
            {error.remediation && (
              <p className="mt-1 opacity-80">{error.remediation}</p>
            )}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="mt-2 font-medium underline underline-offset-2"
              onClick={() => authority
                ? void loadOrganizations(authority)
                : void loadProfiles()}
            >
              Retry
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
