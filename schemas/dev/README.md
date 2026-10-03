# `schemas/dev/` — JSON Schemas for the `./dev --json` outputs

Every `./dev` command prints exactly one JSON object in `--json` mode, and every receipt
and step record it writes is JSON too. Each carries a versioned identifier in its
`schema` field, `keyval.dev.<name>/v<n>` (PLAN.md O-6, §11.5 rule 14). This directory
holds one JSON Schema (draft 2020-12) per identifier the code emits:

| Identifier | Schema | Emitted by |
|---|---|---|
| `keyval.dev.error/v1` | `error.v1.schema.json` | any command that cannot produce its report |
| `keyval.dev.doctor/v2` | `doctor.v2.schema.json` | `./dev doctor` |
| `keyval.dev.check/v2` | `check.v2.schema.json` | `./dev check` |
| `keyval.dev.gate/v3` | `gate.v3.schema.json` | `./dev gate` (embeds the receipt) |
| `keyval.dev.receipt/v3` | `receipt.v3.schema.json` | gate receipts in `artifacts/receipts/` |
| `keyval.dev.accept/v1` | `accept.v1.schema.json` | `./dev accept`, report and receipt |
| `keyval.dev.commit/v1` | `commit.v1.schema.json` | `./dev commit`, report and receipt |
| `keyval.dev.close/v1` | `close.v1.schema.json` | `./dev commit --close` (embeds the commit report) |
| `keyval.dev.beads-import/v1` | `beads-import.v1.schema.json` | `./dev beads import` |
| `keyval.dev.prune/v1` | `prune.v1.schema.json` | `./dev prune`, report and receipt |
| `keyval.dev.step/v1` | `step.v1.schema.json` | step records (stderr, `steps.jsonl`) |
| `keyval.dev.traceability/v1` | `traceability.v1.schema.json` | `./dev report traceability` |

## Conventions

- A schema's `$id` is `https://keyval.invalid/schemas/dev/<file>`, its `title` is the
  identifier, and its `schema` property is a `const` of the identifier. The `.invalid`
  host is never fetched: `$ref`s between schemas, such as `gate` to `receipt` and `close`
  to `commit`, resolve from these files offline.
- Schemas are closed (`additionalProperties: false`) and require every field, because the
  reports serialise every field, with `null` for an absent value. Where the code ties
  fields together, the schema says so too. For example, a gate report is `ok` exactly
  when its receipt passed, and a dry-run prune removes nothing.
- `invalid/<name>.v<n>.json` is the schema's negative fixture: a real output with one
  defect, the rule it breaks, and the JSON pointer (`error_at`) where the schema must
  reject it.

## Changing an output

Change the schema in the same commit as the output. When the change breaks a reader
(a field renamed, removed or retyped, or a value no longer allowed), bump the version.
That means the code's identifier constant, a renamed schema file with its new `$id`,
`title` and `const`, and a renamed negative fixture. The old file goes; git keeps it.

`./dev check DEV-SCHEMAS` enforces all of this (`xtask/src/schemas.rs`,
`xtask/tests/schemas.rs`). It fails when:

- an emitted identifier has no schema;
- an output, receipt or step record that the commands' own code produces does not match
  its schema;
- an enum gains a variant its schema does not list;
- a negative fixture is accepted.
