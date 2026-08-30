# Lenso Customer Directory Plugin

A removable, PostgreSQL-backed customer identity directory for Lenso Apps.

The Plugin owns external-customer contact IDs, normalized email aliases,
display names, merge tombstones, idempotency receipts, and privacy-retention
receipts. It does not own Organizations, authenticated users, memberships,
support cases, email transport, or RBAC policy.

## Capabilities

The Plugin provides:

- `lenso.customer-directory@1`
  - `resolve_or_create_email_contact`
  - `get_contact`
  - `merge_contact`
- `lenso.data-export-source@1`: a bounded JSON export for a contact subject.
- `lenso.retention-participant@1`: durable, idempotent anonymize/delete
  participation that preserves opaque IDs referenced by other Plugins.

It requires exactly one Provider for each of:

- `lenso.secrets@1`
- `lenso.access-control@1`

## Public contract

`resolve_or_create_email_contact` accepts `organization_id`, `email`, optional
`display_name`, and `idempotency_key`; it returns `{ contact, created }`.
Email is trimmed, validated as bounded ASCII addr-spec input, lowercased, and
unique within one Organization. The operation is intended for trusted channel
Plugins such as inbound email. It requires an exact configured caller Instance
but deliberately does not treat the external sender as an authenticated user
or require Organization membership.

`get_contact` accepts `organization_id` and `contact_id`. `merge_contact`
accepts source and target IDs, both expected decimal revisions, and an
idempotency key. Both operations require an exact configured admin caller and
an Auth assertion issued for the exact operation. Access Control must finally
allow `customer.contacts.read` or `customer.contacts.merge` in scope
`{ kind: "organization", id: organization_id }`.

Every returned contact has these stable fields:

- `contact_id`, `organization_id`, and normalized primary `email`
- nullable `display_name`
- `state`, either `active` or `merged`
- `canonical_contact_id`
- decimal-string `revision`
- `created_at` and `updated_at`

Merge locks both active contacts in UUID order and compares both expected
revisions before moving every email alias. The source becomes a redacted
tombstone whose canonical ID points directly to the target. Previously merged
tombstones are repointed in the same transaction, so chains are never exposed.

## Privacy behavior

The contact UUID is the privacy subject used by the shared export and retention
roles. Requests for a merged UUID resolve to its canonical contact family.
Anonymize and delete erase display names and all original email aliases. Delete
retains only a synthetic `.invalid` address and the opaque contact/tombstone IDs
because support cases and other Plugins may still reference those IDs.

## Schema lifecycle

`CustomerDirectoryOperator::setup` creates the owned schema and migration
ledger. `CustomerDirectoryOperator::upgrade` applies pending authored
migrations. Runtime activation only calls `OwnedPostgres::prepare`, so App boot
never performs DDL and refuses a missing or stale schema.

## Verification

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
lenso-contract-codegen workspace check --manifest-path Cargo.toml
./scripts/check-repository-boundary.sh
```

These commands validate the current local suite-integration baseline. Run
`./scripts/check-public-packages.sh` only after the unpublished Auth SDK,
Access Control, Data Export Source, and Retention Participant path dependencies
have been published or replaced with immutable remote coordinates. The script
intentionally blocks publication while any of those local paths remain.

Set `LENSO_CUSTOMER_DIRECTORY_TEST_DATABASE_URL` to run the optional PostgreSQL
acceptance test. It proves concurrent email convergence, restart replay, merge
alias movement, and retention erasure against an isolated random schema.

The intended two public crates and their dormant manual Trusted Publishing
workflow are documented in [`docs/release-process.md`](docs/release-process.md).
This repository is not release-ready until the suite dependency prerequisite
there is cleared.

## v1 limits

v1 supports one primary display email plus private aliases created through
merge. It has no contact listing/search, phone/address profiles, arbitrary
custom fields, automatic Organization membership, cross-Organization merge,
or Console surface. A successful channel resolution does not prove that the
sender controls an authenticated Lenso identity.
