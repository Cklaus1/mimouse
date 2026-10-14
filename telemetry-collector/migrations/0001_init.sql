-- Schema version 1 storage. received_at is the server clock in Unix seconds;
-- rows older than 90 days are deleted by the cron trigger. No column holds
-- client IP addresses or request headers.

-- One row per install and UTC day; data is the validated day object as JSON.
CREATE TABLE days (
  install_id TEXT NOT NULL,
  date TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  version TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  os_family TEXT NOT NULL,
  os_version TEXT,
  desktop TEXT,
  session_type TEXT,
  data TEXT NOT NULL,
  PRIMARY KEY (install_id, date)
);
CREATE INDEX days_received_at ON days (received_at);

CREATE TABLE crashes (
  id INTEGER PRIMARY KEY,
  install_id TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  version TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  os_family TEXT NOT NULL,
  os_version TEXT,
  desktop TEXT,
  session_type TEXT,
  location TEXT NOT NULL,
  message TEXT NOT NULL,
  capture_backend TEXT,
  emulation_backend TEXT
);
CREATE INDEX crashes_install_id ON crashes (install_id);
CREATE INDEX crashes_received_at ON crashes (received_at);

-- Accepted batches per install and UTC day, for the rate limit.
CREATE TABLE rate_limits (
  install_id TEXT NOT NULL,
  day TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (install_id, day)
);
