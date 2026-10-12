use crate::model::{DiagnosticBundle, DiagnosticEntry, Host};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::collections::VecDeque;

const ENTRY_LIMIT: usize = 256;
const RETENTION_HOURS: i64 = 24;

pub struct Diagnostics {
    entries: VecDeque<DiagnosticEntry>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(ENTRY_LIMIT),
        }
    }

    pub fn record(&mut self, mut entry: DiagnosticEntry) {
        let now = Utc::now();
        // The record type is the allowlist: no message, prompt, path, debug or terminal fields.
        let at = match DateTime::parse_from_rfc3339(&entry.at) {
            Ok(at) => at.with_timezone(&Utc),
            Err(_) => return,
        };
        if !retained(at, now) {
            return;
        }
        entry.at = at.to_rfc3339_opts(SecondsFormat::Millis, true);
        self.entries
            .retain(|entry| timestamp(entry).is_some_and(|at| retained(at, now)));
        if self.entries.len() == ENTRY_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn bundle(&self, include_hosts: Vec<Host>) -> DiagnosticBundle {
        let now = Utc::now();
        DiagnosticBundle {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            entries: self
                .entries
                .iter()
                .filter(|e| timestamp(e).is_some_and(|at| retained(at, now)))
                .cloned()
                .collect(),
            // Glue passes [] by default; metadata appears only after explicit user opt-in.
            hosts: include_hosts,
        }
    }
}

fn timestamp(entry: &DiagnosticEntry) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&entry.at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn retained(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    at <= now && at >= now - Duration::hours(RETENTION_HOURS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ErrorCode, HostDraft, HostId, Stage};
    use uuid::Uuid;

    fn entry(at: DateTime<Utc>, exit_code: i32) -> DiagnosticEntry {
        DiagnosticEntry {
            at: at.to_rfc3339(),
            host_id: None,
            stage: Stage::Tcp,
            code: Some(ErrorCode::ConnectionFailed),
            exit_code: Some(exit_code),
        }
    }

    #[test]
    fn logs_bound_entries_and_drop_expired_invalid_and_future_timestamps() {
        let mut log = Diagnostics::new();
        let now = Utc::now() - Duration::seconds(1);
        log.record(entry(now - Duration::hours(25), -1));
        log.record(entry(now + Duration::hours(2), -2));
        let mut invalid = entry(now, -3);
        invalid.at = "credential-looking invalid timestamp".into();
        log.record(invalid);
        for exit in 0..300 {
            log.record(entry(now, exit));
        }
        let bundle = log.bundle(Vec::new());
        assert_eq!(
            bundle
                .entries
                .iter()
                .map(|e| e.exit_code.unwrap())
                .collect::<Vec<_>>(),
            (44..300).collect::<Vec<_>>()
        );
        assert!(bundle.hosts.is_empty());
    }

    #[test]
    fn host_metadata_is_only_in_explicit_opt_in_export() {
        let now = Utc::now() - Duration::seconds(1);
        let host = Host {
            id: HostId(Uuid::new_v4()),
            input: HostDraft {
                label: "private username metadata".into(),
                ssh_target: "privateuser@remote.example".into(),
                port_override: None,
                host_name_override: None,
                host_key_alias_override: None,
                tailnet_node_id: None,
            },
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        let mut log = Diagnostics::new();
        let mut event = entry(now, 255);
        event.host_id = Some(host.id);
        log.record(event);
        let default_export = serde_json::to_value(log.bundle(Vec::new())).unwrap();
        assert_eq!(default_export["hosts"], serde_json::json!([]));
        assert_eq!(default_export["entries"][0]["exitCode"], 255);
        assert!(!serde_json::to_string(&default_export)
            .unwrap()
            .contains("privateuser"));
        let opted_in = log.bundle(vec![host]);
        assert_eq!(
            opted_in.hosts[0].input.ssh_target,
            "privateuser@remote.example"
        );
    }
}
