import { useState } from "react";
import {
  CircleUserRoundIcon,
  ClockIcon,
  MessageSquareTextIcon,
} from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { timestamp } from "@/components/discussion/time";
import { useRequest } from "@/lib/api";
import { cn } from "@/lib/utils";
import { ProfileForm } from "./form";
import { ReviewAvailabilitySettings } from "./review-availability";
import { SavedReplies } from "./saved-replies";
import type { ActorProfile, OwnProfile } from "@/lib/contracts";

export function ProfileSettings({
  csrf,
  suggestedName,
  section = "profile",
}: {
  csrf: string;
  suggestedName: string;
  section?: "profile" | "review-availability" | "saved-replies";
}) {
  const settings = [
    {
      id: "profile",
      href: "/-/profile",
      label: "Public profile",
      Icon: CircleUserRoundIcon,
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
  ];
  return (
    <div className="account-settings">
      <aside aria-label="Account settings">
        {settings.map(({ id, href, label, Icon }) => (
          <Link
            key={id}
            href={href}
            aria-current={id === section ? "page" : undefined}
            className={cn(
              "flex items-center gap-2 rounded-md px-3 py-2 text-sm font-medium hover:bg-muted",
              id === section && "bg-muted",
            )}
          >
            <Icon size={16} aria-hidden="true" />
            {label}
          </Link>
        ))}
      </aside>
      {section === "saved-replies" ? (
        <SavedReplies csrf={csrf} />
      ) : section === "review-availability" ? (
        <ReviewAvailabilitySettings csrf={csrf} />
      ) : (
        <PublicProfileSettings csrf={csrf} suggestedName={suggestedName} />
      )}
    </div>
  );
}

function PublicProfileSettings({
  csrf,
  suggestedName,
}: {
  csrf: string;
  suggestedName: string;
}) {
  const own = useRequest<OwnProfile>("/api/profile");
  const [saved, setSaved] = useState<ActorProfile>();
  return (
    <section className="min-w-0">
      <h1 className="mb-3 text-2xl font-semibold tracking-tight">
        Public profile
      </h1>
      <p className="mb-6 border-b pb-6 text-sm text-muted-foreground">
        Your username and display name are visible to signed-in users of this
        server.
      </p>
      <Result state={own} timingLabel="Your profile">
        {({ profile }) => (
          <ProfileForm
            profile={saved ?? profile}
            csrf={csrf}
            suggestedName={suggestedName}
            onSaved={setSaved}
          />
        )}
      </Result>
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
