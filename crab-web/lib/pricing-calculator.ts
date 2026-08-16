/**
 * Pure cost-calculator helpers used by the pricing page.
 *
 * Kept side-effect-free so they can be exercised by property-based tests
 * (see task 17 in the spec) without a React or DOM environment.
 */

import type { StorageClass } from "@/lib/pricing-data";

export interface CalculatorInputs {
  /** Total repository size in GB. Permitted range: [0.1, 100000]. */
  repositorySizeGb: number;
  /** Monthly push (PUT-class) operations. Permitted range: [1, 10000]. */
  monthlyPushes: number;
  /** Monthly pull (GET-class) operations. Permitted range: [1, 10000]. */
  monthlyPulls: number;
  /** Provider id (e.g. "aws-s3"). */
  provider: string;
  /** Region id within the provider (e.g. "us-east-1"). */
  region: string;
  /** Storage class id within the region (e.g. "standard"). */
  storageClass: string;
}

/**
 * Subset of {@link StorageClass} containing only the rate fields the formula
 * needs. Letting callers pass this directly keeps the function easy to test
 * without constructing a full provider/region/class tree.
 */
export interface StorageClassRates {
  storageCostPerGbMonth: number;
  putCostPer1kOps: number;
  getCostPer1kOps: number;
  egressCostPerGb: number;
}

/**
 * Egress amortization factor. Each pull does not download the full repository;
 * lazy checkout + chunk dedup mean only a fraction of the repo egresses on a
 * typical pull. We model that fraction as `1 / PULL_NORMALIZATION`, so
 * `pulls × repo_size × egress_per_gb / 100` ≈ "pulls × 1% of repo egressed".
 */
export const PULL_NORMALIZATION = 100;

export const CALCULATOR_RANGES = {
  repositorySizeGb: { min: 0.1, max: 100_000 },
  monthlyPushes: { min: 1, max: 10_000 },
  monthlyPulls: { min: 1, max: 10_000 },
} as const;

/**
 * Compute the estimated monthly cost for the given inputs and rates.
 *
 * Formula (matches Requirement 8.3):
 *   cost = (size × storageCostPerGbMonth)
 *        + (pushes × putCostPer1kOps / 1000)
 *        + (pulls  × getCostPer1kOps / 1000)
 *        + (pulls  × size × egressCostPerGb / PULL_NORMALIZATION)
 *
 * Returns USD as a plain `number`. Callers should format the value for
 * display (e.g. `cost.toFixed(2)`).
 */
export function calculateMonthlyCost(
  inputs: CalculatorInputs,
  rates: StorageClassRates,
): number {
  const { repositorySizeGb, monthlyPushes, monthlyPulls } = inputs;
  const storage = repositorySizeGb * rates.storageCostPerGbMonth;
  const puts = (monthlyPushes * rates.putCostPer1kOps) / 1000;
  const gets = (monthlyPulls * rates.getCostPer1kOps) / 1000;
  const egress =
    (monthlyPulls * repositorySizeGb * rates.egressCostPerGb) /
    PULL_NORMALIZATION;
  return storage + puts + gets + egress;
}

export interface ValidationResult {
  valid: boolean;
  error?: string;
}

const NUMERIC_FIELDS = new Set<keyof CalculatorInputs>([
  "repositorySizeGb",
  "monthlyPushes",
  "monthlyPulls",
]);

/**
 * Validate a single calculator input. Returns `{ valid: true }` on success
 * or `{ valid: false, error }` with a user-facing message describing the
 * accepted range or format.
 *
 * Non-numeric fields (provider, region, storageClass) accept any non-empty
 * string — the calculator UI constrains these via select boxes, so we only
 * guard against empty selections here.
 */
export function validateCalculatorInput(
  field: keyof CalculatorInputs,
  value: number | string,
): ValidationResult {
  if (NUMERIC_FIELDS.has(field)) {
    const numeric = typeof value === "number" ? value : Number(value);
    if (
      typeof value === "string" &&
      (value.trim() === "" || Number.isNaN(numeric))
    ) {
      return {
        valid: false,
        error: `${humanFieldName(field)} must be a number.`,
      };
    }
    if (!Number.isFinite(numeric)) {
      return {
        valid: false,
        error: `${humanFieldName(field)} must be a finite number.`,
      };
    }
    const range =
      CALCULATOR_RANGES[field as keyof typeof CALCULATOR_RANGES];
    if (numeric < range.min || numeric > range.max) {
      return {
        valid: false,
        error: `${humanFieldName(field)} must be between ${range.min} and ${range.max}.`,
      };
    }
    return { valid: true };
  }

  // Categorical fields: provider, region, storageClass.
  if (typeof value !== "string" || value.trim() === "") {
    return {
      valid: false,
      error: `${humanFieldName(field)} is required.`,
    };
  }
  return { valid: true };
}

function humanFieldName(field: keyof CalculatorInputs): string {
  switch (field) {
    case "repositorySizeGb":
      return "Repository size (GB)";
    case "monthlyPushes":
      return "Monthly pushes";
    case "monthlyPulls":
      return "Monthly pulls";
    case "provider":
      return "Provider";
    case "region":
      return "Region";
    case "storageClass":
      return "Storage class";
  }
}
