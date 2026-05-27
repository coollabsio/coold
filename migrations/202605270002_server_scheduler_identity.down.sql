DROP INDEX IF EXISTS idx_servers_host_id;
CREATE TABLE servers_new (
    id TEXT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    address TEXT NOT NULL DEFAULT '',
    mgmt_ip TEXT,
    status TEXT NOT NULL DEFAULT 'unknown',
    coold_version TEXT,
    created_at TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT ''
);
INSERT INTO servers_new (id,name,address,mgmt_ip,status,coold_version,created_at,updated_at)
SELECT id,name,address,mgmt_ip,status,coold_version,created_at,updated_at FROM servers;
DROP TABLE servers;
ALTER TABLE servers_new RENAME TO servers;
