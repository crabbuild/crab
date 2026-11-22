import { useState } from "react";
import { CircleUserRoundIcon, MessageSquareTextIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { timestamp } from "@/components/discussion/time";
import { useRequest } from "@/lib/api";
import { cn } from "@/lib/utils";
import { ProfileForm } from "./form";
import { SavedReplies } from "./saved-replies";
import type { ActorProfile, OwnProfile } from "@/lib/contracts";

export function ProfileSettings({
  csrf,
  suggestedName,
  section = "profile",
}: {
  csrf: string;
  suggestedName: string;
  section?: "profile" | "saved-replies";
}) {
  const settings = [
    {
      id: "profile",
      href: "/-/profile",
      label: "Public profile",
      Icon: CircleUserRoundIcon,
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

export function UserProfile({ id }: { id: string }) {
  const actor = useRequest<ActorProfile>(
    `/api/actors/${encodeURIComponent(id)}`,
  );
  return (
    <section className="account-profile">
      <Result state={actor} timingLabel="Actor profile">
        {(profile) => (
          <article className="flex min-w-0 items-start gap-5 rounded-xl border p-6">
            <CircleUserRoundIcon
              className="size-16 shrink-0 text-muted-foreground"
              aria-hidden="true"
            />
            <div className="min-w-0 break-words">
              <h1 className="m-0 text-2xl font-semibold tracking-tight">
                {profile.name}
              </h1>
              <p className="mt-1 text-lg text-muted-foreground">
                @{profile.handle}
              </p>
              <p className="mt-4 text-sm text-muted-foreground">
                Joined {timestamp(profile.created_at_ms)}
              </p>
            </div>
          </article>
        )}
      </Result>
    </section>
  );
}
