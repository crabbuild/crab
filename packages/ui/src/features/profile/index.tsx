import { useState } from "react";
import { CircleUserRoundIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { timestamp } from "@/components/discussion/time";
import { useRequest } from "@/lib/api";
import { ProfileForm } from "./form";
import type { ActorProfile, OwnProfile } from "./model";

export function ProfileSettings({
  csrf,
  suggestedName,
}: {
  csrf: string;
  suggestedName: string;
}) {
  const own = useRequest<OwnProfile>("/api/profile");
  const [saved, setSaved] = useState<ActorProfile>();
  return (
    <div className="mx-auto grid w-full max-w-5xl gap-8 p-4 sm:grid-cols-[200px_minmax(0,1fr)] sm:p-8">
      <aside aria-label="Account settings">
        <Link
          href="/-/profile"
          aria-current="page"
          className="flex items-center gap-2 rounded-md bg-muted px-3 py-2 text-sm font-medium"
        >
          <CircleUserRoundIcon size={16} />
          Public profile
        </Link>
      </aside>
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
    </div>
  );
}

export function UserProfile({ id }: { id: string }) {
  const actor = useRequest<ActorProfile>(
    `/api/actors/${encodeURIComponent(id)}`,
  );
  return (
    <section className="mx-auto w-full max-w-3xl p-4 sm:p-8">
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
