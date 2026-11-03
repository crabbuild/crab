// Card lint engine.
//
// Evaluates declarative lint rules against parsed model/dataset card
// frontmatter fields and body content. Rules are loaded from
// `assets/card-lint-rules.json` and can be filtered by card type
// (model vs dataset) via the `appliesTo` field.

import rulesJson from "../assets/card-lint-rules.json";

// --- Types ---

export type CardType = "model" | "dataset";

export type LintSeverity = "warning" | "error";

export type LintCheckType = "required" | "min_length";

export interface LintRule {
  id: string;
  field: string;
  check: LintCheckType;
  message: string;
  severity: LintSeverity;
  /** When set, the rule only applies to cards of these types. */
  appliesTo?: CardType[];
  /** For `min_length` checks, the minimum character count. */
  value?: number;
}

export interface LintRulesFile {
  rules: LintRule[];
}

export interface LintWarning {
  ruleId: string;
  field: string;
  message: string;
  severity: LintSeverity;
}

// --- Rules loading ---

/** Default lint rules shipped with the app. */
export const defaultLintRules: LintRulesFile = rulesJson as LintRulesFile;

// --- Card type inference ---

/**
 * Infer the card type from the file path. Files containing "model"
 * in the basename (case-insensitive) are treated as model cards;
 * files containing "dataset" are dataset cards. Falls back to
 * "model" when ambiguous (most HF repos use model cards).
 */
export function inferCardType(path: string): CardType {
  const slash = path.lastIndexOf("/");
  const name = (slash === -1 ? path : path.slice(slash + 1)).toLowerCase();
  if (name.includes("dataset")) return "dataset";
  return "model";
}

// --- Lint evaluation ---

/**
 * Run lint rules against the given card fields and body.
 *
 * @param fields - Parsed YAML frontmatter key-value pairs.
 * @param body - The markdown body content of the card.
 * @param cardType - The type of card being linted.
 * @param rules - The lint rules to evaluate (defaults to shipped rules).
 * @returns An array of lint warnings/errors for rules that failed.
 */
export function lintCard(
  fields: Record<string, string>,
  body: string,
  cardType: CardType,
  rules: LintRule[] = defaultLintRules.rules,
): LintWarning[] {
  const warnings: LintWarning[] = [];

  for (const rule of rules) {
    // Skip rules that don't apply to this card type
    if (rule.appliesTo && !rule.appliesTo.includes(cardType)) {
      continue;
    }

    const failed = evaluateRule(rule, fields, body);
    if (failed) {
      warnings.push({
        ruleId: rule.id,
        field: rule.field,
        message: rule.message,
        severity: rule.severity,
      });
    }
  }

  return warnings;
}

/**
 * Evaluate a single rule. Returns true if the rule is violated.
 */
function evaluateRule(
  rule: LintRule,
  fields: Record<string, string>,
  body: string,
): boolean {
  const value = rule.field === "__body__" ? body : (fields[rule.field] ?? "");

  switch (rule.check) {
    case "required":
      return value.trim().length === 0;

    case "min_length":
      return value.trim().length < (rule.value ?? 0);

    default:
      return false;
  }
}
