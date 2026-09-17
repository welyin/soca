-- SoCA 存储层 schema v1（架构文档 §7.3、§9.3）。
--
-- 三条硬性语义，后面所有代码都围绕它们：
--   1. 事件日志 append-only；历史重放只读已记录内容，不重新执行外部动作（§7.3）。
--   2. ActionIntent、状态更新与 outbox 记录必须落在同一个事务里（§7.3）。
--   3. 动作以 action_id 去重；同一动作不得产生第二次外部副作用（§7.3）。
--
-- 本文件只含 DDL。PRAGMA（journal_mode / synchronous / foreign_keys / busy_timeout）
-- 由 Rust 侧在打开连接时显式设置，避免"迁移脚本顺手改了持久化语义"。

CREATE TABLE IF NOT EXISTS schema_migrations (
    version        INTEGER PRIMARY KEY,
    applied_at_utc TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- 事件日志（§7.1、§9.3）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS events (
    -- 全库单调提交序。冷单元恢复时的游标就是它（§9.2：读取游标后的事件）。
    sequence              INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id              TEXT    NOT NULL UNIQUE,
    task_id               TEXT    NOT NULL,
    source_id             TEXT    NOT NULL,
    source_epoch          INTEGER NOT NULL,
    boot_id               TEXT    NOT NULL,
    source_sequence       INTEGER NOT NULL,
    observed_at_utc       TEXT    NOT NULL,
    received_monotonic_ns INTEGER NOT NULL,
    data_class            TEXT    NOT NULL,
    provenance_kind       TEXT    NOT NULL,
    -- §6.1 / §11.1：这条消息是否具有指令权限。冗余成列，使审计查询不必解析 JSON。
    instruction_authority INTEGER NOT NULL,
    -- 完整信封。审计与重放都读它，不读拼接出来的近似结构。
    envelope_json         TEXT    NOT NULL,
    -- 仅在信封带内联载荷时非空，且必须是合法 JSON 的字符串。
    inline_payload        TEXT,
    blob_ref              TEXT,
    recorded_at_utc       TEXT    NOT NULL,
    UNIQUE (source_id, source_epoch, boot_id, source_sequence)
);

CREATE INDEX IF NOT EXISTS idx_events_task   ON events (task_id, sequence);
CREATE INDEX IF NOT EXISTS idx_events_stream ON events (source_id, source_epoch, boot_id, source_sequence);

-- 幂等去重（§7.3：至少一次消息 + 幂等处理）。
-- 重复投递的同一幂等键不再产生第二条事件，也不重复触发单元唤醒。
CREATE TABLE IF NOT EXISTS idempotency (
    idempotency_key TEXT PRIMARY KEY,
    event_id        TEXT NOT NULL,
    sequence        INTEGER NOT NULL,
    first_seen_at_utc TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- 动作账（§7.3、§12.2）
-- ---------------------------------------------------------------------------
-- state 取值：
--   PREPARED        意图已生成并写入 outbox，尚未交给执行代理。可安全（重新）投递。
--   SUBMITTED       已交给执行代理，副作用是否发生**未知**。
--   COMPLETED       执行代理报告完成。注意：不等于后置条件已被验证（§7.2）。
--   FAILED          执行代理报告失败。
--   ABORTED         恢复核对确认副作用未发生。与 FAILED 不同：它说明目标未被改动，
--                   调度器可以据此重新决策，而不是把它当成一次失败的结果。
--   UNKNOWN_COMMIT  在执行后、回执前崩溃，且恢复时未能确认目标状态（§7.3）。
--   DENIED          许可校验未通过，从未进入 outbox。拒绝原因留痕（§6.8）。
CREATE TABLE IF NOT EXISTS actions (
    action_id         TEXT PRIMARY KEY,
    task_id           TEXT NOT NULL,
    unit_id           TEXT NOT NULL,
    tool_id           TEXT NOT NULL,
    object_scope      TEXT NOT NULL,
    parameters_digest TEXT NOT NULL,
    action_level      TEXT NOT NULL,
    prediction_ref    TEXT NOT NULL,
    intent_json       TEXT NOT NULL,
    permit_id         TEXT,
    state             TEXT NOT NULL,
    -- ALLOWED / DENIED。拒绝也必须留痕（§6.8：否决保留在最小审计账中）。
    decision          TEXT NOT NULL,
    denial_reason     TEXT,
    uses_so_far       INTEGER NOT NULL DEFAULT 0,
    created_at_utc    TEXT NOT NULL,
    updated_at_utc    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_actions_state ON actions (state);
CREATE INDEX IF NOT EXISTS idx_actions_task  ON actions (task_id, created_at_utc);

-- 许可消耗记录。max_uses 计数与审计共用同一份事实，不各记一份。
CREATE TABLE IF NOT EXISTS permit_uses (
    permit_id   TEXT NOT NULL,
    action_id   TEXT NOT NULL,
    used_at_utc TEXT NOT NULL,
    PRIMARY KEY (permit_id, action_id)
);

-- ---------------------------------------------------------------------------
-- Outbox（§7.3）
-- ---------------------------------------------------------------------------
-- dispatched_at_utc IS NULL 表示可以安全投递：要么从未投出，要么上次投递未提交成功。
-- dispatched_at_utc 非空且没有回执，表示副作用可能已发生，恢复时只能核对，不能重发。
CREATE TABLE IF NOT EXISTS outbox (
    outbox_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    action_id         TEXT NOT NULL UNIQUE,
    payload_json      TEXT NOT NULL,
    created_at_utc    TEXT NOT NULL,
    dispatched_at_utc TEXT,
    settled_at_utc    TEXT
);

CREATE INDEX IF NOT EXISTS idx_outbox_pending
    ON outbox (outbox_id) WHERE dispatched_at_utc IS NULL;

-- ---------------------------------------------------------------------------
-- 执行回执与后置验证（§7.2）
-- ---------------------------------------------------------------------------
-- receipts 与 outcomes 分成两张表，是 §7.2 的直接体现：
-- 回执只说明"提交了/完成了"，后置条件验证必须由新观测另行产生。
CREATE TABLE IF NOT EXISTS receipts (
    action_id               TEXT PRIMARY KEY,
    permit_id               TEXT NOT NULL,
    status                  TEXT NOT NULL,
    recorded_at_utc         TEXT NOT NULL,
    detail                  TEXT NOT NULL,
    observed_target_version TEXT,
    receipt_json            TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS outcomes (
    action_id       TEXT PRIMARY KEY,
    prediction_ref  TEXT NOT NULL,
    verdict         TEXT NOT NULL,
    verified_at_utc TEXT NOT NULL,
    outcome_json    TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- 单元目录与游标（§3.2、§9.2）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS units (
    unit_id               TEXT PRIMARY KEY,
    kind                  TEXT NOT NULL,
    scope_domain          TEXT NOT NULL,
    scope_task_contract   TEXT NOT NULL,
    state                 TEXT NOT NULL,
    belief_revision       INTEGER NOT NULL,
    last_applied_sequence INTEGER NOT NULL,
    strategy_version      TEXT NOT NULL,
    model_profile_ref     TEXT NOT NULL,
    capability_policy_ref TEXT NOT NULL,
    budget_ref            TEXT NOT NULL,
    snapshot_json         TEXT NOT NULL,
    updated_at_utc        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_units_state ON units (state);

-- ---------------------------------------------------------------------------
-- 内容仓对象目录（§9.3）
-- ---------------------------------------------------------------------------
-- 大对象本体放在分段内容仓里；本表只记引用、校验和与保留期。
-- 先写临时文件、完成校验和耐久化，再提交本表引用；崩溃留下的孤儿对象由 GC 回收。
CREATE TABLE IF NOT EXISTS blobs (
    blob_ref       TEXT PRIMARY KEY,
    sha256         TEXT NOT NULL,
    media_type     TEXT NOT NULL,
    bytes          INTEGER NOT NULL,
    created_at_utc TEXT NOT NULL,
    retired_at_utc TEXT
);

-- ---------------------------------------------------------------------------
-- 审计账（§12.3）
-- ---------------------------------------------------------------------------
-- 只保留最小元数据：不保存密码、完整 prompt 或无限个人内容。
CREATE TABLE IF NOT EXISTS audit_log (
    audit_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    at_utc      TEXT NOT NULL,
    category    TEXT NOT NULL,
    subject_ref TEXT NOT NULL,
    outcome     TEXT NOT NULL,
    detail      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log (at_utc);
