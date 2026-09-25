# beyonddb

Root and `crates/AGENTS.md` apply.

- Keep DynamoDB request parsing, SigV4, IAM, and HTTP outside Cell handlers.
- Store each partition-local mutation, result, and stream intent through one Cell command. Cross-Cell transactions require a durable coordinator decision and idempotent participant resolution.
- Preserve ExtendDB's item and key semantics. Compare new backend results with its SQLite backend and the protocol suite before claiming compatibility.
- Do not report an API as supported until an AWS SDK request reaches a durable Cell commit and the result survives owner restart.
- ExtendDB is pinned as an external Git dependency; review lockfile changes before commit.
