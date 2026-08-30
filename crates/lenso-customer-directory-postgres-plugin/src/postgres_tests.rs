use lenso_postgres_kit::OwnedPostgres;
use sqlx::{AssertSqlSafe, Executor as _};
use uuid::Uuid;

use crate::{CustomerDirectoryOperator, schema, storage};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolution_replay_merge_and_retention_are_postgres_durable() {
    let Ok(database_url) = std::env::var("LENSO_CUSTOMER_DIRECTORY_TEST_DATABASE_URL") else {
        eprintln!(
            "skipping PostgreSQL acceptance; LENSO_CUSTOMER_DIRECTORY_TEST_DATABASE_URL is unset"
        );
        return;
    };
    let schema_name = format!("customer_directory_test_{}", Uuid::new_v4().simple());
    CustomerDirectoryOperator::setup(&database_url, &schema_name)
        .await
        .unwrap();
    let postgres = OwnedPostgres::prepare(
        &database_url,
        schema::schema_plan(schema_name.clone()).unwrap(),
    )
    .await
    .unwrap();

    let first = storage::resolve_or_create_email_contact(
        &postgres,
        "support-email-a",
        "message-1",
        &[1],
        "org_1",
        "customer@example.com",
        Some("Customer"),
    );
    let second = storage::resolve_or_create_email_contact(
        &postgres,
        "support-email-b",
        "message-2",
        &[2],
        "org_1",
        "customer@example.com",
        None,
    );
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_eq!(first.contact.contact_id, second.contact.contact_id);
    assert_ne!(first.created, second.created);

    postgres.pool().close().await;
    let restarted = OwnedPostgres::prepare(
        &database_url,
        schema::schema_plan(schema_name.clone()).unwrap(),
    )
    .await
    .unwrap();
    let replayed = storage::resolve_or_create_email_contact(
        &restarted,
        "support-email-a",
        "message-1",
        &[1],
        "org_1",
        "customer@example.com",
        Some("Customer"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(replayed, first);
    assert_eq!(
        storage::resolve_or_create_email_contact(
            &restarted,
            "support-email-a",
            "message-1",
            &[9],
            "org_1",
            "customer@example.com",
            Some("Other"),
        )
        .await
        .unwrap(),
        Err(storage::DomainFailure::IdempotencyConflict)
    );

    let target = storage::resolve_or_create_email_contact(
        &restarted,
        "support-email-a",
        "message-3",
        &[3],
        "org_1",
        "canonical@example.com",
        Some("Canonical"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        storage::merge_contact(
            &restarted,
            "support-admin",
            "merge-stale",
            &[8],
            "org_1",
            first.contact.contact_id,
            target.contact.contact_id,
            2,
            1,
        )
        .await
        .unwrap(),
        Err(storage::DomainFailure::RevisionConflict)
    );
    let merged = storage::merge_contact(
        &restarted,
        "support-admin",
        "merge-1",
        &[4],
        "org_1",
        first.contact.contact_id,
        target.contact.contact_id,
        1,
        1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(merged.source_contact.state, "merged");
    assert_eq!(
        merged.source_contact.canonical_contact_id,
        target.contact.contact_id
    );
    assert_eq!(merged.target_contact.revision, 2);
    let merge_replay = storage::merge_contact(
        &restarted,
        "support-admin",
        "merge-1",
        &[4],
        "org_1",
        first.contact.contact_id,
        target.contact.contact_id,
        1,
        1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(merge_replay, merged);
    assert_eq!(
        storage::merge_contact(
            &restarted,
            "support-admin",
            "merge-1",
            &[9],
            "org_1",
            first.contact.contact_id,
            target.contact.contact_id,
            1,
            1,
        )
        .await
        .unwrap(),
        Err(storage::DomainFailure::IdempotencyConflict)
    );
    let old_email = storage::resolve_or_create_email_contact(
        &restarted,
        "support-email-b",
        "message-4",
        &[5],
        "org_1",
        "customer@example.com",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!old_email.created);
    assert_eq!(old_email.contact.contact_id, target.contact.contact_id);
    let export =
        storage::export_subject(&restarted, "org_1", &first.contact.contact_id.to_string())
            .await
            .unwrap();
    assert_eq!(
        export.contact.as_ref().unwrap().contact_id,
        target.contact.contact_id
    );
    assert_eq!(
        export.email_aliases,
        vec![
            "canonical@example.com".to_owned(),
            "customer@example.com".to_owned()
        ]
    );

    let receipt = storage::apply_retention(
        &restarted,
        "retention-1",
        &[6],
        "org_1",
        &first.contact.contact_id.to_string(),
        true,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(receipt, "customer-directory:retention-1:deleted");
    assert_eq!(
        storage::apply_retention(
            &restarted,
            "retention-1",
            &[6],
            "org_1",
            &first.contact.contact_id.to_string(),
            true,
        )
        .await
        .unwrap()
        .unwrap(),
        receipt
    );
    assert_eq!(
        storage::apply_retention(
            &restarted,
            "retention-1",
            &[9],
            "org_1",
            &first.contact.contact_id.to_string(),
            true,
        )
        .await
        .unwrap(),
        Err(storage::DomainFailure::RetentionConflict)
    );
    let redacted_replay = storage::resolve_or_create_email_contact(
        &restarted,
        "support-email-a",
        "message-1",
        &[1],
        "org_1",
        "customer@example.com",
        Some("Customer"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(redacted_replay.contact.state, "merged");
    assert!(redacted_replay.contact.email.ends_with(".invalid"));
    assert_eq!(redacted_replay.contact.display_name, None);
    let redacted_export =
        storage::export_subject(&restarted, "org_1", &first.contact.contact_id.to_string())
            .await
            .unwrap();
    assert!(
        redacted_export
            .contact
            .as_ref()
            .unwrap()
            .email
            .ends_with(".invalid")
    );
    assert_eq!(redacted_export.email_aliases.len(), 1);
    assert!(redacted_export.email_aliases[0].ends_with(".invalid"));
    let after_retention = storage::resolve_or_create_email_contact(
        &restarted,
        "support-email-a",
        "message-5",
        &[7],
        "org_1",
        "customer@example.com",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(after_retention.created);
    assert_ne!(
        after_retention.contact.contact_id,
        target.contact.contact_id
    );

    restarted.pool().close().await;
    let cleanup = sqlx::PgPool::connect(&database_url).await.unwrap();
    cleanup
        .execute(AssertSqlSafe(format!(
            "DROP SCHEMA \"{schema_name}\" CASCADE"
        )))
        .await
        .unwrap();
    cleanup.close().await;
}
