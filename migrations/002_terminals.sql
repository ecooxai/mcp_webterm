CREATE TABLE IF NOT EXISTS terminals (
    id            INTEGER PRIMARY KEY,
    workspace_id  INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    name          TEXT NOT NULL COLLATE NOCASE,
    tmux_session  TEXT NOT NULL UNIQUE,
    status        TEXT NOT NULL CHECK (status IN ('starting', 'running', 'stopped')),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (workspace_id, name)
);

CREATE INDEX IF NOT EXISTS terminals_workspace_idx ON terminals(workspace_id, name);
