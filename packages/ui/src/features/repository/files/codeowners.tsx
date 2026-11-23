import { ShieldLockIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

interface FileOwnership {
  commit: string;
  path_hex: string;
  source: {
    path: string;
    path_hex: string;
    line: number | null;
    owners: { reference: string; eligible: boolean }[];
    problem: string | null;
  } | null;
}

export function FileCodeOwners({
  repo,
  commit,
  path,
}: {
  repo: Repository;
  commit: string;
  path: string;
}) {
  const { data } = useRequest<FileOwnership>(
    endpoint(repo, "file-codeowners", { rev: commit, path_hex: path }),
  );
  if (!data?.source || data.commit !== commit || data.path_hex !== path)
    return null;
  const { source } = data;
  const href = repoHref(repo, {
    rev: commit,
    path: source.path_hex,
    kind: "Blob",
  });
  const owners = source.owners.filter((owner) => owner.eligible);
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          size="icon-sm"
          aria-label="Code owners"
          title="Code owners"
        >
          <ShieldLockIcon aria-hidden="true" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        aria-label="File code owners"
        className="w-80 max-w-[calc(100vw-2rem)] gap-2"
      >
        <PopoverHeader>
          <PopoverTitle>Code owners</PopoverTitle>
        </PopoverHeader>
        {source.problem ? (
          <p className="m-0 text-muted-foreground">{source.problem}</p>
        ) : owners.length ? (
          <ul
            aria-label="Eligible code owners"
            className="m-0 max-h-52 list-none space-y-1 overflow-y-auto p-0"
          >
            {owners.map((owner) => (
              <li key={owner.reference} className="wrap-anywhere">
                {owner.reference}
              </li>
            ))}
          </ul>
        ) : (
          <p className="m-0 text-muted-foreground">
            No listed owner currently has Write access.
          </p>
        )}
        {owners.length > 0 && owners.length < source.owners.length && (
          <p className="m-0 text-muted-foreground">
            Some listed owners could not be resolved or lack Write access.
          </p>
        )}
        <Link
          href={source.line === null ? href : `${href}#L${source.line}`}
          className="w-fit wrap-anywhere underline"
        >
          {source.line === null
            ? source.path
            : `${source.path}, line ${source.line}`}
        </Link>
      </PopoverContent>
    </Popover>
  );
}
