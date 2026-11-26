import { KeyIcon, RepoIcon } from "@primer/octicons-react";
import {
  BellIcon,
  CheckIcon,
  EyeIcon,
  HomeIcon,
  LogOutIcon,
  MonitorIcon,
  MoonIcon,
  SunIcon,
} from "lucide-react";
import { useState } from "react";
import type { Theme } from "@/app/theme/control";
import { Link } from "@/components/navigation/link";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Disclosure } from "@/components/ui/collapsible";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { EmailSettings } from "@/features/notifications/email";
import {
  NewOrganizationButton,
  OrganizationDirectory,
} from "@/features/organizations/directory";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { useRequest, type Repository } from "@/lib/api";
import { codeThemeChoices, type CodeTheme } from "@/lib/code-theme";
import type { Session } from "@/lib/contracts";
import { cn } from "@/lib/utils";

const PINNED_TAB_KEY = "crab-home-pinned-tab";

export function AccountSettings({
  session,
  signingOut,
  onSignOut,
}: {
  session: Session;
  signingOut: boolean;
  onSignOut: () => void;
}) {
  const [defaultHome, setDefaultHome] = useState<"home" | "feed">(() => {
    try {
      return localStorage.getItem(PINNED_TAB_KEY) === "feed" ? "feed" : "home";
    } catch {
      return "home";
    }
  });

  const updateDefaultHome = (next: "home" | "feed") => {
    setDefaultHome(next);
    try {
      localStorage.setItem(PINNED_TAB_KEY, next);
    } catch {
      // Ignore storage failures.
    }
  };

  return (
    <section className="grid min-w-0 gap-6">
      <div className="border-b pb-4">
        <h1 className="mb-3 text-2xl font-semibold tracking-tight">Account</h1>
        <p className="m-0 text-sm text-muted-foreground">
          Review your sign-in identity, homepage preference, and active session.
        </p>
      </div>

      <div className="grid gap-3 rounded-lg border border-border bg-card p-4">
        <h2 className="m-0 text-base font-semibold">Identity</h2>
        <dl className="m-0 grid gap-3 text-sm sm:grid-cols-2">
          <div>
            <dt className="text-xs font-medium text-muted-foreground">
              Session mode
            </dt>
            <dd className="m-0 mt-1 font-medium capitalize">{session.mode}</dd>
          </div>
          <div>
            <dt className="text-xs font-medium text-muted-foreground">
              Display name
            </dt>
            <dd className="m-0 mt-1 font-medium">
              {session.user?.name ?? "Local session"}
            </dd>
          </div>
          {session.user && (
            <>
              <div>
                <dt className="text-xs font-medium text-muted-foreground">
                  Subject ID
                </dt>
                <dd className="m-0 mt-1 font-mono text-xs break-all">
                  {session.user.subject}
                </dd>
              </div>
              <div>
                <dt className="text-xs font-medium text-muted-foreground">
                  Identity issuer
                </dt>
                <dd className="m-0 mt-1 font-mono text-xs break-all">
                  {session.user.issuer}
                </dd>
              </div>
            </>
          )}
        </dl>
      </div>

      <div className="grid gap-3 rounded-lg border border-border bg-card p-4">
        <div className="flex items-center gap-2">
          <HomeIcon
            className="size-4 text-muted-foreground"
            aria-hidden="true"
          />
          <h2 className="m-0 text-base font-semibold">Default homepage</h2>
        </div>
        <p className="m-0 text-sm text-muted-foreground">
          Choose which dashboard view opens when you visit Crab.
        </p>
        <div
          className="flex flex-wrap gap-2"
          role="group"
          aria-label="Default homepage"
        >
          {(
            [
              { value: "home", label: "Home dashboard" },
              { value: "feed", label: "Activity feed" },
            ] as const
          ).map(({ value, label }) => (
            <Button
              key={value}
              type="button"
              size="sm"
              variant={defaultHome === value ? "secondary" : "outline"}
              aria-pressed={defaultHome === value}
              onClick={() => updateDefaultHome(value)}
            >
              {defaultHome === value && (
                <CheckIcon data-icon="inline-start" aria-hidden="true" />
              )}
              {label}
            </Button>
          ))}
        </div>
      </div>

      {session.user && (
        <div className="grid gap-3 rounded-lg border border-border bg-card p-4">
          <h2 className="m-0 text-base font-semibold">Current session</h2>
          <p className="m-0 text-sm text-muted-foreground">
            Signing out clears this browser session and revokes derived Git
            access tokens issued during this sign-in.
          </p>
          <div>
            <Button
              type="button"
              variant="outline"
              disabled={signingOut}
              onClick={onSignOut}
            >
              <LogOutIcon data-icon="inline-start" aria-hidden="true" />
              {signingOut ? "Signing out…" : "Sign out"}
            </Button>
          </div>
        </div>
      )}
    </section>
  );
}

export function AppearanceSettings({
  theme,
  setTheme,
  codeTheme,
  setCodeTheme,
}: {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  codeTheme: CodeTheme;
  setCodeTheme: (theme: CodeTheme) => void;
}) {
  const themeCards = [
    {
      value: "auto" as const,
      label: "Sync with system",
      description: "Automatically match your operating system appearance.",
      Icon: MonitorIcon,
      preview: "bg-gradient-to-br from-white via-zinc-100 to-zinc-900",
    },
    {
      value: "light" as const,
      label: "Light default",
      description: "High-contrast light workspace for daytime reading.",
      Icon: SunIcon,
      preview: "bg-white",
    },
    {
      value: "dark" as const,
      label: "Dark default",
      description: "Low-glare dark surfaces matching GitHub dark mode.",
      Icon: MoonIcon,
      preview: "bg-zinc-950",
    },
  ];

  return (
    <section className="grid min-w-0 gap-6">
      <div className="border-b pb-4">
        <h1 className="mb-3 text-2xl font-semibold tracking-tight">
          Appearance
        </h1>
        <p className="m-0 text-sm text-muted-foreground">
          Choose how Crab looks to you. Select a single theme or sync with your
          system, and pick a syntax highlighting palette for code and diffs.
        </p>
      </div>

      <div className="grid gap-3">
        <h2 className="m-0 text-base font-semibold">Theme mode</h2>
        <div
          className="grid gap-3 sm:grid-cols-3"
          role="radiogroup"
          aria-label="Theme mode"
        >
          {themeCards.map(({ value, label, description, Icon, preview }) => {
            const active = theme === value;
            return (
              <button
                key={value}
                type="button"
                role="radio"
                aria-checked={active}
                onClick={() => setTheme(value)}
                className={cn(
                  "flex flex-col overflow-hidden rounded-lg border text-left transition-colors focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                  active
                    ? "border-(--info) ring-1 ring-(--info)"
                    : "border-border hover:border-input-hover",
                )}
              >
                <div
                  className={cn(
                    "h-20 w-full border-b border-border p-3",
                    preview,
                  )}
                  aria-hidden="true"
                >
                  <div className="h-2.5 w-16 rounded bg-current/15" />
                  <div className="mt-2 h-2 w-24 rounded bg-current/10" />
                </div>
                <div className="grid gap-1 bg-card p-3">
                  <span className="flex items-center justify-between gap-2 text-sm font-medium text-foreground">
                    <span className="flex items-center gap-1.5">
                      <Icon className="size-3.5" aria-hidden="true" />
                      {label}
                    </span>
                    {active && (
                      <Badge variant="secondary" className="text-[10px]">
                        Active
                      </Badge>
                    )}
                  </span>
                  <span className="text-xs text-muted-foreground">
                    {description}
                  </span>
                </div>
              </button>
            );
          })}
        </div>
      </div>

      <div className="grid gap-3">
        <h2 className="m-0 text-base font-semibold">Code syntax theme</h2>
        <p className="m-0 text-sm text-muted-foreground">
          Applied across file viewers, code search matches, and pull request
          diffs.
        </p>
        <div
          className="grid gap-2.5 sm:grid-cols-2"
          role="radiogroup"
          aria-label="Code syntax theme"
        >
          {Object.entries(codeThemeChoices).map(([value, choice]) => {
            const active = codeTheme === value;
            return (
              <button
                key={value}
                type="button"
                role="radio"
                aria-checked={active}
                onClick={() => setCodeTheme(value as CodeTheme)}
                className={cn(
                  "flex items-start justify-between gap-3 rounded-lg border bg-card p-3.5 text-left transition-colors focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                  active
                    ? "border-(--info) ring-1 ring-(--info)"
                    : "border-border hover:border-input-hover",
                )}
              >
                <div className="grid gap-0.5">
                  <span className="text-sm font-medium text-foreground">
                    {choice.label}
                  </span>
                  <span className="text-xs text-muted-foreground">
                    {choice.description}
                  </span>
                </div>
                {active && (
                  <CheckIcon
                    className="mt-0.5 size-4 shrink-0 text-(--info)"
                    aria-hidden="true"
                  />
                )}
              </button>
            );
          })}
        </div>
      </div>
    </section>
  );
}

export function NotificationSettingsPanel({ csrf }: { csrf: string }) {
  return (
    <section className="grid min-w-0 gap-6">
      <div className="border-b pb-4">
        <h1 className="mb-3 text-2xl font-semibold tracking-tight">
          Notifications
        </h1>
        <p className="m-0 text-sm text-muted-foreground">
          Choose how you receive email updates and manage watched repositories
          and discussion subscriptions.
        </p>
      </div>

      <div className="grid gap-3 rounded-lg border border-border bg-card p-4">
        <h2 className="m-0 text-base font-semibold">Email notifications</h2>
        <p className="m-0 text-sm text-muted-foreground">
          Receive email about inbox activity, organization invitations, and
          personal repository transfer offers.
        </p>
        {csrf ? (
          <EmailSettings csrf={csrf} />
        ) : (
          <p className="m-0 text-sm text-muted-foreground">
            Sign in with an account to configure email notifications.
          </p>
        )}
      </div>

      <div className="grid gap-3 rounded-lg border border-border bg-card p-4">
        <h2 className="m-0 text-base font-semibold">Subscriptions</h2>
        <div className="grid gap-2 sm:grid-cols-3">
          <Button asChild variant="outline" className="justify-start">
            <Link href="/-/notifications">
              <BellIcon data-icon="inline-start" aria-hidden="true" />
              Notifications inbox
            </Link>
          </Button>
          <Button asChild variant="outline" className="justify-start">
            <Link href="/-/notifications/watched">
              <EyeIcon data-icon="inline-start" aria-hidden="true" />
              Watched repositories
            </Link>
          </Button>
          <Button asChild variant="outline" className="justify-start">
            <Link href="/-/notifications/subscriptions">
              <RepoIcon size={15} aria-hidden="true" />
              Thread subscriptions
            </Link>
          </Button>
        </div>
      </div>
    </section>
  );
}

interface GitToken {
  username: string;
  token: string;
  expires_in: number;
  owner: string;
  repository: string;
  access: "read" | "write";
}

export function GitAccessSettingsPanel({ session }: { session: Session }) {
  const catalog = useRequest<{ repositories: Repository[] }>(
    session.user ? "/api/repos?limit=50" : null,
  );
  const repositories = catalog.data?.repositories ?? [];
  const [token, setToken] = useState<GitToken>();
  const [selected, setSelected] = useState("");
  const [access, setAccess] = useState<"read" | "write">("read");
  const [closeRelatedIssues, setCloseRelatedIssues] = useState(false);
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  const [revealed, setRevealed] = useState(false);

  const credentialKey =
    `credential.${window.location.origin}.useHttpPath`.replaceAll("'", "'\\''");
  const repository = repositories.find(
    (repo) => `${repo.owner}/${repo.name}` === selected,
  );
  const requestedAccess =
    repository?.access === "write" && !repository.archived ? access : "read";
  const canCloseRelatedIssues =
    requestedAccess === "write" && !!repository?.organization_id;

  async function updateToken(method: "POST" | "DELETE") {
    if (method === "POST" && !repository) return;
    setPending(true);
    setToken(undefined);
    setRevealed(false);
    setMessage(undefined);
    try {
      const response = await fetch("/api/git-token", {
        method,
        headers: {
          "X-CSRF-Token": session.csrf ?? "",
          "Content-Type": "application/json",
        },
        body:
          method === "POST" && repository
            ? JSON.stringify({
                owner: repository.owner,
                repository: repository.name,
                access: requestedAccess,
                ...(canCloseRelatedIssues && closeRelatedIssues
                  ? { close_related_issues: true }
                  : {}),
              })
            : undefined,
      });
      if (!response.ok) {
        throw new Error("Could not update Git access. Reload and try again.");
      }
      setToken(
        method === "POST" ? ((await response.json()) as GitToken) : undefined,
      );
      if (method === "DELETE") {
        setMessage("Tokens from this sign-in have been revoked.");
      }
    } catch (error) {
      setMessage(
        error instanceof Error ? error.message : "Could not update Git access",
      );
    } finally {
      setPending(false);
    }
  }

  return (
    <section className="grid min-w-0 gap-6">
      <div className="border-b pb-4">
        <h1 className="mb-3 text-2xl font-semibold tracking-tight">
          Git access
        </h1>
        <p className="m-0 text-sm text-muted-foreground">
          Generate short-lived, repository-scoped HTTPS Git tokens for cloning
          and pushing from your terminal.
        </p>
      </div>

      {!session.user ? (
        <p className="m-0 rounded-lg border border-border bg-card p-4 text-sm text-muted-foreground">
          Local sessions do not issue HTTP Git access tokens.
        </p>
      ) : (
        <div className="grid gap-4 rounded-lg border border-border bg-card p-4">
          <div className="flex items-center gap-2">
            <KeyIcon size={16} className="text-muted-foreground" />
            <h2 className="m-0 text-base font-semibold">
              Repository Git token
            </h2>
          </div>
          <p className="m-0 text-sm text-muted-foreground">
            Use <code>crab</code> as your Git username and the generated token
            as the password. Tokens expire when this sign-in expires or when you
            sign out.
          </p>

          <Disclosure
            className="credential-setup"
            summary={<>Credential manager setup</>}
          >
            <p className="m-0 mb-2 text-xs text-muted-foreground">
              Run once so Git stores per-repository tokens separately for this
              server:
            </p>
            <code>git config --global &apos;{credentialKey}&apos; true</code>
          </Disclosure>

          <div className="grid gap-3 sm:grid-cols-2">
            <div className="grid gap-1.5 text-sm font-medium">
              <label htmlFor="settings-git-token-repository">Repository</label>
              <FieldSelect
                id="settings-git-token-repository"
                label="Repository"
                value={selected}
                onValueChange={(value) => {
                  setSelected(value);
                  setToken(undefined);
                  setAccess("read");
                  setCloseRelatedIssues(false);
                }}
                options={[
                  { value: "", label: "Select a repository" },
                  ...repositories.map((repo) => ({
                    value: `${repo.owner}/${repo.name}`,
                    label: `${repo.owner}/${repo.name}`,
                  })),
                ]}
              />
            </div>
            <div className="grid gap-1.5 text-sm font-medium">
              <label htmlFor="git-token-access">Access</label>
              <FieldSelect
                id="git-token-access"
                label="Access"
                value={requestedAccess}
                disabled={!repository}
                onValueChange={(value) => {
                  setAccess(value === "write" ? "write" : "read");
                  setToken(undefined);
                  setCloseRelatedIssues(false);
                }}
                options={[
                  { value: "read", label: "Read (clone and fetch)" },
                  ...(repository?.access === "write" && !repository.archived
                    ? [{ value: "write", label: "Write (push and clone)" }]
                    : []),
                ]}
              />
            </div>
          </div>

          {canCloseRelatedIssues && (
            <label className="flex items-center gap-2 text-sm">
              <Checkbox
                checked={closeRelatedIssues}
                onCheckedChange={(checked) => {
                  setCloseRelatedIssues(checked === true);
                  setToken(undefined);
                }}
              />
              Allow commit messages to close issues in related repositories
            </label>
          )}

          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              disabled={!repository || pending}
              onClick={() => void updateToken("POST")}
            >
              Generate token
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={pending}
              onClick={() => void updateToken("DELETE")}
            >
              Revoke tokens
            </Button>
          </div>

          {token && (
            <div className="grid gap-2 rounded-md border border-border bg-muted/40 p-3">
              <label
                htmlFor="settings-generated-token"
                className="text-xs font-medium text-muted-foreground"
              >
                {token.access === "write" ? "Write" : "Read"} token for{" "}
                {token.owner}/{token.repository} (expires in{" "}
                {Math.round(token.expires_in / 60)} min)
              </label>
              <div className="flex flex-wrap gap-2">
                <Input
                  id="settings-generated-token"
                  type={revealed ? "text" : "password"}
                  readOnly
                  value={token.token}
                  className="min-w-0 flex-1 font-mono text-xs"
                />
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setRevealed((prev) => !prev)}
                >
                  {revealed ? "Hide" : "Reveal"}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() =>
                    void navigator.clipboard?.writeText(token.token)
                  }
                >
                  Copy
                </Button>
              </div>
            </div>
          )}

          {message && (
            <p role="status" className="m-0 text-sm text-muted-foreground">
              {message}
            </p>
          )}
        </div>
      )}
    </section>
  );
}

export function OrganizationsSettingsPanel() {
  return (
    <section className="grid min-w-0 gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-4">
        <div>
          <h1 className="mb-3 text-2xl font-semibold tracking-tight">
            Organizations
          </h1>
          <p className="m-0 text-sm text-muted-foreground">
            Manage your organization memberships and pending invitations.
          </p>
        </div>
        <NewOrganizationButton />
      </div>
      <OrganizationDirectory title={false} />
    </section>
  );
}

export function RepositoriesSettingsPanel({ url }: { url: URL }) {
  return (
    <section className="grid min-w-0 gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-4">
        <div>
          <h1 className="mb-3 text-2xl font-semibold tracking-tight">
            Repositories
          </h1>
          <p className="m-0 text-sm text-muted-foreground">
            Repositories owned by your personal account.
          </p>
        </div>
        <Button asChild variant="outline" size="sm">
          <Link href="/-/repositories">All accessible repositories</Link>
        </Button>
      </div>
      <BrowseRepositories url={url} scope={{ kind: "personal" }} />
    </section>
  );
}
