//! Journal JSON decoding and gap-free, bounded per-instance log delivery.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use lantern_core::{
    Timestamp,
    ids::InstanceId,
    model::{LogLine, Stream},
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;

use super::systemd::UnitNames;

/// Maximum UTF-8 bytes retained for a single log message.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// A decoded journal message and its cursor for follower resumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLine {
    /// Domain log entry mapped to its instance and stream.
    pub line: LogLine,
    /// Opaque journal cursor if present.
    pub cursor: Option<String>,
}

/// Error parsing a journal entry or constructing a journal command.
#[derive(Debug, Error)]
pub enum JournalError {
    /// The line was not JSON.
    #[error("invalid journal JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A required field was absent or invalid.
    #[error("invalid journal entry: {0}")]
    Invalid(&'static str),
}

/// Map an instance or setup/deploy operation unit to its instance and stream.
pub fn unit_stream(unit: &str, names: &UnitNames) -> Option<(InstanceId, Stream)> {
    let instance = |safe: &str| {
        let id = InstanceId::from_safe_name(safe).ok()?;
        (id.safe_name() == safe).then_some(id)
    };
    let unit = unit.strip_suffix(".service")?;
    if let Some(safe) = unit.strip_prefix(&names.instance_prefix) {
        return Some((instance(safe)?, Stream::Stdout));
    }
    let rest = unit.strip_prefix(&names.operation_prefix)?;
    for (verb, stream) in [("-setup-", Stream::Setup), ("-deploy-", Stream::System)] {
        if let Some((safe, seq)) = rest.rsplit_once(verb)
            && !seq.is_empty()
            && seq.bytes().all(|b| b.is_ascii_digit())
        {
            return Some((instance(safe)?, stream));
        }
    }
    None
}

/// Strip ANSI CSI/OSC and simple escape sequences, then keep the last progress line.
pub fn clean_message(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut clean = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            clean.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&ch) {
                        break;
                    }
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // Other control strings (DCS, PM, APC) terminate with ST.
            b'P' | b'^' | b'_' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    let text = String::from_utf8_lossy(&clean);
    let last = text.rsplit('\r').next().unwrap_or_default();
    truncate_line(last)
}

fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_owned();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

/// Parse one `journalctl -o json --all` record; unrelated units are ignored.
pub fn parse_line(input: &str, names: &UnitNames) -> Result<Option<JournalLine>, JournalError> {
    let value: Value = serde_json::from_str(input)?;
    let Some(unit) = value.get("_SYSTEMD_USER_UNIT").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some((instance, stream)) = unit_stream(unit, names) else {
        return Ok(None);
    };
    let msg = value
        .get("MESSAGE")
        .ok_or(JournalError::Invalid("missing MESSAGE"))?;
    let message = match msg {
        Value::String(text) => text.clone(),
        Value::Array(values) => {
            let bytes = values
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or(JournalError::Invalid("MESSAGE is not a byte array"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => return Err(JournalError::Invalid("MESSAGE is not a string or byte array")),
    };
    let micros = match value.get("__REALTIME_TIMESTAMP") {
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
    .ok_or(JournalError::Invalid("missing or invalid timestamp"))?;
    let ts =
        Timestamp::from_microsecond(micros).map_err(|_| JournalError::Invalid("timestamp out of range"))?;
    Ok(Some(JournalLine {
        line: LogLine {
            instance,
            stream,
            line: clean_message(&message),
            ts,
        },
        cursor: value.get("__CURSOR").and_then(Value::as_str).map(str::to_owned),
    }))
}

/// Build arguments for one slice-scoped journal follower, including future units.
pub fn follow_args(slice: &str, after_cursor: Option<&str>) -> Result<Vec<String>, JournalError> {
    if !slice.ends_with(".slice")
        || !slice
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(JournalError::Invalid("invalid slice"));
    }
    let mut args: Vec<String> = ["--user", "--follow", "--output=json", "--all", "--no-pager"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if let Some(cursor) = after_cursor {
        if cursor.contains('\0') {
            return Err(JournalError::Invalid("invalid cursor"));
        }
        args.push(format!("--after-cursor={cursor}"));
    }
    args.push(format!("_SYSTEMD_USER_SLICE={slice}"));
    Ok(args)
}

#[derive(Debug)]
struct InstanceLogs {
    ring: VecDeque<LogLine>,
    bytes: usize,
    sender: broadcast::Sender<LogLine>,
}

/// Per-instance bounded rings and broadcast channels with atomic subscriptions.
#[derive(Debug)]
pub struct LogHub {
    instances: Mutex<HashMap<InstanceId, InstanceLogs>>,
    max_lines: usize,
    max_bytes: usize,
    channel_capacity: usize,
}

impl Default for LogHub {
    fn default() -> Self {
        Self::new(2000, 2 * 1024 * 1024, 1024)
    }
}

impl LogHub {
    /// Construct a hub with line, byte and live-channel limits per instance.
    pub fn new(max_lines: usize, max_bytes: usize, channel_capacity: usize) -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            max_lines,
            max_bytes,
            channel_capacity: channel_capacity.max(1),
        }
    }
    fn instance<'a>(
        &self,
        map: &'a mut HashMap<InstanceId, InstanceLogs>,
        id: &InstanceId,
    ) -> &'a mut InstanceLogs {
        map.entry(id.clone()).or_insert_with(|| InstanceLogs {
            ring: VecDeque::new(),
            bytes: 0,
            sender: broadcast::channel(self.channel_capacity).0,
        })
    }
    /// Append a line to its bounded ring and broadcast while holding the same lock.
    pub fn push(&self, mut line: LogLine) {
        line.line = truncate_line(&line.line);
        let mut map = self.instances.lock().unwrap();
        let logs = self.instance(&mut map, &line.instance);
        logs.bytes += line.line.len();
        logs.ring.push_back(line.clone());
        while logs.ring.len() > self.max_lines || logs.bytes > self.max_bytes {
            if let Some(removed) = logs.ring.pop_front() {
                logs.bytes -= removed.line.len();
            } else {
                break;
            }
        }
        let _ = logs.sender.send(line);
    }
    /// Return the most recent `n` retained lines in chronological order.
    pub fn tail(&self, id: &InstanceId, n: usize) -> Vec<LogLine> {
        let map = self.instances.lock().unwrap();
        map.get(id)
            .map(|logs| {
                logs.ring
                    .iter()
                    .skip(logs.ring.len().saturating_sub(n))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Atomically return a backlog and a live receiver with no gap or duplicate.
    pub fn subscribe_logs(
        &self,
        id: &InstanceId,
        backlog: usize,
    ) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        let mut map = self.instances.lock().unwrap();
        let logs = self.instance(&mut map, id);
        let lines = logs
            .ring
            .iter()
            .skip(logs.ring.len().saturating_sub(backlog))
            .cloned()
            .collect();
        let receiver = logs.sender.subscribe();
        (lines, receiver)
    }
    /// Discard an instance's retained log state and close its live channel.
    pub fn remove(&self, id: &InstanceId) {
        self.instances.lock().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn line(id: &InstanceId, text: &str) -> LogLine {
        LogLine {
            instance: id.clone(),
            stream: Stream::Stdout,
            line: text.into(),
            ts: Timestamp::from_second(1).unwrap(),
        }
    }

    #[test]
    fn fixture_decodes_arrays_ansi_and_operation_streams() {
        let names = UnitNames::default();
        let parsed: Vec<_> = include_str!("fixtures/journal.jsonl")
            .lines()
            .map(|line| parse_line(line, &names).unwrap())
            .collect();
        assert_eq!(parsed[0].as_ref().unwrap().line.line, "ok �");
        assert_eq!(parsed[0].as_ref().unwrap().line.instance.as_str(), "app@feature");
        assert_eq!(parsed[0].as_ref().unwrap().line.stream, Stream::Stdout);
        assert_eq!(
            parsed[0].as_ref().unwrap().line.ts.as_microsecond(),
            1_760_000_000_123_456
        );
        assert_eq!(parsed[0].as_ref().unwrap().cursor.as_deref(), Some("s=1"));
        assert_eq!(parsed[1].as_ref().unwrap().line.stream, Stream::Setup);
        assert_eq!(parsed[2].as_ref().unwrap().line.stream, Stream::System);
        assert!(parsed[3].is_none());
        assert_eq!(
            clean_message("x\x1b]8;;https://host\x1b\\link\x1b]8;;\x07!"),
            "xlink!"
        );
        assert!(parse_line(r#"{"_SYSTEMD_USER_UNIT":"lantern-inst-app.service","MESSAGE":[256],"__REALTIME_TIMESTAMP":"1"}"#, &names).is_err());
    }

    #[test]
    fn ring_respects_both_limits_and_broadcasts_after_backlog() {
        let hub = LogHub::new(2, 5, 4);
        let id = InstanceId::parse("app").unwrap();
        hub.push(line(&id, "ab"));
        hub.push(line(&id, "cd"));
        hub.push(line(&id, "efg"));
        let (backlog, mut live) = hub.subscribe_logs(&id, 2);
        assert_eq!(
            backlog.iter().map(|x| x.line.as_str()).collect::<Vec<_>>(),
            ["cd", "efg"]
        );
        hub.push(line(&id, "h"));
        assert_eq!(live.try_recv().unwrap().line, "h");
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        hub.remove(&id);
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert!(hub.tail(&id, 10).is_empty());
        assert!(clean_message(&"é".repeat(MAX_LINE_BYTES)).len() <= MAX_LINE_BYTES);
    }

    #[test]
    fn subscription_is_gap_free_during_concurrent_writes() {
        let hub = Arc::new(LogHub::new(2000, 100_000, 2000));
        let id = InstanceId::parse("app").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let (writer, writer_id, sync) = (hub.clone(), id.clone(), barrier.clone());
        let thread = std::thread::spawn(move || {
            sync.wait();
            for n in 0..1000 {
                writer.push(line(&writer_id, &n.to_string()));
            }
        });
        barrier.wait();
        let (backlog, mut rx) = hub.subscribe_logs(&id, 2000);
        thread.join().unwrap();
        let mut numbers: Vec<usize> = backlog.iter().map(|l| l.line.parse().unwrap()).collect();
        while let Ok(line) = rx.try_recv() {
            numbers.push(line.line.parse().unwrap());
        }
        assert_eq!(numbers, (0..1000).collect::<Vec<_>>());
    }

    struct Cleanup {
        unit: String,
        dir: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            use std::process::{Command, Stdio};
            for action in ["stop", "reset-failed"] {
                let _ = Command::new("systemctl")
                    .args(["--user", action, "--", &self.unit])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    #[ignore = "real system: creates and reads its own test unit in app-lantern-test.slice"]
    async fn real_unit_journal_lines() {
        use super::super::systemd::{SystemctlBackend, UnitStart, UserSystemd};
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let id = InstanceId::parse(&format!("jl-{unique}")).unwrap();
        let names = UnitNames {
            instance_prefix: "lantern-test-runtime-inst-".into(),
            operation_prefix: "lantern-test-runtime-op-".into(),
            slice: "app-lantern-test.slice".into(),
        };
        let unit = names.instance(&id);
        let dir = std::env::temp_dir().join(format!("lantern-test-runtime-journal-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup {
            unit: unit.clone(),
            dir: dir.clone(),
        };
        let script = dir.join("test.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\nprintf '\\033[32mjournal-test-line\\033[0m\\n'\nexit 3\n",
        )
        .unwrap();
        let spec = UnitStart {
            unit: unit.clone(),
            instance: id.clone(),
            slice: names.slice.clone(),
            cwd: dir,
            script,
            post_stop: None,
            stop_timeout: Duration::from_secs(1),
        };
        SystemctlBackend.start(&spec).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = tokio::process::Command::new("journalctl")
                .args([
                    "--user",
                    "--output=json",
                    "--all",
                    "--no-pager",
                    "-n",
                    "100",
                    &format!("_SYSTEMD_USER_UNIT={unit}"),
                ])
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            let found = text
                .lines()
                .filter_map(|row| parse_line(row, &names).ok().flatten())
                .any(|row| {
                    row.line.instance == id
                        && row.line.stream == Stream::Stdout
                        && row.line.line == "journal-test-line"
                });
            if found {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "test unit's journal line was not found: {text}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
