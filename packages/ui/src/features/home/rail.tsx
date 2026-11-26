import { ArrowRightIcon, Building2Icon, GlobeIcon } from "lucide-react";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Badge } from "@/components/ui/badge";
import { Spinner } from "@/components/ui/spinner";
import { reasons, type Reason } from "@/features/notifications/filters";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import { useRequest } from "@/lib/api";
import { organizationHref } from "@/lib/organization-route";
import { cn } from "@/lib/utils";

type RailNotice = {
  id: string;
  repository: { id: string; owner: string; name: string };
  title: string;
  href: string;
  sender: string;
  reason: Reason;
  read: boolean;
  updated: { id: string; created_at_ms: number };
};

type RailInboxPage = {
  items: RailNotice[];
  next: string | null;
};

export function HomeRail({
  organizations,
}: {
  organizations: OrganizationMembershipEntry[];
}) {
  const notifications = useRequest<RailInboxPage>(
    "/api/notifications?unread=false&limit=5",
  );
  const items = notifications.data?.items ?? [];

  return (
    <aside className="home-rail" aria-label="Latest updates and organizations">
      <section
        aria-labelledby="home-updates-heading"
        className="rounded-lg border border-border bg-card p-4"
      >
        <h2
          id="home-updates-heading"
          className="m-0 text-sm font-semibold text-foreground"
        >
          Latest updates
        </h2>

        {notifications.loading && !notifications.data ? (
          <div
            role="status"
            className="mt-3 flex items-center gap-2 text-xs text-muted-foreground"
          >
            <Spinner /> Checking notifications…
          </div>
        ) : notifications.error ? (
          <p className="m-0 mt-3 text-xs text-muted-foreground">
            Notifications are unavailable right now.
          </p>
        ) : items.length > 0 ? (
          <ol className="relative m-0 mt-4 ml-1.5 list-none space-y-4 border-l border-border p-0 pl-4">
            {items.map((notice) => {
              const reasonLabel =
                reasons[notice.reason]?.summary ?? "Notification";
              return (
                <li key={notice.id} className="relative">
                  <span
                    aria-hidden="true"
                    className={cn(
                      "absolute top-1.5 -left-[20.5px] size-2 rounded-full ring-2 ring-card",
                      notice.read ? "bg-muted-foreground/45" : "bg-(--info)",
                    )}
                  />
                  <p className="m-0 text-[11px] text-muted-foreground">
                    <RelativeTime value={notice.updated.created_at_ms} /> ·{" "}
                    {reasonLabel}
                  </p>
                  <Link
                    href={notice.href}
                    className="mt-0.5 block text-xs leading-snug font-medium text-foreground hover:text-(--info) hover:underline"
                  >
                    {notice.title}
                  </Link>
                  <p className="m-0 mt-0.5 truncate text-[11px] text-muted-foreground">
                    {notice.repository.owner}/{notice.repository.name}
                  </p>
                </li>
              );
            })}
          </ol>
        ) : (
          <p className="m-0 mt-3 text-xs leading-relaxed text-muted-foreground">
            You&apos;re all caught up. Mentions, review requests, and watched
            repository updates appear here.
          </p>
        )}

        <div className="mt-4 pt-1">
          <Link
            href="/-/notifications"
            className="inline-flex min-h-6 items-center gap-1 text-xs font-medium text-muted-foreground hover:text-(--info) hover:underline"
          >
            <span>View notifications</span>
            <ArrowRightIcon className="size-3" aria-hidden="true" />
          </Link>
        </div>
      </section>

      <section
        aria-labelledby="home-explore-heading"
        className="rounded-lg border border-border bg-card p-4"
      >
        <h2
          id="home-explore-heading"
          className="m-0 text-sm font-semibold text-foreground"
        >
          {organizations.length > 0 ? "Your organizations" : "Explore Crab"}
        </h2>

        {organizations.length > 0 ? (
          <ul className="m-0 mt-3 grid list-none gap-2 p-0">
            {organizations.slice(0, 5).map((entry) => (
              <li
                key={entry.organization.id}
                className="flex items-center justify-between gap-2"
              >
                <Link
                  href={organizationHref(entry.organization)}
                  className="flex min-w-0 items-center gap-2 text-xs font-medium text-foreground hover:text-(--info) hover:underline"
                >
                  <Building2Icon
                    className="size-3.5 shrink-0 text-muted-foreground"
                    aria-hidden="true"
                  />
                  <span className="truncate">{entry.organization.name}</span>
                </Link>
                <Badge
                  variant="outline"
                  className="shrink-0 px-1.5 py-0 text-[10px]"
                >
                  {entry.role === "owner" ? "Owner" : "Member"}
                </Badge>
              </li>
            ))}
          </ul>
        ) : (
          <p className="m-0 mt-2 text-xs leading-relaxed text-muted-foreground">
            Browse accessible repositories, search code across default branches,
            or explore public repositories.
          </p>
        )}

        <div className="mt-3.5 flex flex-col gap-1 border-t border-border pt-3 text-xs">
          <Link
            href="/-/repositories"
            className="inline-flex min-h-6 items-center gap-1.5 text-muted-foreground hover:text-(--info) hover:underline"
          >
            <span>Browse all repositories</span>
            <ArrowRightIcon className="size-3" aria-hidden="true" />
          </Link>
          <Link
            href="/-/public"
            className="inline-flex min-h-6 items-center gap-1.5 text-muted-foreground hover:text-(--info) hover:underline"
          >
            <GlobeIcon className="size-3" aria-hidden="true" />
            <span>Public repositories</span>
          </Link>
        </div>
      </section>
    </aside>
  );
}
