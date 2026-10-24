import type { ReactNode } from "react";
import { useRequest } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

// Selection writes use compare-and-set versions, not durable submission IDs.
// A failed response must be reviewed before a new version can replace data.
export function SelectionReview<T>({
  url,
  label,
  canManage,
  children,
  onResolve,
}: {
  url: string;
  label: string;
  canManage: (saved: T) => boolean;
  children: (saved: T) => ReactNode;
  onResolve: (saved: T, choice: "draft" | "saved") => void;
}) {
  const latest = useRequest<T>(url);
  return (
    <div role="region" aria-label={label} className="p-3">
      <Alert>
        <AlertTitle>Review the saved selection</AlertTitle>
        <AlertDescription>
          Your selection is still in this editor. Review what is saved before
          continuing. Keeping your selection does not save it.
        </AlertDescription>
      </Alert>
      <Result state={latest}>
        {(saved) => (
          <div className="flex flex-col gap-3 pt-3">
            {children(saved)}
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                variant="outline"
                disabled={!canManage(saved)}
                onClick={() => onResolve(saved, "draft")}
              >
                Keep my selection
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => onResolve(saved, "saved")}
              >
                Use saved selection
              </Button>
            </div>
            {!canManage(saved) && (
              <p className="text-muted-foreground">
                You no longer have permission to change this selection.
              </p>
            )}
          </div>
        )}
      </Result>
    </div>
  );
}
