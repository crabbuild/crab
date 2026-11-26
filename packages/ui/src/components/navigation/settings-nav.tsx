import type { ComponentType } from "react";
import { Link } from "@/components/navigation/link";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";

export type SettingsNavGroup<T extends string> = {
  label?: string;
  items: {
    id: T;
    href: string;
    label: string;
    Icon: ComponentType<{ className?: string; size?: number }>;
  }[];
};

// Settings and organization People pages share GitHub's grouped side navigation.
export function SettingsNav<T extends string>({
  label,
  groups,
  active,
}: {
  label: string;
  groups: SettingsNavGroup<T>[];
  active: T;
}) {
  return (
    <nav aria-label={label} className="account-settings-nav">
      {groups.map((group, index) => (
        <div key={group.label ?? "general"} className="grid gap-0.5">
          {index > 0 && <Separator className="my-2.5" />}
          {group.label && (
            <p className="m-0 px-2.5 py-1 text-xs font-semibold text-muted-foreground">
              {group.label}
            </p>
          )}
          {group.items.map(({ id, href, label, Icon }) => {
            const current = id === active;
            return (
              <Link
                key={id}
                href={href}
                aria-current={current ? "page" : undefined}
                className={cn(
                  "relative flex min-h-8 items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm transition-colors hover:bg-muted hover:no-underline",
                  current
                    ? "bg-muted font-semibold text-foreground"
                    : "text-foreground/85 hover:text-foreground",
                )}
              >
                {current && (
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
    </nav>
  );
}
