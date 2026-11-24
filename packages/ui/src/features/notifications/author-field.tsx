import { useId, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { ActorSummary } from "@/lib/contracts";
import { useRequest } from "@/lib/api";

export function AuthorField({
  author,
  onChange,
}: {
  author?: string;
  onChange: (author?: string) => void;
}) {
  const id = useId();
  const [search, setSearch] = useState("");
  const prefix = search.trim().replace(/^@+/, "").toLowerCase();
  const valid = /^[a-z0-9-]{1,39}$/.test(prefix);
  const results = useRequest<{ items: ActorSummary[]; truncated: boolean }>(
    !author && valid
      ? `/api/notifications/authors?${new URLSearchParams({ prefix })}`
      : null,
  );
  const profile = useRequest<ActorSummary>(
    author && author !== "me"
      ? `/api/actors/${encodeURIComponent(author)}`
      : null,
  );
  const items = results.data?.items ?? [];

  function choose(value?: string) {
    setSearch("");
    onChange(value);
  }

  return (
    <div className="grid gap-1.5 text-sm sm:col-span-2">
      {author ? (
        <>
          <span className="font-medium">Author</span>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="min-w-0 break-words">
              {author === "me"
                ? "Me"
                : profile.data
                  ? `${profile.data.name} (@${profile.data.handle})`
                  : profile.error
                    ? "Unavailable author"
                    : "Loading author…"}
            </span>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => choose()}
            >
              Clear author
            </Button>
          </div>
        </>
      ) : (
        <>
          <label className="font-medium" htmlFor={id}>
            Author
          </label>
          <div className="flex flex-wrap items-center gap-2">
            <Input
              id={id}
              name="filter-author"
              className="min-w-0 flex-1"
              value={search}
              maxLength={40}
              placeholder="Search by username"
              autoComplete="off"
              onChange={(event) => setSearch(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter") return;
                event.preventDefault();
                const match = items.find((item) => item.handle === prefix);
                if (match) choose(match.id);
              }}
            />
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => choose("me")}
            >
              Me
            </Button>
          </div>
          {prefix && !valid && (
            <p className="text-muted-foreground">
              Use a username with letters, digits, or hyphens.
            </p>
          )}
          {results.error && (
            <div className="flex flex-wrap items-center gap-2">
              <p role="alert">Unable to search authors.</p>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={results.retry}
              >
                Retry
              </Button>
            </div>
          )}
          {valid && results.loading && (
            <p role="status" className="text-muted-foreground">
              Searching authors…
            </p>
          )}
          {results.data &&
            (items.length ? (
              <>
                <ul className="max-h-40 divide-y overflow-y-auto rounded-md border">
                  {items.map((item) => (
                    <li key={item.id}>
                      <Button
                        type="button"
                        variant="ghost"
                        className="h-auto w-full justify-start whitespace-normal break-words text-left"
                        onClick={() => choose(item.id)}
                      >
                        {item.name} (@{item.handle})
                      </Button>
                    </li>
                  ))}
                </ul>
                {results.data.truncated && (
                  <p className="text-muted-foreground">
                    Enter more of the username to narrow the results.
                  </p>
                )}
              </>
            ) : (
              <p className="text-muted-foreground">No authors match.</p>
            ))}
        </>
      )}
    </div>
  );
}
