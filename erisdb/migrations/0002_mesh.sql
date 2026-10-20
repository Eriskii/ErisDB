-- ErisDB in its owner's mesh. A device approves ErisDB to serve `erisdb`; this
-- database keeps that note and ErisDB's copy of the device list. Apps get in
-- through a device, which enrolls them with the passphrase kept here, and an
-- installation is an app key a device's note approved.

DELETE FROM items WHERE facet = 'pair';
DELETE FROM changes WHERE facet = 'pair';
DELETE FROM items WHERE facet = 'facet' AND body ->> 'name' = 'pair';

DROP TABLE clients;
CREATE TABLE clients (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    -- The app's Iroh key, the device that approved it, and the note it did so with.
    app TEXT NOT NULL CHECK (app ~ '^[0-9a-f]{64}$'),
    device TEXT NOT NULL CHECK (device ~ '^[0-9a-f]{64}$'),
    note TEXT NOT NULL CHECK (note ~ '^[0-9a-f]{64}$'),
    -- What the note approved, and what the installation holds now: never more.
    approved TEXT[] NOT NULL,
    grants TEXT[] NOT NULL,
    revoked_at TIMESTAMPTZ,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX clients_active_app_idx ON clients (app) WHERE revoked_at IS NULL;
CREATE INDEX clients_active_device_idx ON clients (device) WHERE revoked_at IS NULL;

-- One row: ErisDB's own note, and what its mesh node keeps between runs.
CREATE TABLE mesh (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    note JSONB NOT NULL,
    entries JSONB NOT NULL DEFAULT '[]',
    directory JSONB NOT NULL DEFAULT '[]',
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row: the enrollment passphrase as an Argon2id PHC string.
CREATE TABLE passphrase (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    hash TEXT NOT NULL,
    set_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Wrong guesses per device since its last right one, and until when it waits.
CREATE TABLE passphrase_guesses (
    device TEXT PRIMARY KEY CHECK (device ~ '^[0-9a-f]{64}$'),
    wrong INTEGER NOT NULL DEFAULT 0,
    locked_until TIMESTAMPTZ
);
