import { Alert, AlertDescription } from "@/components/ui/alert";
export function MutationError({ error }: { error?: string }) {
  return error ? (
    <Alert variant="destructive">
      <AlertDescription>{error}</AlertDescription>
    </Alert>
  ) : null;
}
