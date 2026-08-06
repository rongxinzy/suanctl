//! 本地数据存储：基于 SurrealDB 嵌入式（surrealkv 文件引擎）的历史快照库。
//!
//! 用途：把每次采集的完整快照持久化到本地，支持历史列表与按 id 回放，
//! 为后续趋势/基线对比（ROADMAP C2）与事件存储（C1）打基础。
//!
//! 表结构：
//! - `snapshots`：完整采集快照（captured_at/status/source/hostname + 原样 JSON）。
//! - `events`：预留（monitor 事件存储，Phase 2 接入）。

use std::path::Path;

use serde::{Deserialize, Serialize};
use surrealdb::engine::local::Db;
use surrealdb::types::{RecordId, SurrealValue, ToSql};
use surrealdb::Surreal;

use crate::domain::{DashboardSnapshot, HealthStatus};

/// SurrealDB 连接错误。
#[derive(Debug)]
pub struct StoreError {
    pub message: String,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

impl From<surrealdb::Error> for StoreError {
    fn from(error: surrealdb::Error) -> Self {
        Self {
            message: format!("存储引擎错误：{error}"),
        }
    }
}

const NAMESPACE: &str = "suanctl";
const DATABASE: &str = "main";

/// 快照记录元信息（列表展示用，不含完整快照）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub id: String,
    pub captured_at: u64,
    pub status: String,
    pub source: String,
    pub hostname: String,
    pub gpu_count: usize,
}

/// 历史快照检索条件。所有条件为可选，组合取 AND 语义；
/// 值一律经 SurrealQL bind 参数绑定，避免注入。
#[derive(Debug, Clone, Default)]
pub struct SnapshotQuery {
    pub limit: usize,
    pub offset: usize,
    /// 采集状态过滤（与 save 时记录的 label 一致）。
    pub status: Option<HealthStatus>,
    /// 主机名子串匹配。
    pub host_contains: Option<String>,
    /// captured_at 下界（Unix 毫秒，含）。
    pub since_millis: Option<u64>,
    /// captured_at 上界（Unix 毫秒，含）。
    pub until_millis: Option<u64>,
    /// 日志异常模式过滤（快照 logs.matches 含该 pattern）。
    pub pattern: Option<String>,
    /// 日志来源过滤（快照 logs.sources 含该来源，如 dmesg/kern.log）。
    pub log_source: Option<String>,
    /// 日志异常严重级别过滤（快照 logs.matches 含该 severity）。
    pub log_status: Option<HealthStatus>,
    /// GPU 数量下界（含）。
    pub gpus_min: Option<usize>,
}

/// 完整存储记录（读取用；`id` 由库填充）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct StoredSnapshot {
    pub id: RecordId,
    pub captured_at: i64,
    pub status: String,
    pub source: String,
    pub hostname: String,
    pub gpu_count: i64,
    #[surreal(wrap)]
    pub snapshot: DashboardSnapshot,
}

/// 写入用内容（不含 id，id 由库自动生成）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct SnapshotContent {
    captured_at: i64,
    status: String,
    source: String,
    hostname: String,
    gpu_count: i64,
    #[surreal(wrap)]
    snapshot: DashboardSnapshot,
}

/// 列表查询用行（只取元字段）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct SnapshotMetaRow {
    id: RecordId,
    captured_at: i64,
    status: String,
    source: String,
    hostname: String,
    gpu_count: i64,
}

/// 日志异常事件（log_events 表内容，写入用）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct LogEventContent {
    captured_at: i64,
    pattern: String,
    severity: String,
    count: i64,
    snapshot_id: String,
    sources: Vec<String>,
    examples: Vec<String>,
}

/// 日志异常事件行（读取用；`id` 由库填充）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct LogEventRow {
    id: RecordId,
    captured_at: i64,
    pattern: String,
    severity: String,
    count: i64,
    snapshot_id: String,
    sources: Vec<String>,
    examples: Vec<String>,
}

/// 日志异常事件元信息（对外展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEventMeta {
    pub id: String,
    pub captured_at: u64,
    pub pattern: String,
    pub severity: String,
    pub count: usize,
    pub snapshot_id: String,
    pub sources: Vec<String>,
    pub examples: Vec<String>,
}

impl From<LogEventRow> for LogEventMeta {
    fn from(row: LogEventRow) -> Self {
        Self {
            id: record_id_string(&row.id),
            captured_at: row.captured_at.max(0) as u64,
            pattern: row.pattern,
            severity: row.severity,
            count: row.count.max(0) as usize,
            snapshot_id: row.snapshot_id,
            sources: row.sources,
            examples: row.examples,
        }
    }
}

/// 日志异常模式统计行（查询用）。
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct LogEventStatRow {
    pattern: String,
    occurrences: i64,
    total_hits: i64,
}

/// 日志异常模式统计（对外展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEventStat {
    pub pattern: String,
    /// 出现该模式的事件数（快照次数）。
    pub occurrences: usize,
    /// 所有事件命中行数合计。
    pub total_hits: usize,
}

impl From<LogEventStatRow> for LogEventStat {
    fn from(row: LogEventStatRow) -> Self {
        Self {
            pattern: row.pattern,
            occurrences: row.occurrences.max(0) as usize,
            total_hits: row.total_hits.max(0) as usize,
        }
    }
}

/// 本地库句柄。每个 CLI 调用创建一次连接，用后即弃（嵌入式库无常驻服务）。
pub struct Store {
    db: Surreal<Db>,
}

impl Store {
    /// 打开（必要时创建）位于 `path` 的嵌入式数据库并初始化 schema。
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| StoreError {
                message: format!("无法创建数据目录 {}：{error}", parent.display()),
            })?;
        }
        let db: Surreal<Db> =
            Surreal::new::<surrealdb::engine::local::SurrealKv>(path.to_string_lossy().as_ref())
                .await?;
        db.use_ns(NAMESPACE).use_db(DATABASE).await?;
        db.query(
            "DEFINE TABLE IF NOT EXISTS snapshots SCHEMAFULL;
             DEFINE FIELD IF NOT EXISTS captured_at ON snapshots TYPE number;
             DEFINE FIELD IF NOT EXISTS status ON snapshots TYPE string;
             DEFINE FIELD IF NOT EXISTS source ON snapshots TYPE string;
             DEFINE FIELD IF NOT EXISTS hostname ON snapshots TYPE string;
             DEFINE FIELD IF NOT EXISTS gpu_count ON snapshots TYPE number;
             DEFINE FIELD IF NOT EXISTS snapshot ON snapshots TYPE object FLEXIBLE;
             DEFINE INDEX IF NOT EXISTS snapshots_captured_at ON snapshots FIELDS captured_at;
             DEFINE TABLE IF NOT EXISTS log_events SCHEMAFULL;
             DEFINE FIELD IF NOT EXISTS captured_at ON log_events TYPE number;
             DEFINE FIELD IF NOT EXISTS pattern ON log_events TYPE string;
             DEFINE FIELD IF NOT EXISTS severity ON log_events TYPE string;
             DEFINE FIELD IF NOT EXISTS count ON log_events TYPE number;
             DEFINE FIELD IF NOT EXISTS snapshot_id ON log_events TYPE string;
             DEFINE FIELD IF NOT EXISTS sources ON log_events TYPE array;
             DEFINE FIELD IF NOT EXISTS examples ON log_events TYPE array;
             DEFINE INDEX IF NOT EXISTS log_events_pattern ON log_events FIELDS pattern;
             DEFINE INDEX IF NOT EXISTS log_events_captured_at ON log_events FIELDS captured_at;",
        )
        .await?;
        Ok(Self { db })
    }

    /// 保存一次快照，返回记录 id。`status` 为采集总体状态（与 doctor/report 一致）。
    pub async fn save_snapshot(
        &self,
        snapshot: &DashboardSnapshot,
        status: HealthStatus,
    ) -> Result<String, StoreError> {
        let hostname = snapshot.host.hostname.clone();
        let source = snapshot.source.label().to_owned();
        let content = SnapshotContent {
            captured_at: snapshot.captured_at as i64,
            status: status.label().to_owned(),
            source,
            hostname,
            gpu_count: snapshot.gpus.len() as i64,
            snapshot: snapshot.clone(),
        };
        let created: Option<StoredSnapshot> = self.db.create("snapshots").content(content).await?;
        let id = created
            .as_ref()
            .map(|record| record_id_string(&record.id))
            .ok_or_else(|| StoreError {
                message: "保存快照后未返回记录".to_owned(),
            })?;
        // 将快照内的日志异常匹配展开为独立事件行，支持跨快照检索与统计。
        if let Some(logs) = &snapshot.logs {
            for matched in &logs.matches {
                let event = LogEventContent {
                    captured_at: snapshot.captured_at as i64,
                    pattern: matched.pattern.clone(),
                    severity: matched.severity.label().to_owned(),
                    count: matched.count as i64,
                    snapshot_id: id.clone(),
                    sources: matched.sources.clone(),
                    examples: matched.examples.clone(),
                };
                let _: Option<LogEventRow> = self.db.create("log_events").content(event).await?;
            }
        }
        Ok(id)
    }

    /// 查询日志异常事件（按模式可选过滤，按时间倒序）。
    pub async fn list_log_events(
        &self,
        pattern: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LogEventMeta>, StoreError> {
        let mut request = self.db.query(
            "SELECT id, captured_at, pattern, severity, count, snapshot_id, sources, examples \
             FROM log_events WHERE ($pattern = NONE OR pattern = $pattern) \
             ORDER BY captured_at DESC LIMIT $limit",
        );
        request = request.bind(("pattern", pattern));
        request = request.bind(("limit", limit as i64));
        let rows: Vec<LogEventRow> = request.await?.take(0)?;
        Ok(rows.into_iter().map(LogEventMeta::from).collect())
    }

    /// 日志异常模式统计（跨快照，按事件次数降序）。
    pub async fn log_event_stats(&self, limit: usize) -> Result<Vec<LogEventStat>, StoreError> {
        let rows: Vec<LogEventStatRow> = self
            .db
            .query(
                "SELECT pattern, count() AS occurrences, math::sum(count) AS total_hits \
                 FROM log_events GROUP BY pattern ORDER BY occurrences DESC LIMIT $limit",
            )
            .bind(("limit", limit as i64))
            .await?
            .take(0)?;
        Ok(rows.into_iter().map(LogEventStat::from).collect())
    }

    /// 最近 N 条快照元信息（不含完整快照）。
    pub async fn list_snapshots(&self, limit: usize) -> Result<Vec<SnapshotMeta>, StoreError> {
        self.query_snapshots(&SnapshotQuery {
            limit,
            ..SnapshotQuery::default()
        })
        .await
    }

    /// 按检索条件查询快照元信息（AND 组合、bind 参数化、分页）。
    pub async fn query_snapshots(
        &self,
        query: &SnapshotQuery,
    ) -> Result<Vec<SnapshotMeta>, StoreError> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut status_bind: Option<String> = None;
        let mut host_bind: Option<String> = None;
        let mut since_bind: Option<i64> = None;
        let mut until_bind: Option<i64> = None;
        let mut pattern_bind: Option<String> = None;
        let mut log_source_bind: Option<String> = None;
        let mut log_status_bind: Option<String> = None;
        let mut gpus_bind: Option<i64> = None;

        if let Some(status) = query.status {
            clauses.push("status = $status");
            status_bind = Some(status.label().to_owned());
        }
        if let Some(host) = query.host_contains.as_deref() {
            clauses.push("hostname CONTAINS $host");
            host_bind = Some(host.to_owned());
        }
        if let Some(since) = query.since_millis {
            clauses.push("captured_at >= $since");
            since_bind = Some(since as i64);
        }
        if let Some(until) = query.until_millis {
            clauses.push("captured_at <= $until");
            until_bind = Some(until as i64);
        }
        if let Some(pattern) = query.pattern.as_deref() {
            clauses
                .push("snapshot.logs.matches[WHERE pattern = $pattern].pattern CONTAINS $pattern");
            pattern_bind = Some(pattern.to_owned());
        }
        if let Some(source) = query.log_source.as_deref() {
            clauses
                .push("snapshot.logs.sources[WHERE name = $log_source].name CONTAINS $log_source");
            log_source_bind = Some(source.to_owned());
        }
        if let Some(log_status) = query.log_status {
            clauses.push(
                "snapshot.logs.matches[WHERE severity = $log_status].severity CONTAINS $log_status",
            );
            log_status_bind = Some(health_status_key(log_status).to_owned());
        }
        if let Some(gpus_min) = query.gpus_min {
            clauses.push("gpu_count >= $gpus_min");
            gpus_bind = Some(gpus_min as i64);
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT id, captured_at, status, source, hostname, gpu_count \
             FROM snapshots{where_sql} ORDER BY captured_at DESC LIMIT $limit START $offset"
        );

        let mut request = self.db.query(sql);
        request = request.bind(("limit", query.limit as i64));
        request = request.bind(("offset", query.offset as i64));
        if let Some(value) = status_bind {
            request = request.bind(("status", value));
        }
        if let Some(value) = host_bind {
            request = request.bind(("host", value));
        }
        if let Some(value) = since_bind {
            request = request.bind(("since", value));
        }
        if let Some(value) = until_bind {
            request = request.bind(("until", value));
        }
        if let Some(value) = pattern_bind {
            request = request.bind(("pattern", value));
        }
        if let Some(value) = log_source_bind {
            request = request.bind(("log_source", value));
        }
        if let Some(value) = log_status_bind {
            request = request.bind(("log_status", value));
        }
        if let Some(value) = gpus_bind {
            request = request.bind(("gpus_min", value));
        }
        let rows: Vec<SnapshotMetaRow> = request.await?.take(0)?;
        Ok(rows
            .into_iter()
            .map(|row| SnapshotMeta {
                id: record_id_string(&row.id),
                captured_at: row.captured_at.max(0) as u64,
                status: row.status,
                source: row.source,
                hostname: row.hostname,
                gpu_count: row.gpu_count.max(0) as usize,
            })
            .collect())
    }

    /// 按 id 读取完整快照（用于回放/对比）。
    /// id 兼容两种形式：`abc` 或 `snapshots:abc`。
    pub async fn get_snapshot(&self, id: &str) -> Result<Option<StoredSnapshot>, StoreError> {
        let key = id.rsplit(':').next().unwrap_or(id);
        let record: Option<StoredSnapshot> = self.db.select(("snapshots", key)).await?;
        Ok(record)
    }

    /// 在最近 N 条快照的日志尾部行中搜索关键字（大小写不敏感），
    /// 返回命中快照 + 来源 + 日志行。应用层扫描（SEARCH INDEX 在
    /// surrealkv 引擎不可用），扫描范围受 limit 限制。
    pub async fn search_logs(
        &self,
        keyword: &str,
        limit: usize,
    ) -> Result<Vec<LogSearchHit>, StoreError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        let needle = keyword.to_lowercase();
        let rows: Vec<StoredSnapshot> = self
            .db
            .query("SELECT * FROM snapshots ORDER BY captured_at DESC LIMIT $limit")
            .bind(("limit", limit as i64))
            .await?
            .take(0)?;
        let mut hits = Vec::new();
        for row in rows {
            let Some(logs) = &row.snapshot.logs else {
                continue;
            };
            for source in &logs.sources {
                for line in &source.lines_tail {
                    if line.to_lowercase().contains(&needle) {
                        hits.push(LogSearchHit {
                            snapshot_id: record_id_string(&row.id),
                            captured_at: row.captured_at.max(0) as u64,
                            status: row.status.clone(),
                            hostname: row.hostname.clone(),
                            source: source.name.clone(),
                            line: line.clone(),
                        });
                    }
                }
            }
        }
        Ok(hits)
    }
}

/// 日志内容搜索命中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSearchHit {
    pub snapshot_id: String,
    pub captured_at: u64,
    pub status: String,
    pub hostname: String,
    pub source: String,
    pub line: String,
}

/// RecordId 的展示字符串（形如 `snapshots:abc`）。
fn record_id_string(id: &RecordId) -> String {
    SurrealValue::into_value(id.clone()).to_sql()
}

/// HealthStatus 的 serde 键（与 domain 序列化一致：critical/warning/…）。
fn health_status_key(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Healthy => "healthy",
        HealthStatus::Warning => "warning",
        HealthStatus::Critical => "critical",
        HealthStatus::Unavailable => "unavailable",
        HealthStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::demo;
    use crate::domain::DataSource;

    #[test]
    fn store_saves_lists_and_gets_snapshots() {
        // 用临时目录验证完整持久化闭环。
        let dir = std::env::temp_dir().join(format!("suanctl-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("suanctl.db");
        let snapshot = demo::snapshot();

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async {
            let store = Store::open(&path).await.expect("open store");
            let id = store
                .save_snapshot(&snapshot, HealthStatus::Warning)
                .await
                .expect("save");
            assert!(!id.is_empty(), "应返回自动生成的记录 id");

            let listed = store.list_snapshots(5).await.expect("list");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].hostname, "rx-box（演示）");
            assert_eq!(listed[0].gpu_count, 2);
            assert_eq!(listed[0].status, "警告");

            let got = store
                .get_snapshot(&listed[0].id)
                .await
                .expect("get")
                .expect("record exists");
            assert_eq!(got.snapshot.source, DataSource::Demo);
            assert_eq!(got.snapshot.gpus.len(), 2);

            // 关闭连接（释放 surrealkv 文件锁）后重新打开，验证持久化。
            drop(store);
            let reopened = Store::open(&path).await.expect("reopen");
            let relisted = reopened.list_snapshots(5).await.expect("relist");
            assert_eq!(relisted.len(), 1);
        });

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn query_filters_status_host_pattern_time_gpus_and_pagination() {
        let dir = std::env::temp_dir().join(format!("suanctl-store-query-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("query.db");

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async {
            let store = Store::open(&path).await.expect("open store");
            // 3 条不同状态/时间的快照（demo 快照 gpus=2、logs 含 xid）。
            let mut snap = demo::snapshot();
            snap.captured_at = 1_000;
            store
                .save_snapshot(&snap, HealthStatus::Warning)
                .await
                .expect("save 1");
            snap.captured_at = 2_000;
            store
                .save_snapshot(&snap, HealthStatus::Critical)
                .await
                .expect("save 2");
            snap.captured_at = 3_000;
            store
                .save_snapshot(&snap, HealthStatus::Healthy)
                .await
                .expect("save 3");

            // status 过滤
            let list = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    status: Some(HealthStatus::Critical),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("status query");
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].status, "严重");

            // host 子串
            let list = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    host_contains: Some("rx-box".to_owned()),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("host query");
            assert_eq!(list.len(), 3);

            // pattern（日志异常模式）命中与未命中
            let hit = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    pattern: Some("xid".to_owned()),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("pattern hit");
            assert_eq!(hit.len(), 3);
            let miss = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    pattern: Some("no_such_pattern".to_owned()),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("pattern miss");
            assert!(miss.is_empty());

            // gpus 下界
            let gpu_ok = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    gpus_min: Some(2),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("gpus hit");
            assert_eq!(gpu_ok.len(), 3);
            let gpu_miss = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    gpus_min: Some(3),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("gpus miss");
            assert!(gpu_miss.is_empty());

            // 时间范围
            let ranged = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    since_millis: Some(2_000),
                    until_millis: Some(3_000),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("range query");
            assert_eq!(ranged.len(), 2);

            // 组合：status + 时间 + 分页
            let combo = store
                .query_snapshots(&SnapshotQuery {
                    limit: 1,
                    offset: 1,
                    status: None,
                    host_contains: Some("rx-box".to_owned()),
                    since_millis: Some(1_000),
                    until_millis: Some(3_000),
                    pattern: None,
                    log_source: None,
                    log_status: None,
                    gpus_min: None,
                })
                .await
                .expect("combo query");
            assert_eq!(
                combo.len(),
                1,
                "limit=1 offset=1 → 第 2 条（时间倒序为 captured_at=2000）"
            );
            assert_eq!(combo[0].captured_at, 2_000);
        });

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_events_expand_save_and_stats_and_search() {
        let dir =
            std::env::temp_dir().join(format!("suanctl-store-logevents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.db");

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async {
            let store = Store::open(&path).await.expect("open store");
            // demo 快照 logs 含 xid 与 gpu_fallen_off 两个 Critical 匹配。
            let snapshot = demo::snapshot();
            let id = store
                .save_snapshot(&snapshot, HealthStatus::Critical)
                .await
                .expect("save");

            // 事件展开
            let events = store.list_log_events(None, 20).await.expect("events");
            assert_eq!(events.len(), 2, "两个异常模式应展开为两条事件");
            assert_eq!(events[0].severity, "严重");
            assert!(events[0].snapshot_id.contains(&id[..id.len().min(20)]));
            let by_pattern = store
                .list_log_events(Some("xid"), 20)
                .await
                .expect("events by pattern");
            assert_eq!(by_pattern.len(), 1);
            assert_eq!(by_pattern[0].pattern, "xid");

            // 统计
            let stats = store.log_event_stats(10).await.expect("stats");
            assert_eq!(stats.len(), 2);
            let xid = stats.iter().find(|s| s.pattern == "xid").expect("xid stat");
            assert_eq!(xid.occurrences, 1);

            // 日志内容搜索（demo dmesg 尾部含 NVRM: Xid 行）
            let hits = store.search_logs("Xid", 10).await.expect("search");
            assert!(
                hits.iter().any(|h| h.line.contains("NVRM: Xid")),
                "应命中 dmesg 的 Xid 行：{hits:?}"
            );
            let miss = store
                .search_logs("不存在关键字XYZ", 10)
                .await
                .expect("search miss");
            assert!(miss.is_empty());

            // 日志来源/级别过滤（快照级）
            let by_source = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    log_source: Some("dmesg".to_owned()),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("log source filter");
            assert_eq!(by_source.len(), 1);
            let by_status = store
                .query_snapshots(&SnapshotQuery {
                    limit: 10,
                    log_status: Some(HealthStatus::Critical),
                    ..SnapshotQuery::default()
                })
                .await
                .expect("log status filter");
            assert_eq!(by_status.len(), 1, "demo 日志匹配为严重级别");
        });

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
