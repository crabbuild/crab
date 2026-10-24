import { useRef, useState } from "react";
import { endpoint, type Repository } from "@/lib/api";
import { useMutation } from "../mutations";
import type { CreationSelection } from "./metadata";

interface Input {
  title: string;
  body: string;
  metadata: CreationSelection;
  draft?: boolean;
  base_ref?: string;
  head_ref?: string;
}
export function useCreateSubmission<T>({
  repo,
  kind,
  csrf,
  onCreated,
}: {
  repo: Repository;
  kind: "issues" | "pulls";
  csrf: string;
  onCreated: (saved: T) => void;
}) {
  const mutation = useMutation(csrf, repo);
  const [attempt, setAttempt] = useState<Input & { request_id: string }>();
  const [validation, setValidation] = useState<string>();
  const busy = useRef(false);
  async function submit(input: Input) {
    if (busy.current) return;
    const next = attempt ?? { ...input, request_id: crypto.randomUUID() };
    const bytes = new TextEncoder();
    const limit = kind === "issues" ? 80 * 1024 : 160 * 1024;
    if (
      !next.title.trim() ||
      /[\u0000-\u001f\u007f-\u009f]/.test(next.title) ||
      bytes.encode(next.body).length > 65536 ||
      next.body.includes("\0") ||
      bytes.encode(JSON.stringify(next)).length > limit
    ) {
      setValidation(
        "Check the title and description length before submitting.",
      );
      return;
    }
    setValidation(undefined);
    busy.current = true;
    setAttempt(next);
    try {
      const saved = await mutation.run<T>(endpoint(repo, kind), "POST", next);
      if (saved) onCreated(saved);
    } finally {
      busy.current = false;
    }
  }
  return {
    submit,
    pending: mutation.pending,
    locked: Boolean(attempt),
    error: validation ?? mutation.error,
    recovering: Boolean(attempt && mutation.error && !mutation.rejected),
    rejected: Boolean(attempt && mutation.rejected),
    review() {
      if (mutation.rejected) {
        setAttempt(undefined);
        mutation.reset();
      }
    },
  };
}
