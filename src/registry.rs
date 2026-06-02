use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A session's self-reported state, written to `~/.til/sessions/<id>.json` by the
/// running session and read back by `mock-terminal ps`. The session is the
/// authoritative source: it records its own pid, name, and status as it runs, so
/// the tracker never has to guess from filenames or scrape logs. `ps` reconciles
/// these records against live pids — a `running` record whose pid is gone is
/// reported as `dead` (the window was closed or the process crashed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    /// "open" (launched from a manifest) or "interactive".
    pub kind: String,
    /// Manifest path/URL for `open` sessions; empty otherwise.
    pub source: String,
    pub pid: u32,
    pub started_ms: u128,
    pub updated_ms: u128,
    /// "running" while the session is live; "done" once it exits cleanly. `ps`
    /// derives "dead" at read time when status is "running" but the pid is gone.
    pub status: String,
    /// A short hint of the most recent activity (the last logged event kind).
    pub last_event: String,
}

impl SessionRecord {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "source": self.source,
            "pid": self.pid,
            "started_ms": self.started_ms as u64,
            "updated_ms": self.updated_ms as u64,
            "status": self.status,
            "last_event": self.last_event,
        })
    }

    /// Parse a record from its JSON file contents. Returns `None` if the file is
    /// malformed or missing required fields, so one corrupt file can't break `ps`.
    pub fn from_json(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
        Some(SessionRecord {
            id: s("id")?,
            name: s("name").unwrap_or_default(),
            kind: s("kind").unwrap_or_default(),
            source: s("source").unwrap_or_default(),
            pid: n("pid")? as u32,
            started_ms: n("started_ms")? as u128,
            updated_ms: n("updated_ms").unwrap_or(0) as u128,
            status: s("status").unwrap_or_else(|| "running".to_string()),
            last_event: s("last_event").unwrap_or_default(),
        })
    }
}

/// A live handle to this process's session record. Created at session start
/// (status "running"); `touch` updates the last-event/timestamp as the session
/// works; `Drop` marks it "done" so a cleanly-exited session reports correctly.
pub struct SessionHandle {
    path: Option<PathBuf>,
    rec: Mutex<SessionRecord>,
}

impl SessionHandle {
    /// Register a new session. Best-effort: if the registry dir is unusable the
    /// handle degrades to a no-op (like the trajectory logger) so tracking can
    /// never take down the session.
    pub fn new(name: &str, kind: &str, source: &str) -> Self {
        let started = unix_millis();
        let pid = std::process::id();
        let id = format!("{started}-{pid}");
        let rec = SessionRecord {
            id: id.clone(),
            name: name.to_string(),
            kind: kind.to_string(),
            source: source.to_string(),
            pid,
            started_ms: started,
            updated_ms: started,
            status: "running".to_string(),
            last_event: "launched".to_string(),
        };
        let path = sessions_dir().and_then(|dir| {
            let _ = fs::create_dir_all(&dir);
            let p = dir.join(format!("{id}.json"));
            write_record(&p, &rec).ok().map(|_| p)
        });
        Self {
            path,
            rec: Mutex::new(rec),
        }
    }

    /// Note recent activity (the last logged event kind) and bump the timestamp.
    pub fn touch(&self, event: &str) {
        self.mutate(|r| {
            r.last_event = event.to_string();
            r.updated_ms = unix_millis();
        });
    }

    /// Mark the session terminal with the given status ("done" on clean exit).
    fn set_status(&self, status: &str) {
        self.mutate(|r| {
            r.status = status.to_string();
            r.updated_ms = unix_millis();
        });
    }

    fn mutate(&self, f: impl FnOnce(&mut SessionRecord)) {
        let Some(path) = &self.path else { return };
        let Ok(mut guard) = self.rec.lock() else { return };
        f(&mut guard);
        // Rewrite the whole (small) record; failures are swallowed — tracking
        // must never crash the session.
        let _ = write_record(path, &guard);
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // A clean exit (handle dropped while still "running") becomes "done".
        // If the process is killed instead, Drop never runs and the record stays
        // "running" with a dead pid — which `list` reconciles to "dead".
        let still_running = self
            .rec
            .lock()
            .map(|r| r.status == "running")
            .unwrap_or(false);
        if still_running {
            self.set_status("done");
        }
    }
}

/// List all known sessions, newest first, reconciled against live pids: a record
/// claiming "running" whose pid is gone is reported as "dead".
pub fn list() -> Vec<SessionRecord> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut records: Vec<SessionRecord> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|t| SessionRecord::from_json(&t))
        .map(reconcile)
        .collect();
    records.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
    records
}

/// Reconcile a record's claimed status against reality: a "running" record whose
/// pid is no longer alive is reported as "dead" (closed window / crash).
fn reconcile(mut r: SessionRecord) -> SessionRecord {
    if r.status == "running" && !pid_alive(r.pid) {
        r.status = "dead".to_string();
    }
    r
}

/// True if `pid` is a live process. Uses `kill(pid, 0)`: 0 means it exists and we
/// may signal it; we treat anything else as not-alive for our own children.
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Render the session table for `ps`. Pure (records + a "now" timestamp in →
/// string out) so it can be unit-tested without the filesystem or a clock.
pub fn render_table(records: &[SessionRecord], now_ms: u128) -> String {
    if records.is_empty() {
        return "No sessions. Launch one with `open <manifest>` or `scripts/spawn.sh`.\n"
            .to_string();
    }
    let mut s = String::new();
    s.push_str(&format!(
        "{:<24} {:<8} {:<8} {:<9} {}\n",
        "SESSION", "STATE", "PID", "AGE", "LAST"
    ));
    for r in records {
        let name = if r.name.is_empty() {
            "(unnamed)"
        } else {
            &r.name
        };
        s.push_str(&format!(
            "{:<24} {:<8} {:<8} {:<9} {}\n",
            truncate(name, 24),
            r.status,
            r.pid,
            age(now_ms, r.started_ms),
            r.last_event,
        ));
    }
    s
}

/// Human-readable elapsed time since `started_ms`, e.g. "12s", "3m", "2h".
fn age(now_ms: u128, started_ms: u128) -> String {
    let secs = now_ms.saturating_sub(started_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn write_record(path: &PathBuf, rec: &SessionRecord) -> std::io::Result<()> {
    fs::write(path, format!("{}\n", rec.to_json()))
}

/// `~/.til/sessions`, with the `~/.til` base overridable via `MT_TIL_DIR` (tests
/// point this at a temp dir so they never touch the real `$HOME`).
fn sessions_dir() -> Option<PathBuf> {
    let base = if let Some(d) = std::env::var_os("MT_TIL_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".til")
    };
    Some(base.join("sessions"))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // MT_TIL_DIR is process-global, so tests that set it must not run
    // concurrently or they clobber each other's dir. Serialise them on one lock
    // held for the lifetime of the guard.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct DirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        dir: PathBuf,
    }
    impl DirGuard {
        fn new(name: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("MT_TIL_DIR");
            let dir = std::env::temp_dir().join(format!(
                "til-reg-{}-{}-{}",
                name,
                std::process::id(),
                unix_millis()
            ));
            std::env::set_var("MT_TIL_DIR", &dir);
            Self {
                _lock: lock,
                prev,
                dir,
            }
        }
    }
    impl Drop for DirGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("MT_TIL_DIR", v),
                None => std::env::remove_var("MT_TIL_DIR"),
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn record_roundtrips_through_json() {
        let r = SessionRecord {
            id: "100-7".into(),
            name: "demo".into(),
            kind: "open".into(),
            source: "/x/session.json".into(),
            pid: 7,
            started_ms: 100,
            updated_ms: 150,
            status: "running".into(),
            last_event: "decision".into(),
        };
        let back = SessionRecord::from_json(&r.to_json().to_string()).expect("parse");
        assert_eq!(back, r);
    }

    #[test]
    fn from_json_rejects_garbage() {
        assert!(SessionRecord::from_json("not json").is_none());
        assert!(SessionRecord::from_json("{}").is_none()); // missing id/pid
    }

    #[test]
    fn handle_registers_and_lists_running() {
        let _g = DirGuard::new("running");
        let h = SessionHandle::new("my session", "open", "/x.json");
        h.touch("command");
        let listed = list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "my session");
        assert_eq!(listed[0].kind, "open");
        // Our own pid is alive, so a running record stays running.
        assert_eq!(listed[0].status, "running");
        assert_eq!(listed[0].last_event, "command");
        assert_eq!(listed[0].pid, std::process::id());
    }

    #[test]
    fn drop_marks_done() {
        let _g = DirGuard::new("done");
        {
            let _h = SessionHandle::new("ends", "interactive", "");
        } // dropped here → "done"
        let listed = list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "done");
    }

    #[test]
    fn dead_pid_running_record_reconciles_to_dead() {
        // A record claiming "running" under a pid that cannot be alive (pid 1 is
        // init/launchd — kill(1,0) fails with EPERM for us, so pid_alive is
        // false; use a very high pid that surely doesn't exist instead).
        let r = SessionRecord {
            id: "1-999999999".into(),
            name: "ghost".into(),
            kind: "open".into(),
            source: String::new(),
            pid: 999_999_999,
            started_ms: 1,
            updated_ms: 1,
            status: "running".into(),
            last_event: "launched".into(),
        };
        assert_eq!(reconcile(r).status, "dead");
    }

    #[test]
    fn render_table_shows_columns_and_empty_state() {
        assert!(render_table(&[], 0).contains("No sessions"));
        let r = SessionRecord {
            id: "1000-5".into(),
            name: "fix flaky test".into(),
            kind: "open".into(),
            source: String::new(),
            pid: 5,
            started_ms: 1000,
            updated_ms: 1000,
            status: "running".into(),
            last_event: "decision".into(),
        };
        let table = render_table(&[r], 1000 + 65_000);
        assert!(table.contains("SESSION"));
        assert!(table.contains("fix flaky test"));
        assert!(table.contains("running"));
        assert!(table.contains("1m"), "age formatted: {table}"); // 65s → 1m
        assert!(table.contains("decision"));
    }

    #[test]
    fn age_formats_units() {
        assert_eq!(age(5_000, 0), "5s");
        assert_eq!(age(120_000, 0), "2m");
        assert_eq!(age(7_200_000, 0), "2h");
    }
}
