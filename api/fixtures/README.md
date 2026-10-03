# api/fixtures — SPEC check fixtures (W03.3)

These files are the oracle for `./dev check SPEC`, which judges `api/openapi.yaml` against
PLAN.md §8. The check itself, its xtask code, `// check: SPEC` marker, catalogue row and
`min_cases`, is registered separately, after W07, on bead `br-w03-3-spec-fixtures-check-tdv`.
The fixtures say **what** must hold. They never change to make a check pass (§11.5 rule 3).

| File | Schema | One case per |
|---|---|---|
| `contract.json` | `keyval.spec.contract/v1` | spec rule (below) |
| `positive.json` | `keyval.spec.fixtures/v1`, `"expect": "valid"` | instance that MUST validate |
| `negative.json` | `keyval.spec.fixtures/v1`, `"expect": "invalid"` | instance that MUST NOT validate |
| `spec-mutants.json` | `keyval.spec.mutants/v1` | mutated spec that MUST trip its named rule |

Case names are unique within a file and are the `--case` names.

## contract.json — PLAN.md §8 in machine-readable form

It is written from the plan, not generated from the spec, so it is an independent oracle.
It records:

- the nine operations: method, path, `operationId`, the §8.2 Req column (`x-requirements`), the parameters, the request and 200 schemas, and the exact set of declared statuses;
- the twelve §8.4 codes with their HTTP statuses;
- the byte-limit extensions, as JSON Pointers into the spec;
- the integer ceiling, 2^53 − 1 (D-77);
- `Cache-Control: no-store`;
- the §8.6 examples, verbatim.

The per-operation status sets include the readings recorded on the W03.1 and W03.2 beads. A
change to the contract surface changes the spec and this file in the same commit.

## Spec rules

The check reports every violation of every rule. The unmutated spec MUST have none.

| Rule | Holds when |
|---|---|
| `openapi` | The document validates as OpenAPI `contract.openapi` (3.1.0), and every local `$ref` resolves. If it fails, the other rules are skipped. |
| `no-store` | Every Response Object (inline or under `components/responses`) declares a `Cache-Control` header that is `required: true`, with schema `{type: string, const: <contract.cache_control>}`. |
| `examples` | Every Example Object value validates against its media type's schema, with the extension keywords below. |
| `closed` | Every schema with `type: object` (under `components/schemas`, `components/parameters`, `components/headers`, recursively) sets `additionalProperties: false` (§8.1 strict JSON; per-operation precision). |
| `codes` | The `ErrorCode` enum equals `contract.codes`, with no duplicates. Every error schema (a component schema with an `error` property) has `code: {$ref: ErrorCode, const: <c>}` with `c` in the enum. Every error schema reachable from an operation response sits under its code's status. Every `(code, status)` pair is reachable from at least one operation. |
| `integers` | Every integer schema declares `maximum` ≤ `contract.integer_maximum` (D-77). |
| `operations` | The spec's operations are exactly the contract's (method + path). Each one has the contract's `operationId`, `x-requirements`, parameter names, `in` and `required`, request-body schema (required when present), 200 schema, and the exact status set. |
| `limits` | Each `contract.byte_limits` entry exists at its pointer with exactly its value. |
| `plan-examples` | Each `contract.examples` entry exists (by operation, request or response status, and example name), and its value equals the contract's exactly. |
| `instances` | Every positive case validates and every negative case does not. |

## Instance cases (positive.json, negative.json)

```json
{"name": "…", "rule": "the plan clause it exercises", "target": {…}, "instance": …}
```

`target` picks the schema the instance is validated against:

- `{"schema": "<Name>"}` → `#/components/schemas/<Name>`
- `{"request": "<operationId>"}` → that operation's `application/json` request-body schema
- `{"response": {"operation": "<operationId>", "status": "<code>"}}` → that response's `application/json` schema (resolving `$ref` responses)
- `{"parameter": {"operation": "<operationId>", "name": "<name>"}}` → the parameter's schema, including path-level parameters. The instance is the **decoded** value, typed as the schema expects. Raw query-string handling (percent-decoding, the unescaped `+` rule) is the conformance suite's job, not this check's.

Before validation, every object of exactly the form `{"$repeat": "<s>", "times": <n>}` is
replaced by the string `<s>` repeated `n` times. This keeps 64 KiB and multibyte boundary
values small on disk. No schema in the spec admits a `$repeat` member, so the form is
unambiguous.

## Extension keywords

The validator MUST implement these, alongside JSON Schema 2020-12 and the `uri` format:

- `x-maxUtf8Bytes: N` — a string instance's UTF-8 encoding is at most `N` bytes. JSON
  Schema's `maxLength` counts characters, so it alone cannot enforce DM-1 or DM-2.
- `x-maxPageUtf8Bytes: N` — on `ScanPage`: when `items` holds more than one item, the sum of
  `len(key) + len(value)` in UTF-8 bytes over the items is at most `N`. The first item is
  always returned (§7.11).
- `x-maxBodyBytes: N` — on a Request Body Object. It is a recorded limit, checked by the `limits`
  rule. A raw body size is not a JSON instance, so it is enforced over HTTP by the
  conformance suite.

Four negative cases are caught **only** by these keywords. Their `rule` text says so. A
validator that ignores the extensions fails those cases, so the selection is not vacuous.

## Spec mutants (spec-mutants.json)

```json
{"name": "…", "rule": "<spec rule>", "why": "…", "patch": [<RFC 6902 operations>]}
```

The check applies `patch` (`add`, `remove` and `replace` are used) to the parsed
`api/openapi.yaml` and runs every spec rule. The case passes when `rule` reports at least one
violation. Other rules may also fire. Mutants are the self-test that proves each rule can
fail (P10). A patch that no longer applies, because the spec changed shape, fails its case
and is fixed by re-pointing the patch, never by deleting the mutant.
