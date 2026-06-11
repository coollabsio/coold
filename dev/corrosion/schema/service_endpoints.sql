CREATE TABLE IF NOT EXISTS service_endpoints (
  container_id TEXT PRIMARY KEY,
  container_name TEXT NOT NULL,
  namespace TEXT NOT NULL,
  host_mgmt_ip TEXT NOT NULL,
  container_ip TEXT NOT NULL,
  state TEXT NOT NULL,
  health TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
