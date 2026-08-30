use lenso_postgres_kit::OwnedPostgres;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction, types::Json};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ContactView {
    pub(crate) contact_id: Uuid,
    pub(crate) organization_id: String,
    pub(crate) email: String,
    pub(crate) display_name: Option<String>,
    pub(crate) state: String,
    pub(crate) canonical_contact_id: Uuid,
    #[serde(with = "decimal_i64")]
    pub(crate) revision: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ResolveResult {
    pub(crate) contact: ContactView,
    pub(crate) created: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct MergeResult {
    pub(crate) source_contact: ContactView,
    pub(crate) target_contact: ContactView,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExportPayload {
    pub(crate) requested_contact_id: String,
    pub(crate) contact: Option<ContactView>,
    pub(crate) email_aliases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DomainFailure {
    ContactNotFound,
    ContactNotActive,
    RevisionConflict,
    IdempotencyConflict,
    RetentionConflict,
}

#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("PostgreSQL operation `{operation}` failed")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("stored Customer Directory data is invalid: {detail}")]
    InvalidStoredData { detail: String },
    #[error("Customer Directory response serialization failed")]
    Serialization(#[from] serde_json::Error),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_or_create_email_contact(
    postgres: &OwnedPostgres,
    caller: &str,
    idempotency_key: &str,
    request_hash: &[u8],
    organization_id: &str,
    email_normalized: &str,
    display_name: Option<&str>,
) -> Result<Result<ResolveResult, DomainFailure>, StorageError> {
    let mut transaction = begin(postgres, "begin email contact resolution").await?;
    match command_replay::<ResolveResult>(
        &mut transaction,
        caller,
        idempotency_key,
        "resolve_or_create_email_contact",
        request_hash,
    )
    .await?
    {
        Ok(Some(replay)) => {
            commit(transaction, "commit email contact replay").await?;
            return Ok(Ok(replay));
        }
        Ok(None) => {}
        Err(failure) => return Ok(Err(failure)),
    }

    advisory_lock(
        &mut transaction,
        &format!("contact-email\u{0}{organization_id}\u{0}{email_normalized}"),
    )
    .await?;
    let existing = sqlx::query(
        "SELECT c.* FROM customer_contact_emails e JOIN customer_contacts c ON c.contact_id=e.contact_id WHERE e.organization_id=$1 AND e.email_normalized=$2 FOR UPDATE OF c",
    )
    .bind(organization_id)
    .bind(email_normalized)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| database("resolve contact email", source))?;

    let result = if let Some(row) = existing {
        let contact = decode_contact(&row)?;
        if contact.state != "active" || contact.canonical_contact_id != contact.contact_id {
            return Err(StorageError::InvalidStoredData {
                detail: "an email alias points to a non-active contact".to_owned(),
            });
        }
        ResolveResult {
            contact,
            created: false,
        }
    } else {
        let contact_id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO customer_contacts(contact_id,organization_id,primary_email,display_name,state,canonical_contact_id,revision) VALUES($1,$2,$3,$4,'active',$1,1) RETURNING *",
        )
        .bind(contact_id)
        .bind(organization_id)
        .bind(email_normalized)
        .bind(display_name)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| database("insert email contact", source))?;
        sqlx::query(
            "INSERT INTO customer_contact_emails(organization_id,email_normalized,contact_id) VALUES($1,$2,$3)",
        )
        .bind(organization_id)
        .bind(email_normalized)
        .bind(contact_id)
        .execute(&mut *transaction)
        .await
        .map_err(|source| database("insert contact email alias", source))?;
        ResolveResult {
            contact: decode_contact(&row)?,
            created: true,
        }
    };

    save_command(
        &mut transaction,
        caller,
        idempotency_key,
        organization_id,
        "resolve_or_create_email_contact",
        request_hash,
        &result,
    )
    .await?;
    commit(transaction, "commit email contact resolution").await?;
    Ok(Ok(result))
}

pub(crate) async fn get_contact(
    postgres: &OwnedPostgres,
    organization_id: &str,
    contact_id: Uuid,
) -> Result<Option<ContactView>, StorageError> {
    let row =
        sqlx::query("SELECT * FROM customer_contacts WHERE organization_id=$1 AND contact_id=$2")
            .bind(organization_id)
            .bind(contact_id)
            .fetch_optional(postgres.pool())
            .await
            .map_err(|source| database("get customer contact", source))?;
    row.as_ref().map(decode_contact).transpose()
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn merge_contact(
    postgres: &OwnedPostgres,
    caller: &str,
    idempotency_key: &str,
    request_hash: &[u8],
    organization_id: &str,
    source_contact_id: Uuid,
    target_contact_id: Uuid,
    expected_source_revision: i64,
    expected_target_revision: i64,
) -> Result<Result<MergeResult, DomainFailure>, StorageError> {
    let mut transaction = begin(postgres, "begin contact merge").await?;
    match command_replay::<MergeResult>(
        &mut transaction,
        caller,
        idempotency_key,
        "merge_contact",
        request_hash,
    )
    .await?
    {
        Ok(Some(replay)) => {
            commit(transaction, "commit contact merge replay").await?;
            return Ok(Ok(replay));
        }
        Ok(None) => {}
        Err(failure) => return Ok(Err(failure)),
    }

    advisory_lock(
        &mut transaction,
        &format!("contact-organization\u{0}{organization_id}"),
    )
    .await?;

    let rows = sqlx::query(
        "SELECT * FROM customer_contacts WHERE organization_id=$1 AND contact_id IN ($2,$3) ORDER BY contact_id FOR UPDATE",
    )
    .bind(organization_id)
    .bind(source_contact_id)
    .bind(target_contact_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|source| database("lock contacts for merge", source))?;
    if rows.len() != 2 {
        return Ok(Err(DomainFailure::ContactNotFound));
    }
    let contacts = rows
        .iter()
        .map(decode_contact)
        .collect::<Result<Vec<_>, _>>()?;
    let source = contacts
        .iter()
        .find(|contact| contact.contact_id == source_contact_id)
        .ok_or_else(|| StorageError::InvalidStoredData {
            detail: "locked merge source disappeared".to_owned(),
        })?;
    let target = contacts
        .iter()
        .find(|contact| contact.contact_id == target_contact_id)
        .ok_or_else(|| StorageError::InvalidStoredData {
            detail: "locked merge target disappeared".to_owned(),
        })?;
    if source.state != "active" || target.state != "active" {
        return Ok(Err(DomainFailure::ContactNotActive));
    }
    if source.revision != expected_source_revision || target.revision != expected_target_revision {
        return Ok(Err(DomainFailure::RevisionConflict));
    }

    sqlx::query("UPDATE customer_contact_emails SET contact_id=$3 WHERE organization_id=$1 AND contact_id=$2")
        .bind(organization_id)
        .bind(source_contact_id)
        .bind(target_contact_id)
        .execute(&mut *transaction)
        .await
        .map_err(|source| database("move merged contact aliases", source))?;
    sqlx::query(
        "UPDATE customer_contacts SET canonical_contact_id=$3,revision=revision+1,updated_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND state='merged' AND canonical_contact_id=$2",
    )
    .bind(organization_id)
    .bind(source_contact_id)
    .bind(target_contact_id)
    .execute(&mut *transaction)
    .await
    .map_err(|source| database("repoint prior contact merges", source))?;

    let source_redaction = format!("merged-{source_contact_id}@redacted.invalid");
    let source_row = sqlx::query(
        "UPDATE customer_contacts SET primary_email=$3,display_name=NULL,state='merged',canonical_contact_id=$4,revision=revision+1,updated_at=CURRENT_TIMESTAMP,merged_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND contact_id=$2 RETURNING *",
    )
    .bind(organization_id)
    .bind(source_contact_id)
    .bind(source_redaction)
    .bind(target_contact_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|source| database("mark source contact merged", source))?;
    let target_row = sqlx::query(
        "UPDATE customer_contacts SET revision=revision+1,updated_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND contact_id=$2 RETURNING *",
    )
    .bind(organization_id)
    .bind(target_contact_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|source| database("advance target contact revision", source))?;
    let result = MergeResult {
        source_contact: decode_contact(&source_row)?,
        target_contact: decode_contact(&target_row)?,
    };
    save_command(
        &mut transaction,
        caller,
        idempotency_key,
        organization_id,
        "merge_contact",
        request_hash,
        &result,
    )
    .await?;
    commit(transaction, "commit contact merge").await?;
    Ok(Ok(result))
}

pub(crate) async fn export_subject(
    postgres: &OwnedPostgres,
    organization_id: &str,
    subject: &str,
) -> Result<ExportPayload, StorageError> {
    let Some(requested_id) = Uuid::parse_str(subject).ok() else {
        return Ok(ExportPayload {
            requested_contact_id: subject.to_owned(),
            contact: None,
            email_aliases: Vec::new(),
        });
    };
    let requested = get_contact(postgres, organization_id, requested_id).await?;
    let Some(requested) = requested else {
        return Ok(ExportPayload {
            requested_contact_id: subject.to_owned(),
            contact: None,
            email_aliases: Vec::new(),
        });
    };
    let canonical = if requested.canonical_contact_id == requested.contact_id {
        requested
    } else {
        get_contact(postgres, organization_id, requested.canonical_contact_id)
            .await?
            .ok_or_else(|| StorageError::InvalidStoredData {
                detail: "merged contact has no canonical target".to_owned(),
            })?
    };
    let aliases = sqlx::query_scalar::<_, String>(
        "SELECT email_normalized FROM customer_contact_emails WHERE organization_id=$1 AND contact_id=$2 ORDER BY email_normalized",
    )
    .bind(organization_id)
    .bind(canonical.contact_id)
    .fetch_all(postgres.pool())
    .await
    .map_err(|source| database("export customer contact aliases", source))?;
    Ok(ExportPayload {
        requested_contact_id: subject.to_owned(),
        contact: Some(canonical),
        email_aliases: aliases,
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn apply_retention(
    postgres: &OwnedPostgres,
    action_id: &str,
    request_hash: &[u8],
    organization_id: &str,
    subject: &str,
    delete: bool,
) -> Result<Result<String, DomainFailure>, StorageError> {
    let mut transaction = begin(postgres, "begin customer retention").await?;
    advisory_lock(&mut transaction, &format!("retention\u{0}{action_id}")).await?;
    let existing = sqlx::query(
        "SELECT request_hash,receipt FROM customer_directory_retention_receipts WHERE action_id=$1",
    )
    .bind(action_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| database("read customer retention receipt", source))?;
    if let Some(row) = existing {
        let stored_hash: Vec<u8> = row
            .try_get("request_hash")
            .map_err(|source| database("decode retention request hash", source))?;
        if stored_hash != request_hash {
            return Ok(Err(DomainFailure::RetentionConflict));
        }
        let receipt = row
            .try_get("receipt")
            .map_err(|source| database("decode retention receipt", source))?;
        commit(transaction, "commit customer retention replay").await?;
        return Ok(Ok(receipt));
    }

    advisory_lock(
        &mut transaction,
        &format!("contact-organization\u{0}{organization_id}"),
    )
    .await?;

    let mut matched = false;
    if let Ok(requested_id) = Uuid::parse_str(subject) {
        let requested = sqlx::query(
            "SELECT * FROM customer_contacts WHERE organization_id=$1 AND contact_id=$2 FOR UPDATE",
        )
        .bind(organization_id)
        .bind(requested_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| database("lock retained contact", source))?;
        if let Some(row) = requested {
            matched = true;
            let requested = decode_contact(&row)?;
            let canonical_id = requested.canonical_contact_id;
            let family_rows = sqlx::query(
                "SELECT * FROM customer_contacts WHERE organization_id=$1 AND (contact_id=$2 OR canonical_contact_id=$2) ORDER BY contact_id FOR UPDATE",
            )
            .bind(organization_id)
            .bind(canonical_id)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|source| database("lock canonical contact family", source))?;
            let family_ids = family_rows
                .iter()
                .map(decode_contact)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|contact| contact.contact_id)
                .collect::<Vec<_>>();
            sqlx::query(
                "DELETE FROM customer_contact_emails WHERE organization_id=$1 AND contact_id=$2",
            )
            .bind(organization_id)
            .bind(canonical_id)
            .execute(&mut *transaction)
            .await
            .map_err(|source| database("erase retained contact aliases", source))?;
            let canonical_redaction = format!("redacted-{canonical_id}@privacy.invalid");
            let active = sqlx::query(
                "UPDATE customer_contacts SET primary_email=$3,display_name=NULL,revision=revision+1,updated_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND contact_id=$2 AND state='active' RETURNING contact_id",
            )
            .bind(organization_id)
            .bind(canonical_id)
            .bind(&canonical_redaction)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|source| database("redact canonical contact", source))?;
            if active.is_none() {
                return Err(StorageError::InvalidStoredData {
                    detail: "canonical contact is not active".to_owned(),
                });
            }
            sqlx::query(
                "UPDATE customer_contacts SET primary_email='merged-' || contact_id::text || '@redacted.invalid',display_name=NULL,revision=revision+1,updated_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND state='merged' AND canonical_contact_id=$2",
            )
            .bind(organization_id)
            .bind(canonical_id)
            .execute(&mut *transaction)
            .await
            .map_err(|source| database("redact merged contact family", source))?;
            sqlx::query(
                "INSERT INTO customer_contact_emails(organization_id,email_normalized,contact_id) VALUES($1,$2,$3)",
            )
            .bind(organization_id)
            .bind(canonical_redaction)
            .bind(canonical_id)
            .execute(&mut *transaction)
            .await
            .map_err(|source| database("store retained contact alias", source))?;
            let current_rows = sqlx::query(
                "SELECT * FROM customer_contacts WHERE organization_id=$1 AND contact_id=ANY($2) ORDER BY contact_id",
            )
            .bind(organization_id)
            .bind(&family_ids)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|source| database("read redacted contact family", source))?;
            let current_contacts = current_rows
                .iter()
                .map(decode_contact)
                .collect::<Result<Vec<_>, _>>()?;
            sanitize_command_receipts(
                &mut transaction,
                organization_id,
                &family_ids,
                &current_contacts,
            )
            .await?;
        }
    }

    let outcome = if matched {
        if delete { "deleted" } else { "anonymized" }
    } else {
        "no-match"
    };
    let receipt = format!("customer-directory:{action_id}:{outcome}");
    sqlx::query(
        "INSERT INTO customer_directory_retention_receipts(action_id,request_hash,receipt) VALUES($1,$2,$3)",
    )
    .bind(action_id)
    .bind(request_hash)
    .bind(&receipt)
    .execute(&mut *transaction)
    .await
    .map_err(|source| database("store customer retention receipt", source))?;
    commit(transaction, "commit customer retention").await?;
    Ok(Ok(receipt))
}

async fn begin<'a>(
    postgres: &'a OwnedPostgres,
    operation: &'static str,
) -> Result<Transaction<'a, Postgres>, StorageError> {
    postgres
        .pool()
        .begin()
        .await
        .map_err(|source| database(operation, source))
}

async fn commit(
    transaction: Transaction<'_, Postgres>,
    operation: &'static str,
) -> Result<(), StorageError> {
    transaction
        .commit()
        .await
        .map_err(|source| database(operation, source))
}

async fn command_replay<T: DeserializeOwned>(
    transaction: &mut Transaction<'_, Postgres>,
    caller: &str,
    key: &str,
    operation: &str,
    request_hash: &[u8],
) -> Result<Result<Option<T>, DomainFailure>, StorageError> {
    advisory_lock(transaction, &format!("{caller}\u{0}{key}")).await?;
    let row = sqlx::query(
        "SELECT operation,request_hash,response FROM customer_directory_commands WHERE caller_instance=$1 AND idempotency_key=$2",
    )
    .bind(caller)
    .bind(key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| database("read customer directory command", source))?;
    let Some(row) = row else {
        return Ok(Ok(None));
    };
    let stored_operation: String = row
        .try_get("operation")
        .map_err(|source| database("decode command operation", source))?;
    let stored_hash: Vec<u8> = row
        .try_get("request_hash")
        .map_err(|source| database("decode command request hash", source))?;
    if stored_operation != operation || stored_hash != request_hash {
        return Ok(Err(DomainFailure::IdempotencyConflict));
    }
    let Json(response): Json<Value> = row
        .try_get("response")
        .map_err(|source| database("decode command response", source))?;
    Ok(Ok(Some(serde_json::from_value(response)?)))
}

async fn save_command<T: Serialize>(
    transaction: &mut Transaction<'_, Postgres>,
    caller: &str,
    key: &str,
    organization_id: &str,
    operation: &str,
    request_hash: &[u8],
    response: &T,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO customer_directory_commands(caller_instance,idempotency_key,operation,request_hash,organization_id,response) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(caller)
    .bind(key)
    .bind(operation)
    .bind(request_hash)
    .bind(organization_id)
    .bind(Json(serde_json::to_value(response)?))
    .execute(&mut **transaction)
    .await
    .map_err(|source| database("store customer directory command", source))?;
    Ok(())
}

async fn sanitize_command_receipts(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    family_ids: &[Uuid],
    current_contacts: &[ContactView],
) -> Result<(), StorageError> {
    let family_strings = family_ids.iter().map(Uuid::to_string).collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT caller_instance,idempotency_key,response FROM customer_directory_commands WHERE organization_id=$1 AND (response->'contact'->>'contact_id'=ANY($2) OR response->'source_contact'->>'contact_id'=ANY($2) OR response->'target_contact'->>'contact_id'=ANY($2)) FOR UPDATE",
    )
    .bind(organization_id)
    .bind(&family_strings)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|source| database("lock personal command receipts", source))?;
    for row in rows {
        let caller: String = row
            .try_get("caller_instance")
            .map_err(|source| database("decode receipt caller", source))?;
        let key: String = row
            .try_get("idempotency_key")
            .map_err(|source| database("decode receipt key", source))?;
        let Json(mut response): Json<Value> = row
            .try_get("response")
            .map_err(|source| database("decode personal command response", source))?;
        replace_contact(&mut response, "contact", current_contacts)?;
        replace_contact(&mut response, "source_contact", current_contacts)?;
        replace_contact(&mut response, "target_contact", current_contacts)?;
        sqlx::query(
            "UPDATE customer_directory_commands SET response=$3 WHERE caller_instance=$1 AND idempotency_key=$2",
        )
        .bind(caller)
        .bind(key)
        .bind(Json(response))
        .execute(&mut **transaction)
        .await
        .map_err(|source| database("sanitize personal command receipt", source))?;
    }
    Ok(())
}

fn replace_contact(
    response: &mut Value,
    field: &str,
    current_contacts: &[ContactView],
) -> Result<(), StorageError> {
    let Some(contact) = response.get_mut(field) else {
        return Ok(());
    };
    let Some(contact_id) = contact.get("contact_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(current) = current_contacts
        .iter()
        .find(|current| current.contact_id.to_string() == contact_id)
    else {
        return Ok(());
    };
    *contact = serde_json::to_value(current)?;
    Ok(())
}

async fn advisory_lock(
    transaction: &mut Transaction<'_, Postgres>,
    value: &str,
) -> Result<(), StorageError> {
    let digest = Sha256::digest(value.as_bytes());
    let lock_key = i64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 always contains eight lock-key bytes"),
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut **transaction)
        .await
        .map_err(|source| database("acquire customer directory lock", source))?;
    Ok(())
}

fn decode_contact(row: &sqlx::postgres::PgRow) -> Result<ContactView, StorageError> {
    let contact_id = row
        .try_get("contact_id")
        .map_err(|source| database("decode contact id", source))?;
    let canonical_contact_id = row
        .try_get("canonical_contact_id")
        .map_err(|source| database("decode canonical contact id", source))?;
    let state: String = row
        .try_get("state")
        .map_err(|source| database("decode contact state", source))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|source| database("decode contact revision", source))?;
    if revision <= 0
        || !matches!(state.as_str(), "active" | "merged")
        || (state == "active" && canonical_contact_id != contact_id)
        || (state == "merged" && canonical_contact_id == contact_id)
    {
        return Err(StorageError::InvalidStoredData {
            detail: "contact state, canonical id, or revision is inconsistent".to_owned(),
        });
    }
    Ok(ContactView {
        contact_id,
        organization_id: row
            .try_get("organization_id")
            .map_err(|source| database("decode contact organization", source))?,
        email: row
            .try_get("primary_email")
            .map_err(|source| database("decode contact email", source))?,
        display_name: row
            .try_get("display_name")
            .map_err(|source| database("decode contact display name", source))?,
        state,
        canonical_contact_id,
        revision,
        created_at: row
            .try_get("created_at")
            .map_err(|source| database("decode contact creation time", source))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|source| database("decode contact update time", source))?,
    })
}

fn database(operation: &'static str, source: sqlx::Error) -> StorageError {
    StorageError::Database { operation, source }
}

mod decimal_i64 {
    use serde::{Deserialize as _, Deserializer, Serializer};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub(super) fn serialize<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<i64, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_payloads_preserve_decimal_revisions() {
        let now = OffsetDateTime::now_utc();
        let result = ResolveResult {
            contact: ContactView {
                contact_id: Uuid::nil(),
                organization_id: "org_1".to_owned(),
                email: "customer@example.com".to_owned(),
                display_name: Some("Customer".to_owned()),
                state: "active".to_owned(),
                canonical_contact_id: Uuid::nil(),
                revision: 7,
                created_at: now,
                updated_at: now,
            },
            created: true,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["contact"]["revision"], json!("7"));
        assert_eq!(
            serde_json::from_value::<ResolveResult>(value).unwrap(),
            result
        );
    }

    #[test]
    fn receipt_contact_replacement_uses_the_current_redacted_view() {
        let now = OffsetDateTime::now_utc();
        let id = Uuid::new_v4();
        let current = ContactView {
            contact_id: id,
            organization_id: "org_1".to_owned(),
            email: format!("redacted-{id}@privacy.invalid"),
            display_name: None,
            state: "active".to_owned(),
            canonical_contact_id: id,
            revision: 2,
            created_at: now,
            updated_at: now,
        };
        let mut response = json!({
            "contact": {
                "contact_id": id,
                "organization_id": "org_1",
                "email": "customer@example.com",
                "display_name": "Customer",
                "state": "active",
                "canonical_contact_id": id,
                "revision": "1",
                "created_at": now,
                "updated_at": now
            },
            "created": true
        });
        replace_contact(&mut response, "contact", std::slice::from_ref(&current)).unwrap();
        assert_eq!(response["contact"], serde_json::to_value(current).unwrap());
    }
}
