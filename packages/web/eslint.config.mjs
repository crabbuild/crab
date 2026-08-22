import { defineConfig, globalIgnores } from "eslint/config";
import nextVitals from "eslint-config-next/core-web-vitals";
import nextTs from "eslint-config-next/typescript";

const eslintConfig = defineConfig([
  ...nextVitals,
  ...nextTs,
  // Override default ignores of eslint-config-next.
  globalIgnores([
    // Default ignores of eslint-config-next:
    ".next/**",
    "out/**",
    "build/**",
    "next-env.d.ts",
    // Fumadocs output is generated from the docs source and is not hand-edited.
    ".source/**",
  ]),
  {
    // These effects intentionally synchronize browser APIs (media queries,
    // observers, and animation frames) with local UI state. Keep the rule
    // visible as a warning without making the generated React guidance block
    // the web quality gate.
    rules: {
      "react-hooks/set-state-in-effect": "warn",
    },
  },
]);

export default eslintConfig;
