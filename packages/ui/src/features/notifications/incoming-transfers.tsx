import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { offerHref } from "@/features/settings/transfer";
import { useRequest } from "@/lib/api";

type Address = { owner: string; name: string };
type Offer = { id: string; from: Address; to: Address; expires_at_ms: number };
type Page = { items: Offer[]; next: string | null };

const address = ({ owner, name }: Address) => `${owner}/${name}`;

export function transfersHref(before?: string) {
  const parameters = new URLSearchParams({ tab: "transfers" });
  if (before) parameters.set("before", before);
  return `/-/notifications?${parameters}`;
}

export function IncomingTransfers({ before }: { before: string | null }) {
  const path = "/api/repository-transfers/incoming";
  const request = useRequest<Page>(
    before ? `${path}?${new URLSearchParams({ before })}` : path,
  );
  return (
    <section aria-labelledby="incoming-transfers" className="grid gap-3">
      <h2 id="incoming-transfers" className="font-semibold">
        Incoming repository transfers
      </h2>
      <Result state={request} timingLabel="Incoming repository transfers">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul className="divide-y rounded-lg border">
                {items.map((offer) => (
                  <li
                    key={offer.id}
                    className="grid gap-2 p-4 sm:grid-cols-[1fr_auto]"
                  >
                    <div className="min-w-0">
                      <p className="break-all font-medium">
                        {address(offer.from)}
                      </p>
                      <p className="break-all text-sm text-muted-foreground">
                        Destination {address(offer.to)}
                      </p>
                      <p className="text-sm text-muted-foreground">
                        Expires <RelativeTime value={offer.expires_at_ms} />
                      </p>
                    </div>
                    <Button variant="outline" size="sm" asChild>
                      <Link href={offerHref(offer.id)}>
                        Review transfer
                        <span className="sr-only">
                          {" "}
                          of {address(offer.from)}
                        </span>
                      </Link>
                    </Button>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="muted">No pending repository transfers.</p>
            )}
            {(before || next) && (
              <nav className="flex gap-2" aria-label="Transfer pages">
                {before && (
                  <Button variant="outline" asChild>
                    <Link href={transfersHref()}>Newest</Link>
                  </Button>
                )}
                {next && (
                  <Button variant="outline" asChild>
                    <Link href={transfersHref(next)}>Older</Link>
                  </Button>
                )}
              </nav>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
