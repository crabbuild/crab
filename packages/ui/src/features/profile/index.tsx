import { KeyIcon, RepoIcon } from "@primer/octicons-react";
import {
  ArrowLeftRightIcon,
  BellIcon,
  Building2Icon,
  CheckIcon,
  ChevronDownIcon,
  ClockIcon,
  ExternalLinkIcon,
  MessageSquareTextIcon,
  PaintbrushIcon,
  SettingsIcon,
  UserRoundIcon,
} from "lucide-react";
import { useState } from "react";
import type { Theme } from "@/app/theme/control";
import { timestamp } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import {
  SettingsNav,
  type SettingsNavGroup,
} from "@/components/navigation/settings-nav";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { OrganizationAvatar, UserAvatar } from "@/components/ui/user-avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import { useOrganizationDirectory } from "@/features/organizations/directory";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { useRequest } from "@/lib/api";
import type { CodeTheme } from "@/lib/code-theme";
import type {
  ActorProfile,
  AvatarConfig,
  OwnProfile,
  Session,
} from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { ProfileAvatarPicker } from "./avatar-picker";
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

export function ProfileSettings({
  session,
  avatar,
  onAvatarSaved,
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
  avatar?: AvatarConfig | null;
  onAvatarSaved?: (next: AvatarConfig | null) => void;
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
  const [savedAvatar, setSavedAvatar] = useState<
    AvatarConfig | null | undefined
  >(undefined);
  const own = useRequest<OwnProfile>(
    section === "profile" ? "/api/profile" : null,
  );
  // Only load organizations when the context switcher dropdown is opened so
  // initial settings page loads stay lean and focused on the active panel.
  const orgDirectory = useOrganizationDirectory(undefined, switcherOpen);
  // Only owners have organization settings to switch into.
  const organizations = (orgDirectory.data?.items ?? []).filter(
    (entry): entry is OrganizationMembershipEntry =>
      entry.kind === "member" && entry.role === "owner",
  );
  const activeProfile = savedProfile ?? own.data?.profile ?? undefined;
  const activeAvatar =
    savedAvatar !== undefined
      ? savedAvatar
      : (own.data?.avatar ?? activeProfile?.avatar ?? avatar ?? null);

  function handleAvatarSaved(next: AvatarConfig | null) {
    setSavedAvatar(next);
    onAvatarSaved?.(next);
  }

  const navGroups: SettingsNavGroup<SettingsSection>[] = [
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

  return (
    <div className="account-settings">
      <header className="account-settings-header">
        <div className="flex min-w-0 items-center gap-3.5">
          <UserAvatar
            name={displayHandle || displayName}
            avatar={activeAvatar}
            size={48}
          />
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
                        <OrganizationAvatar
                          id={entry.organization.id}
                          size={16}
                        />
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
        <SettingsNav
          label="Account settings"
          groups={navGroups}
          active={section}
        />

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
              avatar={activeAvatar}
              onSaved={setSavedProfile}
              onAvatarSaved={handleAvatarSaved}
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
  avatar,
  onSaved,
  onAvatarSaved,
}: {
  own: ReturnType<typeof useRequest<OwnProfile>>;
  csrf: string;
  suggestedName: string;
  saved?: ActorProfile;
  avatar?: AvatarConfig | null;
  onSaved: (profile: ActorProfile) => void;
  onAvatarSaved: (next: AvatarConfig | null) => void;
}) {
  const activeProfile = saved ?? own.data?.profile ?? null;
  const previewName =
    activeProfile?.handle ??
    activeProfile?.name ??
    (suggestedName || "Local session");

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

        <ProfileAvatarPicker
          fallbackName={previewName}
          avatar={avatar}
          csrf={csrf}
          onSaved={onAvatarSaved}
        />
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
            <header className="mb-5 flex min-w-0 items-center gap-4">
              <UserAvatar
                name={profile.handle || profile.name}
                avatar={profile.avatar}
                size={64}
                className="border-2 border-border shadow-xs"
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
