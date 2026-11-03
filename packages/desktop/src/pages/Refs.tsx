import { useEffect, useState } from "react";
import { GitBranch, GitCommit, Tag } from "lucide-react";
import { OpenRepo } from "../state";
import { RefEntry, rpc } from "../ipc";

export function RefsPage({ repo }: { repo: OpenRepo }) {
  const [refs, setRefs] = useState<RefEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setRefs(null);
    setError(null);
    rpc
      .listRefs(repo.repo_id)
      .then((r) => setRefs(r.refs))
      .catch((e: Error) => setError(e.message));
  }, [repo.repo_id]);

  if (error) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-sm text-gx-error">
        {error}
      </div>
    );
  }

  if (!refs) return <div className="p-4 text-sm text-gx-muted">Loading refs…</div>;

  const branches = refs.filter((r) => r.kind === "branch");
  const tags = refs.filter((r) => r.kind === "tag");
  const others = refs.filter((r) => r.kind !== "branch" && r.kind !== "tag");

  return (
    <div className="h-full overflow-auto p-4">
      <Section icon={<GitBranch size={14} />} title="Branches" entries={branches} />
      <Section icon={<Tag size={14} />} title="Tags" entries={tags} />
      <Section icon={<GitCommit size={14} />} title="Other" entries={others} />
    </div>
  );
}

function Section({ icon, title, entries }: { icon: React.ReactNode; title: string; entries: RefEntry[] }) {
  if (entries.length === 0) return null;
  return (
    <div className="mb-6">
      <h3 className="mb-2 flex items-center gap-2 text-xs uppercase tracking-wider text-gx-muted">
        {icon} {title} <span className="text-gx-muted">({entries.length})</span>
      </h3>
      <ul className="rounded border border-gx-border">
        {entries.map((r, i) => (
          <li
            key={r.name}
            className={`flex items-center justify-between gap-3 px-3 py-2 text-sm ${i > 0 ? "border-t border-gx-border" : ""}`}
          >
            <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{r.name}</span>
            <span className="shrink-0 font-mono text-[11px] text-gx-muted">
              {r.target.slice(0, 12)}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}
