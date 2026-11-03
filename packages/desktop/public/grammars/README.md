# Tree-sitter Grammar WASM Files

This directory holds pre-compiled tree-sitter grammar WASM files used by the
symbol outline and breadcrumb features.

## Required files

- `tree-sitter-typescript.wasm` — TypeScript / JavaScript
- `tree-sitter-python.wasm` — Python
- `tree-sitter-rust.wasm` — Rust
- `tree-sitter-go.wasm` — Go
- `tree-sitter-java.wasm` — Java

## How to obtain

Grammar WASM files can be built from source using `tree-sitter-cli`:

```bash
npm install -g tree-sitter-cli

# Example for TypeScript:
git clone https://github.com/tree-sitter/tree-sitter-typescript
cd tree-sitter-typescript/typescript
tree-sitter build --wasm
cp tree-sitter-typescript.wasm /path/to/crab-desktop/public/grammars/
```

Or download pre-built WASM files from the tree-sitter grammar releases on
GitHub. Many grammars publish `.wasm` artifacts in their CI.
