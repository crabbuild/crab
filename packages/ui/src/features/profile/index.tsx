import { KeyIcon, RepoIcon } from "@primer/octicons-react";
import {
  ArrowLeftRightIcon,
  BellIcon,
  Building2Icon,
  CheckIcon,
  ChevronDownIcon,
  CircleUserRoundIcon,
  ClockIcon,
  ExternalLinkIcon,
  MessageSquareTextIcon,
  PaintbrushIcon,
  SettingsIcon,
  UserRoundIcon,
} from "lucide-react";
import { useState, type ComponentType } from "react";
import type { Theme } from "@/app/theme/control";
import { timestamp } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Separator } from "@/components/ui/separator";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import { useOrganizationDirectory } from "@/features/organizations/directory";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { useRequest } from "@/lib/api";
import type { CodeTheme } from "@/lib/code-theme";
import type { ActorProfile, OwnProfile, Session } from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { cn } from "@/lib/utils";
import { ProfileForm } from "./form";
import { ReviewAvailabilitySettings } from "./review-availability";
import { SavedReplies } from "./saved-replies";
import {
  AccountSettings,
  AppearanceSettings,
  GitAccessSettingsPanel,
  NotificationSettingsPanel,
  OrganizationsSettingsPanel,
  RepositoriesSettingsPanel,
} from "./settings-sections";

export type SettingsSection =
  | "profile"
  | "account"
  | "appearance"
  | "notifications"
  | "git-access"
  | "organizations"
  | "repositories"
  | "review-availability"
  | "saved-replies";

type NavItem = {
  id: SettingsSection;
  href: string;
  label: string;
  Icon: ComponentType<{ className?: string; size?: number }>;
};

type NavGroup = {
  label?: string;
  items: NavItem[];
};

export function ProfileSettings({
  session,
  csrf,
  suggestedName,
  section = "profile",
  url,
  theme,
  setTheme,
  codeTheme,
  setCodeTheme,
  signingOut,
  onSignOut,
}: {
  session: Session;
  csrf: string;
  suggestedName: string;
  section?: SettingsSection;
  url: URL;
  theme: Theme;
  setTheme: (theme: Theme) => void;
  codeTheme: CodeTheme;
  setCodeTheme: (theme: CodeTheme) => void;
  signingOut: boolean;
  onSignOut: () => void;
}) {
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [savedProfile, setSavedProfile] = useState<ActorProfile>();
  const own = useRequest<OwnProfile>(
    section === "profile" ? "/api/profile" : null,
  );
  // Only load organizations when the context switcher dropdown is opened so
  // initial settings page loads stay lean and focused on the active panel.
  const orgDirectory = useOrganizationDirectory(undefined, switcherOpen);
  const organizations = (orgDirectory.data?.items ?? []).filter(
    (entry): entry is OrganizationMembershipEntry => entry.kind === "member",
  );
  const activeProfile = savedProfile ?? own.data?.profile ?? undefined;

  const navGroups: NavGroup[] = [
    {
      items: [
        {
          id: "profile",
          href: "/-/profile",
          label: "Public profile",
          Icon: UserRoundIcon,
        },
        {
          id: "account",
          href: "/-/settings/account",
          label: "Account",
          Icon: SettingsIcon,
        },
        {
          id: "appearance",
          href: "/-/settings/appearance",
          label: "Appearance",
          Icon: PaintbrushIcon,
        },
        {
          id: "notifications",
          href: "/-/settings/notifications",
          label: "Notifications",
          Icon: BellIcon,
        },
      ],
    },
    {
      label: "Access",
      items: [
        ...(session.user
          ? [
              {
                id: "git-access" as const,
                href: "/-/settings/git-access",
                label: "Git access",
                Icon: KeyIcon,
              },
            ]
          : []),
        {
          id: "organizations",
          href: "/-/settings/organizations",
          label: "Organizations",
          Icon: Building2Icon,
        },
      ],
    },
    {
      label: "Code, planning, and automation",
      items: [
        {
          id: "repositories",
          href: "/-/settings/repositories",
          label: "Repositories",
          Icon: RepoIcon,
        },
        {
          id: "review-availability",
          href: "/-/settings/review-availability",
          label: "Review availability",
          Icon: ClockIcon,
        },
        {
          id: "saved-replies",
          href: "/-/settings/saved-replies",
          label: "Saved replies",
          Icon: MessageSquareTextIcon,
        },
      ],
    },
  ];

  const displayName = activeProfile?.name ?? (suggestedName || "Local session");
  const displayHandle = activeProfile?.handle;
  const initial = Array.from(displayName.trim())[0]?.toUpperCase();

  return (
    <div className="account-settings">
      <header className="account-settings-header">
        <div className="flex min-w-0 items-center gap-3.5">
          <Avatar className="size-12 shrink-0 border border-border">
            <AvatarFallback className="text-base font-semibold">
              {initial || <UserRoundIcon className="size-5" />}
            </AvatarFallback>
          </Avatar>
          <div className="min-w-0">
            <p className="m-0 flex flex-wrap items-baseline gap-x-1.5 text-lg leading-tight font-semibold text-foreground">
              <span className="truncate">{displayName}</span>
              {displayHandle && (
                <span className="text-base font-normal text-muted-foreground">
                  ({displayHandle})
                </span>
              )}
            </p>
            <p className="m-0 mt-0.5 text-xs text-muted-foreground">
              {session.user ? "Your personal account" : "Local session"}
            </p>
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <DropdownMenu
            modal={false}
            open={switcherOpen}
            onOpenChange={setSwitcherOpen}
          >
            <DropdownMenuTrigger asChild>
              <Button variant="outline" size="sm" className="gap-1.5">
                <ArrowLeftRightIcon className="size-3.5" aria-hidden="true" />
                <span>Switch settings context</span>
                <ChevronDownIcon
                  className="size-3.5 opacity-70"
                  aria-hidden="true"
                />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-64">
              <DropdownMenuLabel>Settings context</DropdownMenuLabel>
              <DropdownMenuSeparator />
              <DropdownMenuItem asChild className="justify-between">
                <Link href="/-/profile">
                  <span className="flex items-center gap-2">
                    <UserRoundIcon className="size-4" aria-hidden="true" />
                    <span>{displayHandle || displayName}</span>
                  </span>
                  <CheckIcon className="size-3.5" aria-hidden="true" />
                </Link>
              </DropdownMenuItem>
              {organizations.length > 0 && (
                <>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>Organizations</DropdownMenuLabel>
                  {organizations.map((entry) => (
                    <DropdownMenuItem key={entry.organization.id} asChild>
                      <Link
                        href={organizationHref(
                          entry.organization,
                          "?tab=settings",
                        )}
                      >
                        <Building2Icon className="size-4" aria-hidden="true" />
                        <span className="truncate">
                          {entry.organization.name}
                        </span>
                      </Link>
                    </DropdownMenuItem>
                  ))}
                </>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem asChild>
                <Link href="/-/organizations">
                  <Building2Icon className="size-4" aria-hidden="true" />
                  Manage organizations
                </Link>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>

          {activeProfile && (
            <Button asChild variant="outline" size="sm">
              <Link href={`/-/users/${encodeURIComponent(activeProfile.id)}`}>
                Go to your personal profile
                <ExternalLinkIcon data-icon="inline-end" aria-hidden="true" />
              </Link>
            </Button>
          )}
        </div>
      </header>

      <div className="account-settings-body">
        <aside aria-label="Account settings" className="account-settings-nav">
          {navGroups.map((group, groupIndex) => (
            <div key={group.label ?? "general"} className="grid gap-0.5">
              {groupIndex > 0 && <Separator className="my-2.5" />}
              {group.label && (
                <p className="m-0 px-2.5 py-1 text-xs font-semibold text-muted-foreground">
                  {group.label}
                </p>
              )}
              {group.items.map(({ id, href, label, Icon }) => {
                const active = id === section;
                return (
                  <Link
                    key={id}
                    href={href}
                    aria-current={active ? "page" : undefined}
                    className={cn(
                      "relative flex min-h-8 items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm transition-colors hover:bg-muted hover:no-underline",
                      active
                        ? "bg-muted font-semibold text-foreground"
                        : "text-foreground/85 hover:text-foreground",
                    )}
                  >
                    {active && (
                      <span
                        className="absolute top-1.5 bottom-1.5 left-0 w-0.5 rounded-full bg-(--info)"
                        aria-hidden="true"
                      />
                    )}
                    <Icon
                      size={16}
                      className="size-4 shrink-0 text-muted-foreground"
                    />
                    <span className="truncate">{label}</span>
                  </Link>
                );
              })}
            </div>
          ))}
        </aside>

        <div className="min-w-0">
          {section === "account" ? (
            <AccountSettings
              session={session}
              signingOut={signingOut}
              onSignOut={onSignOut}
            />
          ) : section === "appearance" ? (
            <AppearanceSettings
              theme={theme}
              setTheme={setTheme}
              codeTheme={codeTheme}
              setCodeTheme={setCodeTheme}
            />
          ) : section === "notifications" ? (
            <NotificationSettingsPanel csrf={csrf} />
          ) : section === "git-access" ? (
            <GitAccessSettingsPanel session={session} />
          ) : section === "organizations" ? (
            <OrganizationsSettingsPanel />
          ) : section === "repositories" ? (
            <RepositoriesSettingsPanel url={url} />
          ) : section === "saved-replies" ? (
            <SavedReplies csrf={csrf} />
          ) : section === "review-availability" ? (
            <ReviewAvailabilitySettings csrf={csrf} />
          ) : (
            <PublicProfileSettings
              own={own}
              csrf={csrf}
              suggestedName={suggestedName}
              saved={savedProfile}
              onSaved={setSavedProfile}
            />
          )}
        </div>
      </div>
    </div>
  );
}

function PublicProfileSettings({
  own,
  csrf,
  suggestedName,
  saved,
  onSaved,
}: {
  own: ReturnType<typeof useRequest<OwnProfile>>;
  csrf: string;
  suggestedName: string;
  saved?: ActorProfile;
  onSaved: (profile: ActorProfile) => void;
}) {
  const activeProfile = saved ?? own.data?.profile ?? null;
  const previewName = activeProfile?.name ?? (suggestedName || "Local session");
  const previewInitial = Array.from(previewName.trim())[0]?.toUpperCase();

  return (
    <section className="min-w-0">
      <h1 className="mb-3 text-2xl font-semibold tracking-tight">
        Public profile
      </h1>
      <p className="mb-6 border-b pb-6 text-sm text-muted-foreground">
        Your username and display name are visible to signed-in users of this
        server.
      </p>
      <div className="grid items-start gap-8 lg:grid-cols-[minmax(0,1fr)_220px]">
        <div className="min-w-0">
          <Result state={own} timingLabel="Your profile">
            {({ profile }) => (
              <ProfileForm
                profile={saved ?? profile}
                csrf={csrf}
                suggestedName={suggestedName}
                onSaved={onSaved}
              />
            )}
          </Result>
        </div>

        <div className="grid justify-items-start gap-3">
          <h2 className="m-0 text-sm font-semibold text-foreground">
            Profile picture
          </h2>
          <Avatar className="size-44 border border-border">
            <AvatarFallback className="text-5xl font-semibold text-muted-foreground">
              {previewInitial || <UserRoundIcon className="size-16" />}
            </AvatarFallback>
          </Avatar>
          <p className="m-0 text-xs leading-relaxed text-muted-foreground">
            Generated from your display name initial across issues, pull
            requests, and activity feeds.
          </p>
        </div>
      </div>
    </section>
  );
}

export function UserProfile({
  id,
  repositories,
  url,
}: {
  id: string;
  repositories: boolean;
  url: URL;
}) {
  const href = `/-/users/${encodeURIComponent(id)}`;
  const actor = useRequest<ActorProfile>(
    `/api/actors/${encodeURIComponent(id)}`,
  );
  return (
    <section className="account-profile">
      <Result state={actor} timingLabel="Actor profile">
        {(profile) => (
          <>
            <header className="mb-5 flex min-w-0 items-start gap-4">
              <CircleUserRoundIcon
                className="size-12 shrink-0 rounded-lg border p-2 text-muted-foreground"
                aria-hidden="true"
              />
              <div className="min-w-0 wrap-anywhere">
                <h1
                  dir="auto"
                  className="m-0 text-2xl font-semibold tracking-tight"
                >
                  {profile.name}
                </h1>
                <p className="mt-1 text-sm text-muted-foreground">
                  @{profile.handle}
                </p>
              </div>
            </header>
            <nav
              aria-label="User profile"
              className="mb-5 flex flex-wrap gap-2 border-b pb-3"
            >
              <Button asChild variant={repositories ? "ghost" : "secondary"}>
                <Link
                  href={href}
                  aria-current={repositories ? undefined : "page"}
                >
                  Overview
                </Link>
              </Button>
              <Button asChild variant={repositories ? "secondary" : "ghost"}>
                <Link
                  href={`${href}/repositories`}
                  aria-current={repositories ? "page" : undefined}
                >
                  Repositories
                </Link>
              </Button>
            </nav>
            {repositories ? (
              <BrowseRepositories
                url={url}
                scope={{ kind: "profile", actor: id }}
              />
            ) : (
              <p className="text-sm text-muted-foreground">
                Joined {timestamp(profile.created_at_ms)}
              </p>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
