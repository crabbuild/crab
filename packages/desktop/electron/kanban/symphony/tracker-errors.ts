import type { SymphonyTrackerKind } from "./types";

export type SymphonyTrackerErrorCode =
  | "auth_error"
  | "rate_limit"
  | "http_error"
  | "graphql_error"
  | "invalid_json"
  | "invalid_response"
  | "transport_error"
  | "not_authenticated"
  | "unsupported_tracker";

export interface SymphonyTrackerErrorOptions {
  tracker?: SymphonyTrackerKind;
  status?: number;
  cause?: unknown;
}

export class SymphonyTrackerError extends Error {
  readonly code: SymphonyTrackerErrorCode;
  readonly tracker?: SymphonyTrackerKind;
  readonly status?: number;
  override readonly cause?: unknown;

  constructor(code: SymphonyTrackerErrorCode, message: string, options: SymphonyTrackerErrorOptions = {}) {
    super(message);
    this.name = "SymphonyTrackerError";
    this.code = code;
    this.tracker = options.tracker;
    this.status = options.status;
    this.cause = options.cause;
  }
}
