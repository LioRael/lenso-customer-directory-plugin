CREATE TABLE customer_contacts (
    contact_id UUID PRIMARY KEY,
    organization_id TEXT NOT NULL CHECK (octet_length(organization_id) BETWEEN 1 AND 512),
    primary_email TEXT NOT NULL CHECK (octet_length(primary_email) BETWEEN 3 AND 320 AND primary_email = lower(primary_email)),
    display_name TEXT CHECK (display_name IS NULL OR octet_length(display_name) BETWEEN 1 AND 200),
    state TEXT NOT NULL CHECK (state IN ('active', 'merged')),
    canonical_contact_id UUID NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    merged_at TIMESTAMPTZ,
    UNIQUE (organization_id, contact_id),
    FOREIGN KEY (organization_id, canonical_contact_id)
        REFERENCES customer_contacts (organization_id, contact_id)
        DEFERRABLE INITIALLY DEFERRED,
    CHECK ((state = 'active' AND canonical_contact_id = contact_id AND merged_at IS NULL)
        OR (state = 'merged' AND canonical_contact_id <> contact_id AND merged_at IS NOT NULL))
);

CREATE INDEX customer_contacts_organization_idx
    ON customer_contacts (organization_id, updated_at DESC, contact_id DESC);
CREATE INDEX customer_contacts_canonical_idx
    ON customer_contacts (organization_id, canonical_contact_id);

CREATE TABLE customer_contact_emails (
    organization_id TEXT NOT NULL CHECK (octet_length(organization_id) BETWEEN 1 AND 512),
    email_normalized TEXT NOT NULL CHECK (octet_length(email_normalized) BETWEEN 3 AND 320 AND email_normalized = lower(email_normalized)),
    contact_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, email_normalized),
    FOREIGN KEY (organization_id, contact_id)
        REFERENCES customer_contacts (organization_id, contact_id)
        ON DELETE CASCADE
);

CREATE INDEX customer_contact_emails_contact_idx
    ON customer_contact_emails (contact_id, email_normalized);

CREATE TABLE customer_directory_commands (
    caller_instance TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_hash BYTEA NOT NULL,
    organization_id TEXT NOT NULL,
    response JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (caller_instance, idempotency_key)
);

CREATE INDEX customer_directory_commands_scope_idx
    ON customer_directory_commands (organization_id, created_at DESC);

CREATE TABLE customer_directory_retention_receipts (
    action_id TEXT PRIMARY KEY,
    request_hash BYTEA NOT NULL,
    receipt TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
