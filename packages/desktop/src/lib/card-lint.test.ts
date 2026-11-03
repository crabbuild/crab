// Tests for the card lint engine.
//
// Verifies:
//   1. inferCardType correctly identifies model vs dataset cards.
//   2. lintCard flags missing required fields.
//   3. lintCard respects appliesTo filter.
//   4. lintCard evaluates min_length checks.
//   5. lintCard returns correct warning structure.
//
// Run via: npx tsx src/lib/card-lint.test.ts

import { inferCardType, lintCard, LintRule } from "./card-lint";

/* eslint-disable no-console */

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(
      `FAIL: ${msg}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`,
    );
    process.exit(1);
  }
}

// ---------- inferCardType ----------

{
  assertEq(inferCardType("MODEL_CARD.md"), "model", "MODEL_CARD.md → model");
  assertEq(inferCardType("DATASET_CARD.md"), "dataset", "DATASET_CARD.md → dataset");
  assertEq(inferCardType("README.md"), "model", "README.md → model (default)");
  assertEq(inferCardType("some/path/dataset_card.md"), "dataset", "nested dataset path");
  assertEq(inferCardType("models/model-card.md"), "model", "nested model path");
}

console.log("inferCardType: OK");

// ---------- lintCard: no warnings when all fields present ----------

{
  const rules: LintRule[] = [
    { id: "license-missing", field: "license", check: "required", message: "License is not specified", severity: "warning" },
    { id: "model-size-missing", field: "model_size", check: "required", message: "Model size is not declared", severity: "warning", appliesTo: ["model"] },
    { id: "description-too-short", field: "__body__", check: "min_length", value: 50, message: "Card description is too short", severity: "warning" },
  ];

  const fields = { license: "MIT", model_size: "7B" };
  const body = "A".repeat(50);
  const warnings = lintCard(fields, body, "model", rules);
  assertEq(warnings.length, 0, "no warnings when all fields present");
}

// ---------- lintCard: flags missing required fields ----------

{
  const rules: LintRule[] = [
    { id: "license-missing", field: "license", check: "required", message: "License is not specified", severity: "warning" },
    { id: "model-size-missing", field: "model_size", check: "required", message: "Model size is not declared", severity: "warning", appliesTo: ["model"] },
    { id: "description-too-short", field: "__body__", check: "min_length", value: 50, message: "Card description is too short", severity: "warning" },
  ];

  const fields = {};
  const body = "A".repeat(50);
  const warnings = lintCard(fields, body, "model", rules);
  assertEq(warnings.length, 2, "flags 2 missing required fields");
  const ids = warnings.map((w) => w.ruleId);
  assertEq(ids.includes("license-missing"), true, "includes license-missing");
  assertEq(ids.includes("model-size-missing"), true, "includes model-size-missing");
}

// ---------- lintCard: empty/whitespace treated as missing ----------

{
  const rules: LintRule[] = [
    { id: "license-missing", field: "license", check: "required", message: "License is not specified", severity: "warning" },
    { id: "model-size-missing", field: "model_size", check: "required", message: "Model size is not declared", severity: "warning", appliesTo: ["model"] },
  ];

  const fields = { license: "", model_size: "  " };
  const body = "";
  const warnings = lintCard(fields, body, "model", rules);
  assertEq(warnings.length, 2, "empty/whitespace treated as missing");
}

// ---------- lintCard: appliesTo filter skips model-only rules for dataset ----------

{
  const rules: LintRule[] = [
    { id: "license-missing", field: "license", check: "required", message: "License is not specified", severity: "warning" },
    { id: "model-size-missing", field: "model_size", check: "required", message: "Model size is not declared", severity: "warning", appliesTo: ["model"] },
  ];

  const fields = {};
  const body = "";
  const warnings = lintCard(fields, body, "dataset", rules);
  assertEq(warnings.length, 1, "dataset card skips model-only rules");
  assertEq(warnings[0].ruleId, "license-missing", "only license-missing for dataset");
}

// ---------- lintCard: min_length check ----------

{
  const rules: LintRule[] = [
    { id: "description-too-short", field: "__body__", check: "min_length", value: 50, message: "Card description is too short", severity: "warning" },
  ];

  const shortBody = "Short";
  const warnings = lintCard({}, shortBody, "model", rules);
  assertEq(warnings.length, 1, "short body triggers min_length");
  assertEq(warnings[0].ruleId, "description-too-short", "correct rule id for short body");

  const longBody = "A".repeat(50);
  const noWarnings = lintCard({}, longBody, "model", rules);
  assertEq(noWarnings.length, 0, "body at threshold passes min_length");
}

// ---------- lintCard: warning structure ----------

{
  const rules: LintRule[] = [
    { id: "license-missing", field: "license", check: "required", message: "License is not specified", severity: "warning" },
  ];

  const warnings = lintCard({}, "", "model", rules);
  assertEq(warnings.length, 1, "one warning for missing license");
  assertEq(warnings[0].ruleId, "license-missing", "ruleId correct");
  assertEq(warnings[0].field, "license", "field correct");
  assertEq(warnings[0].message, "License is not specified", "message correct");
  assertEq(warnings[0].severity, "warning", "severity correct");
}

console.log("lintCard: OK");
console.log("card-lint: ALL TESTS PASSED");
