# Agent instructions

This repository owns the removable Customer Directory domain boundary.

- PostgreSQL is the only durable runtime state. App activation verifies an
  operator-managed schema and never creates or upgrades it.
- Email channel resolution is authorized by exact configured caller Instance;
  an external sender is not an authenticated Organization member.
- Human reads and merges require an exact configured admin caller, an
  operation-audienced Auth `ActorAssertion`, an Access Control allow decision,
  and the Plugin's final organization-local row predicate.
- Preserve organization-local normalized email uniqueness, caller-scoped
  idempotency, dual-revision merge fencing, and direct canonical merge targets.
- Capability descriptors and Schemas are authoritative. Regenerate Rust
  projections with `lenso-contract-codegen`; never hand-edit them.
- Before release work, read `docs/release-process.md` and do not bypass Trusted
  Publishing controls.
