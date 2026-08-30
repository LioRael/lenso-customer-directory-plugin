# Release process

## Crate publication prerequisite: currently blocked

This repository is source-buildable from a clean checkout using immutable Git
coordinates for Auth SDK, Access Control, Data Export Source, and Retention
Participant. Those exact upstream revisions are not yet available as registry
packages, so crates.io publication remains intentionally deferred.

Do not publish crates or claim registry readiness until the prerequisite
contracts are published, the Git coordinates are replaced with registry
dependencies, `Cargo.lock` is regenerated, and every required evidence command
including `./scripts/check-public-packages.sh` passes.

Publish the public crates in dependency order:

1. `lenso-capability-customer-directory`
2. `lenso-customer-directory-postgres-plugin`

Publication is manual-only from reviewed `main` through
`.github/workflows/release-plz.yml`. Pushes may refresh a Release-plz pull
request but cannot publish. A live run additionally requires `live=true`, the
literal confirmation `publish`, and `main`.

## Trusted Publishers

Configure a separate crates.io Trusted Publisher for each public crate:

- owner: `LioRael`
- repository: `lenso-customer-directory-plugin`
- workflow: `release-plz.yml`
- environment: unset

Only the confirmed live job receives `id-token: write`. There is no registry
token fallback. Trusted Publishing cannot allocate a new crates.io name, so
allocate each `0.1.0` name once with a temporary new-package-only token, revoke
that token immediately, and use OIDC for subsequent publication.

## Required evidence

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
lenso-contract-codegen workspace check --manifest-path Cargo.toml
./scripts/check-repository-boundary.sh
./scripts/check-public-packages.sh
```

The first six checks (through repository boundary) validate the source release.
The public-package check is release-only and remains blocked until the registry
prerequisite above is complete.

Before publication, run PostgreSQL acceptance against a disposable database by
setting `LENSO_CUSTOMER_DIRECTORY_TEST_DATABASE_URL`. Generated Capability
projections are locked artifacts and must not be edited by hand.
