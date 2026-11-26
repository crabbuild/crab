import {
  GlobeIcon,
  HomeIcon,
  LockIcon,
  PinIcon,
  RepoIcon,
  SearchIcon,
} from "@primer/octicons-react";
import {
  ArrowLeftRightIcon,
  Building2Icon,
  CheckIcon,
  UserRoundIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { UserAvatar } from "@/components/ui/user-avatar";
import type { AvatarConfig } from "@/lib/contracts";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { Spinner } from "@/components/ui/spinner";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import type { Repository } from "@/lib/api";
import { organizationHref } from "@/lib/organization-route";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";

export type HomeTab = "home" | "feed";

export type HomeContext =
  | { kind: "personal" }
  | { kind: "organization"; entry: OrganizationMembershipEntry };

const PIN_TIP_KEY = "crab-home-pin-tip-dismissed";
const INITIAL_REPO_COUNT = 10;

export function HomeSidebar({
  accountName,
  accountHandle,
  accountAvatar,
  activeTab,
  pinnedTab,
  onSelectTab,
  onPinTab,
  context,
  onSelectContext,
  organizations,
  repositories,
  repositoriesLoading,
  repositoriesError,
  hasMoreRepositories,
  onRetryRepositories,
  onNewRepository,
}: {
  accountName: string;
  accountHandle?: string;
  accountAvatar?: AvatarConfig | null;
  activeTab: HomeTab;
  pinnedTab: HomeTab;
  onSelectTab: (tab: HomeTab) => void;
  onPinTab: (tab: HomeTab) => void;
  context: HomeContext;
  onSelectContext: (next: HomeContext) => void;
  organizations: OrganizationMembershipEntry[];
  repositories: Repository[];
  repositoriesLoading: boolean;
  repositoriesError?: string;
  hasMoreRepositories: boolean;
  onRetryRepositories: () => void;
  onNewRepository: () => void;
}) {
  const [tipDismissed, setTipDismissed] = useState(() => {
    try {
      return localStorage.getItem(PIN_TIP_KEY) === "true";
    } catch {
      return false;
    }
  });
  const [searchOpen, setSearchOpen] = useState(false);
  const [filterText, setFilterText] = useState("");
  const [showAll, setShowAll] = useState(false);
  const filterInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (searchOpen) filterInputRef.current?.focus();
  }, [searchOpen]);

  const dismissTip = () => {
    setTipDismissed(true);
    try {
      localStorage.setItem(PIN_TIP_KEY, "true");
    } catch {
      // Ignore storage failures.
    }
  };

  const query = filterText.trim().toLowerCase();
  const filteredRepositories = query
    ? repositories.filter((repo) =>
        `${repo.owner}/${repo.name}`.toLowerCase().includes(query),
      )
    : repositories;
  const visibleRepositories =
    showAll || query
      ? filteredRepositories
      : filteredRepositories.slice(0, INITIAL_REPO_COUNT);
  const canShowMore =
    !query &&
    ((!showAll && filteredRepositories.length > INITIAL_REPO_COUNT) ||
      hasMoreRepositories);

  const primaryLabel =
    context.kind === "organization"
      ? context.entry.organization.name
      : accountHandle || accountName;
  const secondaryLabel =
    context.kind === "organization"
      ? `Organization · ${context.entry.role === "owner" ? "Owner" : "Member"}`
      : "Your personal account";

  return (
    <aside className="home-sidebar" aria-label="Dashboard sidebar">
      <div className="p-3">
        <DropdownMenu modal={false}>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              className="flex w-full items-center gap-2.5 rounded-lg px-2 py-2 text-left transition-colors hover:bg-accent/70 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
              aria-label="Switch dashboard context"
            >
              {context.kind === "organization" ? (
                <Avatar className="size-8 shrink-0 border border-border">
                  <AvatarFallback className="text-xs font-semibold">
                    <Building2Icon className="size-4" aria-hidden="true" />
                  </AvatarFallback>
                </Avatar>
              ) : (
                <UserAvatar
                  name={primaryLabel}
                  avatar={accountAvatar}
                  size={32}
                />
              )}
              <div className="min-w-0 flex-1">
                <span className="block truncate text-sm font-semibold text-foreground">
                  {primaryLabel}
                </span>
                <span className="block truncate text-xs text-muted-foreground">
                  {secondaryLabel}
                </span>
              </div>
              <ArrowLeftRightIcon
                className="size-3.5 shrink-0 text-muted-foreground"
                aria-hidden="true"
              />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-64">
            <DropdownMenuLabel>Switch dashboard context</DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onSelect={() => onSelectContext({ kind: "personal" })}
              className="justify-between"
            >
              <span className="flex min-w-0 items-center gap-2">
                <UserRoundIcon className="size-4 shrink-0" aria-hidden="true" />
                <span className="truncate">{accountHandle || accountName}</span>
              </span>
              {context.kind === "personal" && (
                <CheckIcon className="size-3.5 shrink-0" aria-hidden="true" />
              )}
            </DropdownMenuItem>
            {organizations.length > 0 && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuLabel>Organizations</DropdownMenuLabel>
                {organizations.map((entry) => {
                  const selected =
                    context.kind === "organization" &&
                    context.entry.organization.id === entry.organization.id;
                  return (
                    <DropdownMenuItem
                      key={entry.organization.id}
                      onSelect={() =>
                        onSelectContext({ kind: "organization", entry })
                      }
                      className="justify-between"
                    >
                      <span className="flex min-w-0 items-center gap-2">
                        <Building2Icon
                          className="size-4 shrink-0"
                          aria-hidden="true"
                        />
                        <span className="truncate">
                          {entry.organization.name}
                        </span>
                      </span>
                      {selected && (
                        <CheckIcon
                          className="size-3.5 shrink-0"
                          aria-hidden="true"
                        />
                      )}
                    </DropdownMenuItem>
                  );
                })}
              </>
            )}
            <DropdownMenuSeparator />
            <DropdownMenuItem asChild>
              <Link href="/-/organizations">
                <Building2Icon aria-hidden="true" />
                Manage organizations
              </Link>
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>

        {context.kind === "organization" && (
          <div className="mt-1 px-2">
            <Link
              href={organizationHref(context.entry.organization)}
              className="text-xs text-(--info) hover:underline"
            >
              Open {context.entry.organization.name} page →
            </Link>
          </div>
        )}

        <nav aria-label="Dashboard views" className="mt-2 grid gap-0.5">
          {(
            [
              { id: "home", label: "Home", Icon: HomeIcon },
              { id: "feed", label: "Feed", Icon: GlobeIcon },
            ] as const
          ).map(({ id, label, Icon }) => {
            const active = activeTab === id;
            const pinned = pinnedTab === id;
            return (
              <div
                key={id}
                className={cn(
                  "group relative flex items-center rounded-md transition-colors",
                  active
                    ? "bg-accent/80 font-medium text-foreground"
                    : "text-muted-foreground hover:bg-accent/40 hover:text-foreground",
                )}
              >
                {active && (
                  <span
                    className="absolute top-1.5 bottom-1.5 left-0 w-0.5 rounded-full bg-(--info)"
                    aria-hidden="true"
                  />
                )}
                <button
                  type="button"
                  onClick={() => onSelectTab(id)}
                  aria-current={active ? "page" : undefined}
                  className="flex min-w-0 flex-1 items-center gap-2.5 px-2.5 py-1.5 text-left text-sm focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
                >
                  <Icon size={16} aria-hidden="true" />
                  <span>{label}</span>
                </button>
                <button
                  type="button"
                  onClick={() => onPinTab(id)}
                  aria-label={
                    pinned ? `${label} is pinned as homepage` : `Pin ${label}`
                  }
                  aria-pressed={pinned}
                  title={
                    pinned
                      ? `${label} is pinned as your default view`
                      : `Pin ${label} as default`
                  }
                  className={cn(
                    "mr-1.5 inline-flex size-6 shrink-0 items-center justify-center rounded transition-colors hover:bg-background/70 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                    pinned
                      ? "text-(--info)"
                      : "text-muted-foreground/70 hover:text-foreground",
                  )}
                >
                  <PinIcon size={14} aria-hidden="true" />
                </button>
              </div>
            );
          })}
        </nav>

        {!tipDismissed && (
          <div className="mt-3 rounded-lg border border-border bg-background/80 p-3 text-xs shadow-xs">
            <div className="flex items-center justify-between gap-2">
              <strong className="font-semibold text-foreground">
                Pin your homepage
              </strong>
              <span className="rounded-full border border-(--info)/40 bg-(--info)/10 px-1.5 py-0.5 text-[10px] font-medium text-(--info)">
                New
              </span>
            </div>
            <p className="m-0 mt-1.5 leading-relaxed text-muted-foreground">
              Choose whether Crab opens to Home or Feed. Select the pin next to
              either one.
            </p>
            <Button
              type="button"
              variant="outline"
              size="xs"
              className="mt-2.5"
              onClick={dismissTip}
            >
              Got it
            </Button>
          </div>
        )}
      </div>

      <Separator />

      <div className="p-3">
        <div className="mb-2 flex items-center justify-between gap-2 px-1">
          <h2 className="m-0 text-xs font-semibold tracking-tight text-foreground">
            Top repositories
          </h2>
          <div className="flex items-center gap-1">
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              aria-label={
                searchOpen
                  ? "Close repository filter"
                  : "Filter top repositories"
              }
              aria-expanded={searchOpen}
              onClick={() => {
                setSearchOpen((prev) => {
                  if (prev) setFilterText("");
                  return !prev;
                });
              }}
              className="text-muted-foreground hover:text-foreground"
            >
              {searchOpen ? (
                <XIcon className="size-3.5" aria-hidden="true" />
              ) : (
                <SearchIcon size={14} aria-hidden="true" />
              )}
            </Button>
            <Button
              type="button"
              size="xs"
              variant="outline"
              onClick={onNewRepository}
              className="h-6 gap-1 border-(--success)/40 bg-(--success)/10 px-2 text-xs font-medium text-(--success) hover:bg-(--success)/20"
            >
              <RepoIcon size={12} aria-hidden="true" />
              New
            </Button>
          </div>
        </div>

        {searchOpen && (
          <div className="mb-2">
            <Input
              ref={filterInputRef}
              type="search"
              value={filterText}
              onChange={(event) => setFilterText(event.target.value)}
              placeholder="Find a repository…"
              aria-label="Find a repository"
              className="h-7 bg-background text-xs"
            />
          </div>
        )}

        {repositoriesLoading && repositories.length === 0 ? (
          <div
            role="status"
            className="flex items-center gap-2 px-2 py-3 text-xs text-muted-foreground"
          >
            <Spinner /> Loading repositories…
          </div>
        ) : repositoriesError ? (
          <div role="alert" className="grid gap-2 px-2 py-2 text-xs">
            <span className="text-muted-foreground">{repositoriesError}</span>
            <Button
              variant="outline"
              size="xs"
              className="w-fit"
              onClick={onRetryRepositories}
            >
              Retry
            </Button>
          </div>
        ) : visibleRepositories.length > 0 ? (
          <>
            <ul
              className="m-0 grid list-none gap-0.5 p-0"
              aria-label="Top repositories"
            >
              {visibleRepositories.map((repo) => {
                const ownerInitial = Array.from(repo.owner)[0]?.toUpperCase();
                return (
                  <li key={repo.id}>
                    <Link
                      href={repoHref(repo)}
                      className="flex items-center gap-2 rounded-md px-1.5 py-1.5 text-sm text-foreground transition-colors hover:bg-accent/60 hover:no-underline"
                    >
                      <span
                        className="grid size-4.5 shrink-0 place-items-center rounded-full border border-border bg-muted text-[10px] font-semibold text-muted-foreground"
                        aria-hidden="true"
                      >
                        {ownerInitial || <RepoIcon size={10} />}
                      </span>
                      <span className="min-w-0 flex-1 truncate text-[13px] font-medium">
                        {repo.owner}/{repo.name}
                      </span>
                      {repo.visibility === "private" && (
                        <LockIcon
                          size={12}
                          className="shrink-0 text-muted-foreground/70"
                          aria-label="Private"
                        />
                      )}
                    </Link>
                  </li>
                );
              })}
            </ul>
            <div className="mt-2 flex items-center justify-between gap-2 px-1.5">
              {canShowMore ? (
                !showAll && filteredRepositories.length > INITIAL_REPO_COUNT ? (
                  <button
                    type="button"
                    onClick={() => setShowAll(true)}
                    className="text-xs text-muted-foreground hover:text-foreground hover:underline"
                  >
                    Show more
                  </button>
                ) : (
                  <Link
                    href="/-/repositories"
                    className="text-xs text-muted-foreground hover:text-foreground hover:underline"
                  >
                    Show more
                  </Link>
                )
              ) : (
                <Link
                  href="/-/repositories"
                  className="text-xs text-muted-foreground hover:text-foreground hover:underline"
                >
                  Browse all repositories
                </Link>
              )}
            </div>
          </>
        ) : (
          <p className="m-0 px-2 py-3 text-xs text-muted-foreground">
            {query
              ? "No repositories match your filter."
              : "No repositories available yet."}
          </p>
        )}
      </div>
    </aside>
  );
}
