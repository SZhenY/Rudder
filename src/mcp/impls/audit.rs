//! 审计日志落盘：按天切分的 NDJSON + 内存环形缓冲。
//!
//! 三条硬约束：
//! 1. **写盘慢不能拖慢工具调用**：`try_send` 进有界通道，专用线程负责 IO；
//!    通道满了就丢并计数，绝不让调用方等。
//! 2. **写盘失败不能让工具失败**：所有 IO 错误只 `warn!` + 置降级位。
//! 3. **宁可停写也不截断**：单文件到顶就写一条 `audit_rotated` 后当天停写，
//!    而不是像 `error.log` 那样清空重来 —— 审计被抹掉比丢一段更糟。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use super::audit_types::AuditEntry;

/// AI 页"最近调用"保留的条数。
const RING_CAPACITY: usize = 200;
/// 待写队列容量：突发流量下够撑一阵，满了就丢。
const CHANNEL_CAPACITY: usize = 1024;
/// 单日文件上限。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

static TX: OnceLock<Option<SyncSender<Value>>> = OnceLock::new();
static RING: OnceLock<Mutex<VecDeque<AuditEntry>>> = OnceLock::new();
static DEGRADED: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn ring() -> &'static Mutex<VecDeque<AuditEntry>> {
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAPACITY)))
}

/// 起写线程（幂等）。`enabled == false` 时也要起 —— 开关可能在会话中途被打开。
pub(super) fn init() {
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let spawned = std::thread::Builder::new()
            .name("rudder-mcp-audit".to_string())
            .spawn(move || drain(rx));
        if spawned.is_err() {
            tracing::warn!("mcp audit: writer thread failed to start; audit disabled");
            DEGRADED.store(true, Ordering::Relaxed);
            return None;
        }
        Some(tx)
    });
}

/// 记一条。`record` 是整条 JSON（写文件），`entry` 是最小集（给界面）。
pub(super) fn record(record: Value, entry: AuditEntry) {
    {
        let mut ring = ring().lock().unwrap_or_else(|err| err.into_inner());
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry);
    }
    init();
    let Some(Some(tx)) = TX.get() else {
        return;
    };
    match tx.try_send(record) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {
            DEGRADED.store(true, Ordering::Relaxed);
        }
    }
}

/// 只写文件，不进环形缓冲（会话事件用）。
pub(super) fn record_only(record: Value) {
    init();
    let Some(Some(tx)) = TX.get() else {
        return;
    };
    let _ = tx.try_send(record);
}

/// 最近若干条（新的在前）。
pub(super) fn recent() -> Vec<AuditEntry> {
    let ring = ring().lock().unwrap_or_else(|err| err.into_inner());
    ring.iter().rev().cloned().collect()
}

/// 是否处于降级（写盘失败 / 线程没起来 / 队列断过）。
pub(super) fn degraded() -> bool {
    DEGRADED.load(Ordering::Relaxed)
}


fn drain(rx: Receiver<Value>) {
    let dir = crate::config::log_dir();
    let mut sink = Sink::new(dir);
    while let Ok(record) = rx.recv() {
        sink.write(&record);
    }
}

struct Sink {
    dir: PathBuf,
    file: Option<File>,
    day: String,
    written: u64,
    /// 当天已到上限：只写一条说明，之后不再动文件。
    full_for_today: bool,
    warned: bool,
}

impl Sink {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            file: None,
            day: String::new(),
            written: 0,
            full_for_today: false,
            warned: false,
        }
    }

    fn write(&mut self, record: &Value) {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if today != self.day {
            self.file = None;
            self.day = today;
            self.written = 0;
            self.full_for_today = false;
            sweep(&self.dir, record_retention_days());
        }
        if self.full_for_today {
            return;
        }
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(err) => {
                self.warn_once(&format!("serialize failed: {err}"));
                return;
            }
        };
        let bytes = line.len() as u64 + 1;
        if self.written + bytes > MAX_FILE_BYTES {
            self.full_for_today = true;
            let note = json!({
                "ts": chrono::Local::now().to_rfc3339(),
                "event": "audit_rotated",
                "reason": "size_cap",
            });
            if let Ok(note) = serde_json::to_string(&note) {
                self.append(note.as_bytes());
            }
            return;
        }
        self.append(line.as_bytes());
        self.written += bytes;
    }

    fn append(&mut self, line: &[u8]) {
        if self.file.is_none() && !self.open() {
            return;
        }
        let failure = {
            let Some(file) = self.file.as_mut() else {
                return;
            };
            file.write_all(line)
                .and_then(|_| file.write_all(b"\n"))
                .err()
        };
        if let Some(err) = failure {
            self.file = None;
            self.warn_once(&format!("write failed: {err}"));
        }
    }

    fn open(&mut self) -> bool {
        let path = self.dir.join(format!("audit-{}.ndjson", self.day));
        match open_append(&path) {
            Ok(file) => {
                self.file = Some(file);
                true
            }
            Err(err) => {
                self.warn_once(&format!("open {} failed: {err}", path.display()));
                false
            }
        }
    }

    fn warn_once(&mut self, message: &str) {
        DEGRADED.store(true, Ordering::Relaxed);
        if !self.warned {
            self.warned = true;
            tracing::warn!("mcp audit degraded: {message}");
        }
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

fn record_retention_days() -> u16 {
    RETENTION.load(Ordering::Relaxed) as u16
}

/// 保留天数由配置快照推进来（服务热更新时调用）。
static RETENTION: AtomicU64 = AtomicU64::new(14);

pub(super) fn set_retention(days: u16) {
    RETENTION.store(days as u64, Ordering::Relaxed);
}

/// 删掉超过保留期的 `audit-YYYY-MM-DD.ndjson`。`0` = 永久保留。
fn sweep(dir: &Path, retention_days: u16) {
    if retention_days == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = chrono::Local::now()
        .date_naive()
        .checked_sub_days(chrono::Days::new(retention_days as u64));
    let Some(cutoff) = cutoff else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stamp) = name
            .strip_prefix("audit-")
            .and_then(|rest| rest.strip_suffix(".ndjson"))
        else {
            continue;
        };
        let Ok(date) = chrono::NaiveDate::parse_from_str(stamp, "%Y-%m-%d") else {
            continue;
        };
        if date < cutoff && std::fs::remove_file(&path).is_err() {
            tracing::warn!("mcp audit: failed to remove {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tool: &str) -> AuditEntry {
        AuditEntry {
            at: "10:00:00".to_string(),
            tool: tool.to_string(),
            ok: true,
            code: String::new(),
            ms: 1,
            client: "test".to_string(),
        }
    }

    #[test]
    fn ring_keeps_only_the_last_entries() {
        for index in 0..(RING_CAPACITY + 5) {
            record(json!({"tool": format!("t{index}")}), entry(&format!("t{index}")));
        }
        let recent = recent();
        assert_eq!(recent.len(), RING_CAPACITY);
        assert_eq!(recent.first().unwrap().tool, format!("t{}", RING_CAPACITY + 4));
    }

    #[test]
    fn size_cap_stops_writing_for_the_day() {
        let dir = std::env::temp_dir().join("rudder-audit-test");
        let _ = std::fs::create_dir_all(&dir);
        let mut sink = Sink::new(dir.clone());
        // 先把 day 对齐到"今天"，否则 write() 会当成换天把计数清零。
        sink.day = chrono::Local::now().format("%Y-%m-%d").to_string();
        sink.written = MAX_FILE_BYTES;
        sink.write(&json!({"event": "tool_call"}));
        assert!(sink.full_for_today);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
