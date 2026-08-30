//! PostgreSQL-backed Customer Directory Plugin with channel and administrator boundaries.

mod operator;
#[cfg(all(test, feature = "postgres-acceptance"))]
mod postgres_tests;
mod schema;
mod storage;

use std::{cell::RefCell, collections::BTreeSet, fmt, rc::Rc, time::Duration};

use lenso::prelude::*;
use lenso_auth_sdk::{
    ActorAssertion, ActorAssertionVerifier, ActorProjectionError, AssertionClock, TypedActor,
};
use lenso_capability_access_control as access;
use lenso_capability_access_control::{
    AccessControlInvocationError, CheckPermissionRequest, CheckPermissionRequestScope,
};
use lenso_capability_customer_directory as directory;
use lenso_capability_customer_directory::{
    Contact, GetContactError, GetContactRequest, GetContactResponse, MergeContactError,
    MergeContactRequest, MergeContactResponse, ResolveOrCreateEmailContactError,
    ResolveOrCreateEmailContactRequest, ResolveOrCreateEmailContactResponse,
};
use lenso_capability_data_export_source as export_source;
use lenso_capability_data_export_source::{
    CollectExportError, CollectExportRequest, CollectExportResponse, CollectExportResponseItemsItem,
};
use lenso_capability_retention_participant as retention;
use lenso_capability_retention_participant::{
    ApplyRetentionError, ApplyRetentionRequest, ApplyRetentionRequestMode, ApplyRetentionResponse,
};
use lenso_capability_secrets as secrets;
use lenso_capability_secrets::{ResolveRequest, SecretsClient, SecretsInvocationError};
use lenso_kernel::{PluginDependencies, RuntimeFailure};
use lenso_postgres_kit::OwnedPostgres;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

pub use operator::{CustomerDirectoryOperator, CustomerDirectoryOperatorError};

const DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CALLERS: usize = 64;
const MAX_ID_BYTES: usize = 512;
const MAX_IDEMPOTENCY_BYTES: usize = 200;
const MAX_DISPLAY_NAME_BYTES: usize = 200;
const MAX_REASON_BYTES: usize = 2_000;
const DEFAULT_MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;

const CONTACTS_READ: &str = "customer.contacts.read";
const CONTACTS_MERGE: &str = "customer.contacts.merge";

/// Immutable configuration for one Customer Directory Plugin Instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerDirectoryConfig {
    schema: String,
    database_url_secret: String,
    auth_issuer: String,
    auth_assertion_public_key: String,
    resolve_callers: Vec<String>,
    admin_callers: Vec<String>,
    export_callers: Vec<String>,
    retention_callers: Vec<String>,
    #[serde(default = "default_max_export_bytes")]
    max_export_bytes: usize,
}

impl CustomerDirectoryConfig {
    /// Creates and validates immutable Customer Directory configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        schema: impl Into<String>,
        database_url_secret: impl Into<String>,
        auth_issuer: impl Into<String>,
        auth_assertion_public_key: impl Into<String>,
        resolve_callers: Vec<String>,
        admin_callers: Vec<String>,
        export_callers: Vec<String>,
        retention_callers: Vec<String>,
        max_export_bytes: usize,
    ) -> Result<Self, CustomerDirectoryConfigError> {
        let config = Self {
            schema: schema.into(),
            database_url_secret: database_url_secret.into(),
            auth_issuer: auth_issuer.into(),
            auth_assertion_public_key: auth_assertion_public_key.into(),
            resolve_callers,
            admin_callers,
            export_callers,
            retention_callers,
            max_export_bytes,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), CustomerDirectoryConfigError> {
        schema::schema_plan(self.schema.clone())
            .map_err(|_| CustomerDirectoryConfigError::InvalidSchema)?;
        if !valid_secret_reference(&self.database_url_secret) {
            return Err(CustomerDirectoryConfigError::InvalidSecretReference);
        }
        if !valid_identifier(&self.auth_issuer, 256) {
            return Err(CustomerDirectoryConfigError::InvalidAuthIssuer);
        }
        ActorAssertionVerifier::from_public_key_base64(
            self.auth_issuer.clone(),
            &self.auth_assertion_public_key,
        )
        .map_err(|_| CustomerDirectoryConfigError::InvalidAuthPublicKey)?;
        validate_callers(&self.resolve_callers)
            .map_err(|()| CustomerDirectoryConfigError::InvalidResolveCallers)?;
        validate_callers(&self.admin_callers)
            .map_err(|()| CustomerDirectoryConfigError::InvalidAdminCallers)?;
        validate_callers(&self.export_callers)
            .map_err(|()| CustomerDirectoryConfigError::InvalidExportCallers)?;
        validate_callers(&self.retention_callers)
            .map_err(|()| CustomerDirectoryConfigError::InvalidRetentionCallers)?;
        if !(1..=DEFAULT_MAX_EXPORT_BYTES).contains(&self.max_export_bytes) {
            return Err(CustomerDirectoryConfigError::InvalidExportLimit);
        }
        Ok(())
    }

    fn verifier(&self) -> Result<ActorAssertionVerifier, RuntimeFailure> {
        ActorAssertionVerifier::from_public_key_base64(
            self.auth_issuer.clone(),
            &self.auth_assertion_public_key,
        )
        .map_err(|_| RuntimeFailure::InvalidResolvedPlan {
            detail: "Customer Directory Auth verification key is invalid".to_owned(),
        })
    }
}

/// Invalid immutable Customer Directory configuration.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CustomerDirectoryConfigError {
    #[error("invalid owned PostgreSQL schema")]
    InvalidSchema,
    #[error("invalid database URL secret reference")]
    InvalidSecretReference,
    #[error("invalid Auth issuer")]
    InvalidAuthIssuer,
    #[error("invalid Auth assertion public key")]
    InvalidAuthPublicKey,
    #[error("resolve_callers must contain unique exact Instance keys")]
    InvalidResolveCallers,
    #[error("admin_callers must contain unique exact Instance keys")]
    InvalidAdminCallers,
    #[error("export_callers must contain unique exact Instance keys")]
    InvalidExportCallers,
    #[error("retention_callers must contain unique exact Instance keys")]
    InvalidRetentionCallers,
    #[error("max_export_bytes must be between 1 and 8388608")]
    InvalidExportLimit,
}

fn validate_config(config: &CustomerDirectoryConfig) -> Result<(), RuntimeFailure> {
    config
        .validate()
        .map_err(|error| RuntimeFailure::InvalidResolvedPlan {
            detail: format!("Customer Directory configuration is invalid: {error}"),
        })
}

#[derive(Clone, Debug)]
struct PreparedCustomerDirectory {
    postgres: OwnedPostgres,
}

#[lenso::plugin(
    lifecycle,
    configuration_schema = "configuration.schema.json",
    validate = validate_config
)]
#[derive(Clone)]
struct PostgresCustomerDirectoryPlugin {
    #[config]
    config: CustomerDirectoryConfig,
    secrets: Port<secrets::SecretsClient>,
    access: Port<access::AccessControlClient>,
    prepared: Rc<RefCell<Option<PreparedCustomerDirectory>>>,
}

impl fmt::Debug for PostgresCustomerDirectoryPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresCustomerDirectoryPlugin")
            .field("schema", &self.config.schema)
            .field("prepared", &self.prepared.borrow().is_some())
            .field("resolve_caller_count", &self.config.resolve_callers.len())
            .field("admin_caller_count", &self.config.admin_callers.len())
            .finish_non_exhaustive()
    }
}

#[lenso::provides(
    directory::CustomerDirectory,
    export_source::DataExportSource,
    retention::RetentionParticipant
)]
impl PostgresCustomerDirectoryPlugin {}

impl PostgresCustomerDirectoryPlugin {
    async fn resolve_or_create_email_contact(
        &self,
        context: Ctx,
        request: ResolveOrCreateEmailContactRequest,
    ) -> PluginResult<ResolveOrCreateEmailContactResponse, ResolveOrCreateEmailContactError> {
        let caller = Self::allowed_caller(&context, &self.config.resolve_callers)
            .ok_or_else(|| PluginError::domain(ResolveOrCreateEmailContactError::Forbidden))?;
        let email = normalize_email(&request.email)
            .ok_or_else(|| PluginError::domain(ResolveOrCreateEmailContactError::InvalidRequest))?;
        if !valid_opaque_id(&request.organization_id, MAX_ID_BYTES)
            || !valid_idempotency_key(&request.idempotency_key)
            || request
                .display_name
                .as_deref()
                .is_some_and(|name| !valid_text(name, MAX_DISPLAY_NAME_BYTES, false))
        {
            return Err(PluginError::domain(
                ResolveOrCreateEmailContactError::InvalidRequest,
            ));
        }
        let request_hash = request_hash(&request)?;
        let result = storage::resolve_or_create_email_contact(
            &self.prepared().map_err(PluginError::runtime)?.postgres,
            &caller,
            &request.idempotency_key,
            &request_hash,
            &request.organization_id,
            &email,
            request.display_name.as_deref(),
        )
        .await
        .map_err(storage_runtime)?
        .map_err(|failure| PluginError::domain(map_resolve_failure(failure)))?;
        wire_cast(&result)
    }

    async fn get_contact(
        &self,
        context: Ctx,
        request: GetContactRequest,
    ) -> PluginResult<GetContactResponse, GetContactError> {
        Self::allowed_caller(&context, &self.config.admin_callers)
            .ok_or_else(|| PluginError::domain(GetContactError::Forbidden))?;
        let actor = self
            .authenticated_subject(&context, directory::GET_CONTACT_OPERATION)
            .map_err(|()| PluginError::domain(GetContactError::Unauthenticated))?;
        let contact_id = Uuid::parse_str(&request.contact_id)
            .map_err(|_| PluginError::domain(GetContactError::InvalidRequest))?;
        if !valid_opaque_id(&request.organization_id, MAX_ID_BYTES) {
            return Err(PluginError::domain(GetContactError::InvalidRequest));
        }
        self.require_permission(&context, &request.organization_id, &actor, CONTACTS_READ)
            .await
            .map_err(map_get_authorization)?;
        let record = storage::get_contact(
            &self.prepared().map_err(PluginError::runtime)?.postgres,
            &request.organization_id,
            contact_id,
        )
        .await
        .map_err(storage_runtime)?
        .ok_or_else(|| PluginError::domain(GetContactError::ContactNotFound))?;
        Ok(GetContactResponse {
            contact: wire_cast::<Contact, GetContactError>(&record)?,
        })
    }

    async fn merge_contact(
        &self,
        context: Ctx,
        request: MergeContactRequest,
    ) -> PluginResult<MergeContactResponse, MergeContactError> {
        let caller = Self::allowed_caller(&context, &self.config.admin_callers)
            .ok_or_else(|| PluginError::domain(MergeContactError::Forbidden))?;
        let actor = self
            .authenticated_subject(&context, directory::MERGE_CONTACT_OPERATION)
            .map_err(|()| PluginError::domain(MergeContactError::Unauthenticated))?;
        let (source_id, target_id, source_revision, target_revision) =
            parse_merge_request(&request)
                .ok_or_else(|| PluginError::domain(MergeContactError::InvalidRequest))?;
        self.require_permission(&context, &request.organization_id, &actor, CONTACTS_MERGE)
            .await
            .map_err(map_merge_authorization)?;
        let request_hash = request_hash(&request)?;
        let result = storage::merge_contact(
            &self.prepared().map_err(PluginError::runtime)?.postgres,
            &caller,
            &request.idempotency_key,
            &request_hash,
            &request.organization_id,
            source_id,
            target_id,
            source_revision,
            target_revision,
        )
        .await
        .map_err(storage_runtime)?
        .map_err(|failure| PluginError::domain(map_merge_failure(failure)))?;
        wire_cast(&result)
    }

    async fn collect_export(
        &self,
        context: Ctx,
        request: CollectExportRequest,
    ) -> PluginResult<CollectExportResponse, CollectExportError> {
        if Self::allowed_caller(&context, &self.config.export_callers).is_none() {
            return Err(PluginError::domain(CollectExportError::Forbidden));
        }
        if request.scope_kind != "organization"
            || !valid_opaque_id(&request.export_id, MAX_ID_BYTES)
            || !valid_opaque_id(&request.scope_id, MAX_ID_BYTES)
            || !valid_opaque_id(&request.subject, MAX_ID_BYTES)
        {
            return Err(PluginError::domain(CollectExportError::InvalidRequest));
        }
        let value = storage::export_subject(
            &self.prepared().map_err(PluginError::runtime)?.postgres,
            &request.scope_id,
            &request.subject,
        )
        .await
        .map_err(storage_runtime)?;
        let payload = serde_json::to_string(&value).map_err(serialization_runtime)?;
        if payload.len() > self.config.max_export_bytes {
            return Err(PluginError::runtime(RuntimeFailure::ResourceExhausted {
                capability: export_source::CAPABILITY_ID,
                operation: export_source::COLLECT_EXPORT_OPERATION.to_owned(),
            }));
        }
        Ok(CollectExportResponse {
            items: vec![CollectExportResponseItemsItem {
                item_name: "customer-directory.json".to_owned(),
                media_type: "application/json".to_owned(),
                payload,
            }],
        })
    }

    async fn apply_retention(
        &self,
        context: Ctx,
        request: ApplyRetentionRequest,
    ) -> PluginResult<ApplyRetentionResponse, ApplyRetentionError> {
        if Self::allowed_caller(&context, &self.config.retention_callers).is_none() {
            return Err(PluginError::domain(ApplyRetentionError::Forbidden));
        }
        if request.scope_kind != "organization"
            || !valid_opaque_id(&request.action_id, MAX_ID_BYTES)
            || !valid_opaque_id(&request.scope_id, MAX_ID_BYTES)
            || !valid_opaque_id(&request.subject, MAX_ID_BYTES)
            || !valid_text(&request.reason, MAX_REASON_BYTES, true)
        {
            return Err(PluginError::domain(ApplyRetentionError::InvalidRequest));
        }
        let delete = matches!(request.mode, ApplyRetentionRequestMode::Delete);
        let hash = request_hash(&request)?;
        let receipt = storage::apply_retention(
            &self.prepared().map_err(PluginError::runtime)?.postgres,
            &request.action_id,
            &hash,
            &request.scope_id,
            &request.subject,
            delete,
        )
        .await
        .map_err(storage_runtime)?
        .map_err(|_| PluginError::domain(ApplyRetentionError::InvalidRequest))?;
        Ok(ApplyRetentionResponse { receipt })
    }

    fn prepared(&self) -> Result<PreparedCustomerDirectory, RuntimeFailure> {
        self.prepared
            .borrow()
            .clone()
            .ok_or_else(|| RuntimeFailure::PluginFailure {
                detail: "Customer Directory Plugin is not prepared".to_owned(),
            })
    }

    fn allowed_caller(context: &Ctx, allowed: &[String]) -> Option<String> {
        context.caller_instance().and_then(|caller| {
            allowed
                .iter()
                .any(|entry| entry == caller)
                .then(|| caller.to_owned())
        })
    }

    fn authenticated_subject(&self, context: &Ctx, operation: &str) -> Result<String, ()> {
        let actor = self
            .config
            .verifier()
            .map_err(|_| ())?
            .project_context::<CustomerActor>(
                context,
                directory::CAPABILITY_ID,
                operation,
                &UtcClock,
            )
            .map_err(|_| ())?;
        valid_opaque_id(&actor.subject, MAX_ID_BYTES)
            .then_some(actor.subject)
            .ok_or(())
    }

    async fn permission(
        &self,
        context: &Ctx,
        organization_id: &str,
        subject: &str,
        permission: &str,
    ) -> Result<bool, RuntimeFailure> {
        self.access
            .check_permission_with_context(
                context.clone(),
                CheckPermissionRequest {
                    subject: subject.to_owned(),
                    scope: CheckPermissionRequestScope {
                        kind: "organization".to_owned(),
                        id: organization_id.to_owned(),
                    },
                    permission: permission.to_owned(),
                },
            )
            .await
            .map(|response| response.allowed)
            .map_err(|error| match error {
                AccessControlInvocationError::Domain(_) => RuntimeFailure::PluginFailure {
                    detail: "Access Control rejected a Customer Directory authorization query"
                        .to_owned(),
                },
                AccessControlInvocationError::Runtime(error) => error,
            })
    }

    async fn require_permission(
        &self,
        context: &Ctx,
        organization_id: &str,
        subject: &str,
        permission: &str,
    ) -> Result<(), AuthorizationFailure> {
        if !self
            .permission(context, organization_id, subject, permission)
            .await
            .map_err(AuthorizationFailure::Runtime)?
        {
            return Err(AuthorizationFailure::Forbidden);
        }
        Ok(())
    }
}

impl Lifecycle for PostgresCustomerDirectoryPlugin {
    async fn activate(&self, context: ActivateContext) -> Result<(), RuntimeFailure> {
        let database_url = resolve_secret(
            &self.secrets,
            context.dependencies(),
            context.cancellation(),
            &self.config.database_url_secret,
        )
        .await?;
        let postgres = OwnedPostgres::prepare(
            &database_url,
            schema::schema_plan(self.config.schema.clone()).map_err(|error| {
                RuntimeFailure::InvalidResolvedPlan {
                    detail: error.to_string(),
                }
            })?,
        )
        .await
        .map_err(|error| RuntimeFailure::PluginFailure {
            detail: error.to_string(),
        })?;
        self.prepared
            .borrow_mut()
            .replace(PreparedCustomerDirectory { postgres });
        Ok(())
    }

    async fn deactivate(&self, _context: DeactivateContext) -> Result<(), RuntimeFailure> {
        let prepared = self.prepared.borrow_mut().take();
        if let Some(prepared) = prepared {
            prepared.postgres.pool().close().await;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct CustomerActor {
    subject: String,
}

impl TypedActor for CustomerActor {
    fn from_assertion(assertion: &ActorAssertion) -> Result<Self, ActorProjectionError> {
        Ok(Self {
            subject: assertion.subject().to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct UtcClock;

impl AssertionClock for UtcClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Debug)]
enum AuthorizationFailure {
    Forbidden,
    Runtime(RuntimeFailure),
}

async fn resolve_secret(
    secrets: &SecretsClient,
    dependencies: &PluginDependencies,
    cancellation: lenso_kernel::CancellationToken,
    reference: &str,
) -> Result<Zeroizing<String>, RuntimeFailure> {
    let context = dependencies.invocation_context_after(DEPENDENCY_TIMEOUT, cancellation)?;
    secrets
        .resolve_with_context(
            context,
            ResolveRequest {
                reference: reference.to_owned(),
            },
        )
        .await
        .map(|response| Zeroizing::new(response.value))
        .map_err(|error| match error {
            SecretsInvocationError::Domain(_) => RuntimeFailure::PluginFailure {
                detail: format!("database URL secret `{reference}` was rejected"),
            },
            SecretsInvocationError::Runtime(error) => error,
        })
}

fn map_get_authorization(failure: AuthorizationFailure) -> PluginError<GetContactError> {
    match failure {
        AuthorizationFailure::Forbidden => PluginError::domain(GetContactError::Forbidden),
        AuthorizationFailure::Runtime(error) => PluginError::runtime(error),
    }
}

fn map_merge_authorization(failure: AuthorizationFailure) -> PluginError<MergeContactError> {
    match failure {
        AuthorizationFailure::Forbidden => PluginError::domain(MergeContactError::Forbidden),
        AuthorizationFailure::Runtime(error) => PluginError::runtime(error),
    }
}

fn map_resolve_failure(failure: storage::DomainFailure) -> ResolveOrCreateEmailContactError {
    match failure {
        storage::DomainFailure::IdempotencyConflict => {
            ResolveOrCreateEmailContactError::IdempotencyConflict
        }
        _ => ResolveOrCreateEmailContactError::InvalidRequest,
    }
}

fn map_merge_failure(failure: storage::DomainFailure) -> MergeContactError {
    match failure {
        storage::DomainFailure::ContactNotFound => MergeContactError::ContactNotFound,
        storage::DomainFailure::ContactNotActive => MergeContactError::ContactNotActive,
        storage::DomainFailure::RevisionConflict => MergeContactError::RevisionConflict,
        storage::DomainFailure::IdempotencyConflict => MergeContactError::IdempotencyConflict,
        storage::DomainFailure::RetentionConflict => MergeContactError::InvalidRequest,
    }
}

fn request_hash<T: Serialize, E>(request: &T) -> Result<Vec<u8>, PluginError<E>> {
    serde_json::to_vec(request)
        .map(|wire| Sha256::digest(wire).to_vec())
        .map_err(serialization_runtime)
}

fn wire_cast<T: DeserializeOwned, E>(value: &impl Serialize) -> Result<T, PluginError<E>> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(serialization_runtime)
}

#[allow(clippy::needless_pass_by_value)]
fn serialization_runtime<E>(error: serde_json::Error) -> PluginError<E> {
    PluginError::runtime(RuntimeFailure::Internal {
        detail: format!("Customer Directory wire serialization failed: {error}"),
    })
}

#[allow(clippy::needless_pass_by_value)]
fn storage_runtime<E>(error: storage::StorageError) -> PluginError<E> {
    PluginError::runtime(RuntimeFailure::PluginFailure {
        detail: error.to_string(),
    })
}

fn parse_merge_request(request: &MergeContactRequest) -> Option<(Uuid, Uuid, i64, i64)> {
    if !valid_opaque_id(&request.organization_id, MAX_ID_BYTES)
        || !valid_idempotency_key(&request.idempotency_key)
    {
        return None;
    }
    let source = Uuid::parse_str(&request.source_contact_id).ok()?;
    let target = Uuid::parse_str(&request.target_contact_id).ok()?;
    if source == target {
        return None;
    }
    let source_revision = request
        .expected_source_revision
        .parse::<i64>()
        .ok()
        .filter(|revision| *revision > 0)?;
    let target_revision = request
        .expected_target_revision
        .parse::<i64>()
        .ok()
        .filter(|revision| *revision > 0)?;
    Some((source, target, source_revision, target_revision))
}

fn normalize_email(value: &str) -> Option<String> {
    let value = value.trim();
    if !(3..=320).contains(&value.len()) || !value.is_ascii() {
        return None;
    }
    let (local, domain) = value.split_once('@')?;
    if local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || domain.len() > 255
        || domain.contains('@')
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
        })
    {
        return None;
    }
    if domain.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn valid_text(value: &str, maximum: usize, allow_empty: bool) -> bool {
    let trimmed = value.trim();
    (allow_empty || !trimmed.is_empty())
        && value.len() <= maximum
        && !value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn valid_idempotency_key(value: &str) -> bool {
    valid_opaque_id(value, MAX_IDEMPOTENCY_BYTES)
}

fn valid_opaque_id(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    valid_opaque_id(value, maximum) && !value.contains('/')
}

fn valid_secret_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 256
        && !reference.starts_with('/')
        && !reference.ends_with('/')
        && !reference.contains("//")
        && reference
            .split('/')
            .all(|segment| segment != "." && segment != "..")
        && valid_opaque_id(reference, 256)
}

fn validate_callers(callers: &[String]) -> Result<(), ()> {
    if callers.is_empty()
        || callers.len() > MAX_CALLERS
        || callers.iter().any(|caller| !valid_identifier(caller, 256))
        || callers.iter().collect::<BTreeSet<_>>().len() != callers.len()
    {
        Err(())
    } else {
        Ok(())
    }
}

const fn default_max_export_bytes() -> usize {
    DEFAULT_MAX_EXPORT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use lenso_auth_sdk::{ActorAssertionIssuer, Validity, audience};
    use lenso_kernel::{CancellationToken, InvocationContext};
    use lenso_native_adapter::NativePluginRegistry;
    use time::Duration as TimeDuration;

    fn config() -> CustomerDirectoryConfig {
        let issuer = ActorAssertionIssuer::new("auth.users", b"customer-directory-test-key");
        CustomerDirectoryConfig::new(
            "customer_directory",
            "customer-directory/database-url",
            "auth.users",
            issuer.public_key_base64(),
            vec!["support-email".to_owned()],
            vec!["support-admin".to_owned()],
            vec!["privacy-export".to_owned()],
            vec!["privacy-retention".to_owned()],
            DEFAULT_MAX_EXPORT_BYTES,
        )
        .unwrap()
    }

    fn plugin() -> PostgresCustomerDirectoryPlugin {
        PostgresCustomerDirectoryPlugin {
            config: config(),
            secrets: Port::default(),
            access: Port::default(),
            prepared: Rc::new(RefCell::new(None)),
        }
    }

    fn context(caller: &str) -> InvocationContext {
        InvocationContext::new(1, None, CancellationToken::new()).with_caller_instance(caller)
    }

    #[test]
    fn descriptor_declares_only_owned_roles_and_required_dependencies() {
        let descriptor: serde_json::Value = serde_json::from_str(PLUGIN_DESCRIPTOR_JSON).unwrap();
        let provided = descriptor["provided_capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value["capability_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            provided,
            BTreeSet::from([
                directory::CAPABILITY_ID,
                export_source::CAPABILITY_ID,
                retention::CAPABILITY_ID,
            ])
        );
        let required = descriptor["required_capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value["capability_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            required,
            BTreeSet::from([secrets::CAPABILITY_ID, access::CAPABILITY_ID])
        );
        assert_eq!(
            NativePluginRegistry::new()
                .with_linked_factories()
                .factories()
                .filter(|factory| factory.package_id() == PACKAGE_ID)
                .count(),
            1
        );
    }

    #[test]
    fn email_normalization_is_deterministic_and_rejects_malformed_input() {
        assert_eq!(
            normalize_email("  Customer+Case@Example.COM  "),
            Some("customer+case@example.com".to_owned())
        );
        assert_eq!(normalize_email("missing-domain@"), None);
        assert_eq!(normalize_email("dot..dot@example.com"), None);
        assert_eq!(normalize_email("customer@-example.com"), None);
        assert_eq!(normalize_email("客户@example.com"), None);
    }

    #[test]
    fn config_rejects_empty_and_duplicate_caller_sets() {
        let mut invalid = config();
        invalid.resolve_callers.clear();
        assert_eq!(
            invalid.validate(),
            Err(CustomerDirectoryConfigError::InvalidResolveCallers)
        );
        let mut invalid = config();
        invalid.admin_callers.push("support-admin".to_owned());
        assert_eq!(
            invalid.validate(),
            Err(CustomerDirectoryConfigError::InvalidAdminCallers)
        );
    }

    #[test]
    fn admin_actor_assertions_are_bound_to_the_exact_operation() {
        let issuer = ActorAssertionIssuer::new("auth.users", b"customer-directory-test-key");
        let now = OffsetDateTime::now_utc();
        let assertion = issuer.issue(
            "usr_admin",
            "user",
            "strong",
            [audience(
                directory::CAPABILITY_ID,
                directory::GET_CONTACT_OPERATION,
            )],
            Validity::new(
                now - TimeDuration::seconds(1),
                now + TimeDuration::minutes(1),
            )
            .unwrap(),
            BTreeSet::new().into_iter().collect(),
        );
        let context = assertion.attach(context("support-admin")).unwrap();
        assert_eq!(
            plugin().authenticated_subject(&context, directory::GET_CONTACT_OPERATION),
            Ok("usr_admin".to_owned())
        );
        assert_eq!(
            plugin().authenticated_subject(&context, directory::MERGE_CONTACT_OPERATION),
            Err(())
        );
    }

    #[test]
    fn channel_resolution_requires_an_exact_caller_before_storage() {
        let request = ResolveOrCreateEmailContactRequest {
            organization_id: "org_1".to_owned(),
            email: "customer@example.com".to_owned(),
            display_name: None,
            idempotency_key: "message_1".to_owned(),
        };
        let result = futures::executor::block_on(
            plugin().resolve_or_create_email_contact(context("other-channel"), request),
        );
        assert_eq!(
            result,
            Err(PluginError::Domain(
                ResolveOrCreateEmailContactError::Forbidden
            ))
        );
    }

    #[test]
    fn merge_parser_requires_distinct_ids_and_positive_decimal_revisions() {
        let source = Uuid::new_v4();
        let target = Uuid::new_v4();
        let valid = MergeContactRequest {
            organization_id: "org_1".to_owned(),
            source_contact_id: source.to_string(),
            target_contact_id: target.to_string(),
            expected_source_revision: "1".to_owned(),
            expected_target_revision: "2".to_owned(),
            idempotency_key: "merge_1".to_owned(),
        };
        assert_eq!(parse_merge_request(&valid), Some((source, target, 1, 2)));
        let mut invalid = valid.clone();
        invalid.target_contact_id = source.to_string();
        assert_eq!(parse_merge_request(&invalid), None);
        let mut invalid = valid;
        invalid.expected_source_revision = "0".to_owned();
        assert_eq!(parse_merge_request(&invalid), None);
    }
}
