-- 迁移 3：单元实例注册、拓扑世代与游戏动作账（实施规格 §6、§7）。
--
-- 两处对规格起始 SQL 的有意调整，都基于规格自己的警告"避免另建不一致的第二套动作账"：
--
-- 1. 单元注册信息并入既有 units 表，而不是另建 unit_instances。
--    同一理由适用于单元目录：两张表会产出"注册说它在跑、快照说它是冷的"这种无法裁决的
--    分歧。同一行内，lifecycle 与 state、consumed_sequence 与 last_applied_sequence
--    各自共用一列，因此不可能互相矛盾。
-- 2. snapshot_ref 保留为 units.snapshot_json。P0 的内联快照还没到需要外置到内容仓的体积，
--    等实测证明需要时再改引用，而不是现在先假设一个还没有测量依据的结构。

ALTER TABLE units ADD COLUMN subject_id      TEXT;
ALTER TABLE units ADD COLUMN parent_id       TEXT;
ALTER TABLE units ADD COLUMN template_id     TEXT;
ALTER TABLE units ADD COLUMN partition_key   TEXT;
ALTER TABLE units ADD COLUMN state_revision  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE units ADD COLUMN topology_epoch  INTEGER NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_units_subject   ON units (subject_id);
CREATE INDEX IF NOT EXISTS idx_units_partition ON units (subject_id, partition_key);
CREATE INDEX IF NOT EXISTS idx_units_epoch     ON units (topology_epoch);

-- 主体路由。current_epoch 是 CAS 切换的目标（§6.2 "路由提交"）。
CREATE TABLE IF NOT EXISTS subject_routes (
    subject_id     TEXT PRIMARY KEY,
    current_epoch  INTEGER NOT NULL,
    graph_ref      TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL
);

-- 迁移事务。UNIQUE(subject_id, new_epoch) 让"同一主体同一世代只能有一个事务"成为数据库
-- 约束，而不是调用方的纪律。
CREATE TABLE IF NOT EXISTS scale_transactions (
    transaction_id          TEXT PRIMARY KEY,
    subject_id              TEXT NOT NULL,
    old_epoch               INTEGER NOT NULL,
    new_epoch               INTEGER NOT NULL,
    state                   TEXT NOT NULL,
    plan_json               TEXT NOT NULL,
    resource_reservation_id TEXT NOT NULL,
    deadline_utc            TEXT NOT NULL,
    created_at_utc          TEXT NOT NULL,
    updated_at_utc          TEXT NOT NULL,
    UNIQUE (subject_id, new_epoch)
);

CREATE INDEX IF NOT EXISTS idx_scale_transactions_state
    ON scale_transactions (state, subject_id);

-- 消息去重键 (unit_id, event_id)（§6.3）。
-- 与动作幂等键相互独立：一条事件被同一单元消费一次，与它是否触发过外部动作无关。
CREATE TABLE IF NOT EXISTS processed_events (
    unit_id           TEXT NOT NULL,
    event_id          TEXT NOT NULL,
    result_ref        TEXT,
    processed_at_utc  TEXT NOT NULL,
    PRIMARY KEY (unit_id, event_id)
);

-- 游戏动作账（§11.2、§13）。
-- status='pending' 表示"已登记请求、step 尚无结论"。崩溃留下的 pending 正是恢复流程要
-- 处理的 unknown_commit：不能简单 reset 然后冒充继续。
CREATE TABLE IF NOT EXISTS game_action_ledger (
    episode_id              TEXT NOT NULL,
    request_id              TEXT NOT NULL,
    request_hash            TEXT NOT NULL,
    expected_observation_id TEXT NOT NULL,
    topology_epoch          INTEGER NOT NULL,
    status                  TEXT NOT NULL,
    result_observation_id   TEXT,
    receipt_json            TEXT,
    recorded_at_utc         TEXT NOT NULL,
    settled_at_utc          TEXT,
    PRIMARY KEY (episode_id, request_id)
);

CREATE INDEX IF NOT EXISTS idx_game_ledger_status ON game_action_ledger (status, episode_id);
