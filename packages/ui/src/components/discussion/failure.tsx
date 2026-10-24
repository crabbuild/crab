import { Alert, AlertDescription } from "@/components/ui/alert";

export function Failure({ message }: { message?: string }) {
  return message ? (
    <Alert variant="destructive">
      <AlertDescription>
        {message} Your draft is still in this form.
      </AlertDescription>
    </Alert>
  ) : null;
}
