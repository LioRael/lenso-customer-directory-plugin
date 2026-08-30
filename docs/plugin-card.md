# Customer Directory v1 Plugin card

## Owner and deletion boundary

`lenso-customer-directory-postgres-plugin` owns external-customer contact IDs,
organization-local normalized email aliases, optional display names, merge
tombstones, command receipts, and privacy-retention receipts. Removing its
package, Instance, bindings, and owned PostgreSQL schema removes all Customer
Directory behavior and personal profile data. Support cases and other Plugins
retain only opaque contact IDs; Kernel contains no Customer Directory branch or
registry entry.

## Provided and required roles

The Plugin provides portable `lenso.customer-directory@1` with
`resolve_or_create_email_contact`, `get_contact`, and `merge_contact`. The same
release provides real PostgreSQL implementations of
`lenso.data-export-source@1` and `lenso.retention-participant@1`.

It requires exactly one `lenso.secrets@1` Provider during activation and one
`lenso.access-control@1` Provider for administrator authorization. It does not
require Organization Membership: an inbound channel's external sender is not
silently promoted to a Lenso user or member.

## Authorization boundary

Resolution is admitted only from exact configured channel caller Instance
keys. Human `get_contact` and `merge_contact` calls require a separate exact
admin caller allowlist, an Auth `ActorAssertion` audienced to the exact
operation, and an allow decision for `customer.contacts.read` or
`customer.contacts.merge` in the target Organization scope. The Plugin makes
the final organization-local resource decision. Dependency domain rejection
never becomes an allow decision, and dependency outages remain Runtime
Failures.

Export and retention each have their own exact caller allowlist. No caller can
gain another surface merely because it is admitted to resolution.

## Lifecycle and state

Composition supplies an owned schema, database secret reference, Auth issuer
and public verification key, four exact caller allowlists, and an export size
limit. `prepare` resolves the secret and verifies an operator-installed schema;
`deactivate` closes the owned pool. Setup and upgrade are explicit operator
work. There is no background task or in-memory durable fallback.

PostgreSQL is the only source of truth. Organization-email advisory locks and a
unique alias key make concurrent resolution converge. Caller/idempotency-key
locks and durable response receipts make exact replay deterministic. Merge
locks both UUIDs in stable order, compares both positive decimal revisions,
moves aliases, repoints prior tombstones, and advances source and target
revisions in one transaction.

## First observable behavior

A configured email-channel Plugin resolves `Customer@Example.COM` and receives
one stable `contact.contact_id` plus normalized `customer@example.com`. Exact
webhook replay returns the stored response. Another caller resolving the same
email converges on the same active contact. An admin can read or merge it only
after Auth and Access Control allow the exact operation.

## Privacy and honest limits

Export follows merged IDs to the canonical contact and includes its private
email aliases in one bounded JSON item. Anonymize/delete erase direct
identifiers for the whole canonical family while retaining opaque tombstones
needed by other domain records. v1 has no listing/search, phones, postal
addresses, arbitrary fields, identity verification, membership provisioning,
cross-Organization merge, HTTP/UI contribution, or Audit event role.
