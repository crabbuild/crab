// Invariants for the dataset-kind classifier. We check:
//
//   1. Glob matching: **, *, ? patterns work correctly.
//   2. classifyFile respects minBytes thresholds.
//   3. classifyFile respects pointerOnly flag.
//   4. First-match-wins ordering.
//   5. Unmatched files return "other".
//   6. Null size handled gracefully with minBytes rules.
//
// Pattern matches existing test files: plain node script with
// assert helpers; run via `scripts/dataset-kind-check.mjs`.

import {
  classifyFile,
  globMatch,
  defaultRules,
  mergeRules,
  type DatasetRule,
  type DatasetRulesFile,
  type ClassifyInput,
} from "./dataset-kind";

/* eslint-disable no-console */

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(
      `FAIL: ${msg}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`,
    );
    process.exit(1);
  }
}

// ---------- globMatch: ** recursive ----------

{
  assertEq(globMatch("**/*.py", "src/train/model.py"), true, "** matches nested dirs");
  assertEq(globMatch("**/*.py", "model.py"), true, "** matches zero dirs");
  assertEq(globMatch("**/train/**", "data/train/file.csv"), true, "** matches dir in middle");
  assertEq(globMatch("**/train/**", "train/file.csv"), true, "** matches at start");
  assertEq(globMatch("**/train/**", "train/sub/deep.csv"), true, "** matches deep nesting");
}

// ---------- globMatch: * single segment ----------

{
  assertEq(globMatch("*.py", "model.py"), true, "* matches within segment");
  assertEq(globMatch("*.py", "src/model.py"), false, "* does not cross /");
  assertEq(globMatch("**/*.py", "src/model.py"), true, "**/* crosses /");
}

// ---------- globMatch: ? single character ----------

{
  assertEq(
    globMatch("model-?????-of-?????.safetensors", "model-00001-of-00015.safetensors"),
    true,
    "? matches single char",
  );
  assertEq(
    globMatch("model-?????-of-?????.safetensors", "model-1-of-15.safetensors"),
    false,
    "? requires exact count",
  );
}

// ---------- globMatch: case insensitive ----------

{
  assertEq(globMatch("**/*.PY", "src/Model.py"), true, "case insensitive ext");
  assertEq(globMatch("**/README.md", "docs/readme.MD"), true, "case insensitive name");
}

// ---------- globMatch: backslash normalisation ----------

{
  assertEq(globMatch("**/*.py", "src\\train\\model.py"), true, "backslash normalised");
}

// ---------- globMatch: non-matches ----------

{
  assertEq(globMatch("**/*.py", "model.rs"), false, "wrong extension");
  assertEq(globMatch("**/train/**", "eval/file.csv"), false, "wrong directory");
}

// ---------- classifyFile: checkpoint with minBytes ----------

{
  const rules = defaultRules.rules;
  const large: ClassifyInput = { path: "models/model.safetensors", size: 2_000_000, pointerKind: null };
  assertEq(classifyFile(large, rules), "checkpoint", "large safetensors → checkpoint");

  const small: ClassifyInput = { path: "models/tiny.safetensors", size: 500, pointerKind: null };
  assertEq(classifyFile(small, rules), "other", "small safetensors below minBytes → other");
}

// ---------- classifyFile: sharded checkpoint (no minBytes) ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "model-00001-of-00015.safetensors", size: 100, pointerKind: null };
  assertEq(classifyFile(input, rules), "checkpoint", "sharded pattern matches without size");
}

// ---------- classifyFile: tokenizer ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "tokenizer.json", size: 5000, pointerKind: null };
  assertEq(classifyFile(input, rules), "tokenizer", "tokenizer.json → tokenizer");
}

// ---------- classifyFile: notebook ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "experiments/train.ipynb", size: 8000, pointerKind: null };
  assertEq(classifyFile(input, rules), "notebook", ".ipynb → notebook");
}

// ---------- classifyFile: training-data by directory ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "data/train/shard-001.parquet", size: 500, pointerKind: null };
  assertEq(classifyFile(input, rules), "training-data", "train/ dir → training-data");
}

// ---------- classifyFile: training-data by size ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "data/shard.parquet", size: 50_000_000, pointerKind: null };
  assertEq(classifyFile(input, rules), "training-data", "large parquet → training-data");
}

// ---------- classifyFile: eval-data ----------

{
  const rules = defaultRules.rules;
  assertEq(
    classifyFile({ path: "eval/metrics.csv", size: 1000, pointerKind: null }, rules),
    "eval-data",
    "eval/ → eval-data",
  );
  assertEq(
    classifyFile({ path: "test/samples.jsonl", size: 500, pointerKind: null }, rules),
    "eval-data",
    "test/ → eval-data",
  );
}

// ---------- classifyFile: config ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "config.json", size: 200, pointerKind: null };
  assertEq(classifyFile(input, rules), "config", "config.json → config");
}

// ---------- classifyFile: code ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "src/model.py", size: 3000, pointerKind: null };
  assertEq(classifyFile(input, rules), "code", ".py → code");
}

// ---------- classifyFile: other (unmatched) ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "README.md", size: 500, pointerKind: null };
  assertEq(classifyFile(input, rules), "other", "unmatched → other");
}

// ---------- classifyFile: pointerOnly flag ----------

{
  const pointerRule: DatasetRule[] = [
    { kind: "checkpoint", globs: ["**/*.bin"], pointerOnly: true },
  ];
  const nonPointer: ClassifyInput = { path: "model.bin", size: 5_000_000, pointerKind: null };
  const pointer: ClassifyInput = { path: "model.bin", size: 5_000_000, pointerKind: "crab" };

  assertEq(classifyFile(nonPointer, pointerRule), "other", "pointerOnly skips non-pointer");
  assertEq(classifyFile(pointer, pointerRule), "checkpoint", "pointerOnly matches pointer");
}

// ---------- classifyFile: first match wins ----------

{
  const rules = defaultRules.rules;
  // A .py file under train/ — the train/ directory rule comes first
  const input: ClassifyInput = { path: "train/preprocess.py", size: 2000, pointerKind: null };
  assertEq(classifyFile(input, rules), "training-data", "first match wins over later code rule");
}

// ---------- classifyFile: null size with minBytes ----------

{
  const rules = defaultRules.rules;
  const input: ClassifyInput = { path: "data/big.parquet", size: null, pointerKind: null };
  const result = classifyFile(input, rules);
  // minBytes rules won't match null size, so it won't be training-data
  // (the size-gated parquet rule). It should fall through.
  assertEq(result !== "training-data" || result === "training-data", true, "null size doesn't crash");
  // More specifically: data/big.parquet doesn't match train/ dir,
  // the size-gated parquet rule skips it, eval/ doesn't match,
  // config doesn't match, code doesn't match → other
  assertEq(result, "other", "null size with minBytes rule → falls through to other");
}

console.log("dataset-kind: OK");

// ---------- mergeRules: prepend mode (default) ----------

{
  const defaults: DatasetRule[] = [
    { kind: "code", globs: ["**/*.py"] },
    { kind: "config", globs: ["**/config.json"] },
  ];
  const overrides: DatasetRulesFile = {
    rules: [{ kind: "training-data", globs: ["**/*.custom"] }],
  };
  const merged = mergeRules(defaults, overrides);
  assertEq(merged.length, 3, "prepend: total rules = overrides + defaults");
  assertEq(merged[0].kind, "training-data", "prepend: workspace rule comes first");
  assertEq(merged[1].kind, "code", "prepend: default rules follow");
  assertEq(merged[2].kind, "config", "prepend: all defaults preserved");
}

// ---------- mergeRules: workspace rules take priority ----------

{
  const defaults: DatasetRule[] = [
    { kind: "code", globs: ["**/*.py"] },
  ];
  const overrides: DatasetRulesFile = {
    rules: [{ kind: "notebook", globs: ["**/*.py"] }],
  };
  const merged = mergeRules(defaults, overrides);
  const input: ClassifyInput = { path: "train.py", size: 100, pointerKind: null };
  assertEq(classifyFile(input, merged), "notebook", "workspace override takes priority over default");
}

// ---------- mergeRules: replace mode ----------

{
  const defaults: DatasetRule[] = [
    { kind: "code", globs: ["**/*.py"] },
    { kind: "config", globs: ["**/config.json"] },
  ];
  const overrides: DatasetRulesFile = {
    rules: [{ kind: "checkpoint", globs: ["**/*.bin"] }],
    replace: true,
  };
  const merged = mergeRules(defaults, overrides);
  assertEq(merged.length, 1, "replace: only workspace rules present");
  assertEq(merged[0].kind, "checkpoint", "replace: workspace rule is the only one");
}

// ---------- mergeRules: empty overrides (no-op) ----------

{
  const defaults: DatasetRule[] = [
    { kind: "code", globs: ["**/*.py"] },
  ];
  const overrides: DatasetRulesFile = { rules: [] };
  const merged = mergeRules(defaults, overrides);
  assertEq(merged.length, 1, "empty overrides: defaults unchanged");
  assertEq(merged[0].kind, "code", "empty overrides: default rule preserved");
}

// ---------- mergeRules: replace with empty replaces everything ----------

{
  const defaults: DatasetRule[] = [
    { kind: "code", globs: ["**/*.py"] },
  ];
  const overrides: DatasetRulesFile = { rules: [], replace: true };
  const merged = mergeRules(defaults, overrides);
  assertEq(merged.length, 0, "replace with empty: no rules");
}

console.log("mergeRules: OK");
