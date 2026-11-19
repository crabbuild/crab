import { endpoint, request, type Repository } from "@/lib/api";
import type {
  ReportDraft,
  ResultsRead,
  ResultsResponse,
} from "@/lib/results-contracts";
export interface ResultsRepository extends Repository {
  csrf: string;
}
export function readResults(
  repo: Pick<Repository, "owner" | "name">,
  input: ResultsRead,
) {
  const controller = new AbortController();
  const url = endpoint(repo, "results", { request: JSON.stringify(input) });
  return {
    promise: request<ResultsResponse>(url, controller.signal).then(
      (result) => result.data,
    ),
    cancel: () => controller.abort(),
  };
}
export async function saveResultsReport(
  repo: ResultsRepository,
  draft: ReportDraft,
): Promise<{ id: string }> {
  const response = await fetch(endpoint(repo, "results/reports"), {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-CSRF-Token": repo.csrf,
      "X-Crab-Repository-Id": repo.id,
    },
    body: JSON.stringify(draft),
  });
  if (!response.ok)
    throw new Error(
      `Report publication failed (${response.status}); retry the same comparison`,
    );
  return response.json() as Promise<{ id: string }>;
}
