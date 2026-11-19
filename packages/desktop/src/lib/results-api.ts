import { call, callCancellable, type CancellableCall } from "../ipc";
import type {
  ArtifactPromotionRequest,
  ReportDraft,
  ResultsRead,
  ResultsResponse,
  RunSelection,
} from "./results-contracts";
import type { OpenRepo } from "../state";

type ResultsScope = Pick<OpenRepo, "repo_id" | "native_session_id">;

export function readResults(
  repo: ResultsScope,
  request: ResultsRead,
): CancellableCall<ResultsResponse> {
  return callCancellable("workflow_results_read", {
    repo_id: repo.repo_id,
    native_session_id: repo.native_session_id,
    request,
  });
}
export function saveResultsReport(
  repo: ResultsScope,
  draft: ReportDraft,
): Promise<{ id: string }> {
  return call("workflow_results_save_report", {
    repo_id: repo.repo_id,
    native_session_id: repo.native_session_id,
    draft,
  });
}
export function resultsAction<T = unknown>(
  repo: ResultsScope,
  action: string,
  run?: RunSelection,
  report_id?: string,
  branch?: string,
  promotion?: ArtifactPromotionRequest,
): Promise<T> {
  return call("workflow_results_action", {
    repo_id: repo.repo_id,
    native_session_id: repo.native_session_id,
    action,
    run: run ?? null,
    report_id: report_id ?? null,
    branch: branch ?? null,
    promotion: promotion ?? null,
  });
}
