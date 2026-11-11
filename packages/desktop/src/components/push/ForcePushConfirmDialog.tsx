import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useState } from "react";
import { AlertOctagon, Loader2 } from "lucide-react";

interface Props {
  open: boolean;
  /** Local branch being force-pushed. */
  branch: string;
  /** Destination remote name + URL — surfaced verbatim so the user
   *  can spot a wrong-bucket force-push before it ships. */
  remoteName: string;
  remoteUrl: string;
  /** Branch name on the remote that will be overwritten. */
  destinationBranch: string;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

/**
 * Force-push confirm sheet. Force pushing to a crab remote rewrites
 * a multi-GB store's ref history — a slip costs hours of recovery,
 * so we make the user retype the destination remote name to confirm.
 *
 * Echoing the full `crab://` URL back is the single most useful thing
 * here: most force-push mistakes happen when someone has multiple
 * environments configured and the muscle memory points at prod.
 */
export function ForcePushConfirmDialog({
  open,
  branch,
  remoteName,
  remoteUrl,
  destinationBranch,
  busy,
  onCancel,
  onConfirm,
}: Props) {
  const [typed, setTyped] = useState("");
  if (!open) return null;
  const matches = typed.trim() === remoteName;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      onClick={onCancel}
    >
      <div
        className="w-[min(540px,92vw)] rounded-lg border border-destructive/40 bg-card p-5 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center gap-2 text-base font-semibold text-destructive">
          <AlertOctagon size={16} />
          Force push?
        </div>
        <p className="mb-3 text-sm text-muted-foreground">
          Force pushing rewrites history on the remote. Anyone else with this branch
          checked out will see their work diverge.
        </p>

        <div className="mb-4 space-y-1.5 rounded border border-border bg-background p-3 text-sm">
          <Row label="From">
            <code className="font-mono">{branch}</code>
          </Row>
          <Row label="To">
            <code className="font-mono">
              {remoteName} / {destinationBranch}
            </code>
          </Row>
          <Row label="Remote URL">
            <code className="break-all font-mono text-[12px]">{remoteUrl}</code>
          </Row>
        </div>

        <label className="mb-3 block text-xs text-muted-foreground">
          Type <code className="font-mono text-foreground">{remoteName}</code> to confirm:
          <Input
            type="text"
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            spellCheck={false}
            autoFocus
            className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 font-mono text-sm focus-visible:outline-none focus:border-destructive"
            placeholder={remoteName}
          />
        </label>

        <div className="flex justify-end gap-2">
          <Button variant="outline" type="button" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
          <Button variant="outline"
            type="button"
            className={
 " " +
 (matches
 ? "border-destructive/60 bg-destructive/10 text-destructive hover:bg-destructive/20"
 : "")
 }
            disabled={!matches || busy}
            onClick={onConfirm}
          >
            {busy ? (
              <>
                <Loader2 size={12} className="animate-spin" /> Force pushing…
              </>
            ) : (
              <>Force push to {remoteName}</>
            )}
          </Button>
        </div>
      </div>
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="w-[88px] shrink-0 text-[11px] uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <span className="flex-1 text-sm">{children}</span>
    </div>
  );
}
