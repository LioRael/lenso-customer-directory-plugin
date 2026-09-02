#!/usr/bin/env bash
set -euo pipefail

cargo_bin="${LENSO_CARGO_BIN:-cargo}"
repository_root="$(git rev-parse --show-toplevel)"

suite_path_dependencies="$(rg -n 'path\s*=\s*"\.\./\.worktrees/' "$repository_root/Cargo.toml" || true)"
if [[ -n "$suite_path_dependencies" ]]; then
  printf '%s\n' \
    'public-package verification is blocked by unpublished local suite dependencies:' \
    "$suite_path_dependencies" \
    'Publish or merge those dependencies, replace the paths with public or immutable remote coordinates, and regenerate Cargo.lock.' >&2
  exit 1
fi

metadata="$($cargo_bin metadata --locked --no-deps --format-version=1)"
target_directory="$(jq -r '.target_directory' <<<"$metadata")"
verification_root="$(mktemp -d "${TMPDIR:-/tmp}/lenso-customer-directory-packages.XXXXXX")"
trap 'find "$verification_root" -depth -delete' EXIT

packages=(
  lenso-capability-customer-directory
  lenso-customer-directory-postgres-plugin
)

for package in "${packages[@]}"; do
  manifest="$(jq -r --arg package "$package" '.packages[] | select(.name == $package) | .manifest_path' <<<"$metadata")"
  if [[ -z "$manifest" || ! -f "$manifest" ]]; then
    printf 'public package manifest is missing: %s\n' "$package" >&2
    exit 1
  fi
  if ! rg -qx 'publish = true' "$manifest"; then
    printf '%s is not explicitly publishable\n' "$package" >&2
    exit 1
  fi
  publish="$(jq -r --arg package "$package" '.packages[] | select(.name == $package) | .publish == null or (.publish | length > 0)' <<<"$metadata")"
  if [[ "$publish" != "true" ]]; then
    printf '%s is not public in Cargo metadata\n' "$package" >&2
    exit 1
  fi
done

required_source_set=(
  crates/lenso-capability-customer-directory/capability.json
  crates/lenso-capability-customer-directory/schemas/resolve-or-create-email-contact-request.schema.json
  crates/lenso-capability-customer-directory/src/generated.rs
  crates/lenso-capability-customer-directory/src/lib.rs
  crates/lenso-customer-directory-postgres-plugin/configuration.schema.json
  crates/lenso-customer-directory-postgres-plugin/migrations/001_create_customer_directory.sql
  crates/lenso-customer-directory-postgres-plugin/src/lib.rs
  crates/lenso-customer-directory-postgres-plugin/src/operator.rs
  crates/lenso-customer-directory-postgres-plugin/src/storage.rs
)
for source in "${required_source_set[@]}"; do
  if [[ ! -f "$repository_root/$source" ]]; then
    printf 'required public package source is missing: %s\n' "$source" >&2
    exit 1
  fi
done

capability_manifest="$repository_root/crates/lenso-capability-customer-directory/Cargo.toml"
for packaged_asset in '"capability.json"' '"schemas/*.json"' '"src/*.rs"'; do
  rg --fixed-strings --quiet "$packaged_asset" "$capability_manifest" || {
    printf 'Capability include set is missing %s\n' "$packaged_asset" >&2
    exit 1
  }
done
plugin_manifest="$repository_root/crates/lenso-customer-directory-postgres-plugin/Cargo.toml"
for packaged_asset in '"configuration.schema.json"' '"migrations/*.sql"' '"src/*.rs"'; do
  rg --fixed-strings --quiet "$packaged_asset" "$plugin_manifest" || {
    printf 'Plugin include set is missing %s\n' "$packaged_asset" >&2
    exit 1
  }
done

package_flags=(--locked)
if [[ "${LENSO_PACKAGE_ALLOW_DIRTY:-0}" == "1" ]]; then
  package_flags+=(--allow-dirty)
fi

"$cargo_bin" package --quiet "${package_flags[@]}" -p lenso-capability-customer-directory
"$cargo_bin" package --quiet "${package_flags[@]}" --no-verify \
  -p lenso-customer-directory-postgres-plugin \
  --config 'patch.crates-io.lenso-capability-customer-directory.path="crates/lenso-capability-customer-directory"'

for package in "${packages[@]}"; do
  version="$(jq -r --arg package "$package" '.packages[] | select(.name == $package) | .version' <<<"$metadata")"
  archive="$target_directory/package/$package-$version.crate"
  if [[ ! -s "$archive" ]]; then
    printf 'public package archive is missing: %s\n' "$archive" >&2
    exit 1
  fi
  tar -xzf "$archive" -C "$verification_root"
  extracted="$verification_root/$package-$version"
  if [[ ! -f "$extracted/Cargo.toml" || ! -f "$extracted/Cargo.toml.orig" ]]; then
    printf 'normalized manifest pair is missing from %s\n' "$package" >&2
    exit 1
  fi
  if rg -n '/Users/[^/[:space:]]+|[A-Za-z]:\\Users\\' "$extracted"; then
    printf 'local absolute path leaked into %s\n' "$package" >&2
    exit 1
  fi
done

capability_version="$(jq -r '.packages[] | select(.name == "lenso-capability-customer-directory") | .version' <<<"$metadata")"
plugin_version="$(jq -r '.packages[] | select(.name == "lenso-customer-directory-postgres-plugin") | .version' <<<"$metadata")"
capability_extracted="$verification_root/lenso-capability-customer-directory-$capability_version"
plugin_extracted="$verification_root/lenso-customer-directory-postgres-plugin-$plugin_version"

for source in "${required_source_set[@]}"; do
  relative="${source#crates/lenso-capability-customer-directory/}"
  if [[ "$relative" != "$source" ]]; then
    test -f "$capability_extracted/$relative" || {
      printf 'Capability archive source is missing: %s\n' "$relative" >&2
      exit 1
    }
    continue
  fi
  relative="${source#crates/lenso-customer-directory-postgres-plugin/}"
  test -f "$plugin_extracted/$relative" || {
    printf 'Plugin archive source is missing: %s\n' "$relative" >&2
    exit 1
  }
done

"$cargo_bin" check --quiet --locked --all-targets --all-features \
  --manifest-path "$plugin_extracted/Cargo.toml" \
  --config "patch.crates-io.lenso-capability-customer-directory.path='$capability_extracted'"

printf 'public Customer Directory package archives and source sets are valid\n'
