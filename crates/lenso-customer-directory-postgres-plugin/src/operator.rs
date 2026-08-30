use lenso_postgres_kit::{PostgresKitError, SchemaOperator, SetupOutcome, UpgradeOutcome};
use thiserror::Error;

use crate::schema::schema_plan;

/// Explicit schema administration for Customer Directory storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct CustomerDirectoryOperator;

impl CustomerDirectoryOperator {
    /// Creates the owned schema and installs the authored migration plan.
    pub async fn setup(
        database_url: &str,
        schema: &str,
    ) -> Result<SetupOutcome, CustomerDirectoryOperatorError> {
        Ok(SchemaOperator::connect(database_url, schema_plan(schema)?)
            .await?
            .setup()
            .await?)
    }

    /// Applies pending authored migrations to the owned schema.
    pub async fn upgrade(
        database_url: &str,
        schema: &str,
    ) -> Result<UpgradeOutcome, CustomerDirectoryOperatorError> {
        Ok(SchemaOperator::connect(database_url, schema_plan(schema)?)
            .await?
            .upgrade()
            .await?)
    }
}

/// Failure from an explicit Customer Directory schema workflow.
#[derive(Debug, Error)]
pub enum CustomerDirectoryOperatorError {
    #[error(transparent)]
    Plan(#[from] lenso_postgres_kit::PlanError),
    #[error(transparent)]
    Postgres(#[from] PostgresKitError),
}
