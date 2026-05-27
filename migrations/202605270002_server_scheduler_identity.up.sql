ALTER TABLE servers ADD COLUMN host_id TEXT;
ALTER TABLE servers ADD COLUMN capabilities TEXT NOT NULL DEFAULT '[]';
ALTER TABLE servers ADD COLUMN last_seen_at TEXT;
CREATE INDEX IF NOT EXISTS idx_servers_host_id ON servers(host_id);
