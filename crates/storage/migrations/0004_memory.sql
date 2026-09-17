-- 迁移 4：L5 记忆主存与删除传播（架构文档 §4.1 L5、§12.3、§13.2）。
--
-- 为什么不复用 events / blobs / audit_log：
--   那三张表分别回答"发生过什么"、"内容是什么"、"谁做了什么"。记忆回答的是第四类问题——
--   **"系统现在相信什么，依据是什么，什么时候该忘掉"**。把三者混成一张表，会让"删掉一条
--   记忆"变成"删掉一段历史"，而 §12.3 要求的是前者：历史（审计）保留，信念失效。
--
-- 本表结构直接承担三条约束：
--
--   1. 每条记忆必须有证据。非空由契约层保证；memory_evidence 让"按证据反查"成为一次索引
--      扫描——§12.3 的失效传播靠的就是它，而不是全表解析 JSON。
--
--   2. 修订不覆盖原证据（§13.2）。supersedes 指向旧条目，superseded_by 回填到旧条目，
--      两者是同一次事务里的两次更新，不存在"新的写了、旧的没改"的中间态。
--
--   3. 删除先隐藏后清理（§12.3）。status='tombstoned' 即刻生效，purge 是另一条独立路径，
--      由调用方显式发起。到期同样不自动删除：§12.3 要求给用户完成状态。
--
-- 这里**故意不建** supersedes / superseded_by 的外键：§12.3 允许清理已删除的条目，而外键
-- 会让"删除旧修订"因为"新修订还指着它"而失败。引用完整性由契约层与事务内的显式检查保证。

CREATE TABLE IF NOT EXISTS memory_entries (
    memory_id          TEXT PRIMARY KEY,
    kind               TEXT NOT NULL,
    -- 所有者。§4.1 L5："横切；按所有者和任务隔离"。跨所有者的检索不是"加个过滤条件"，
    -- 而是根本不提供入口。
    owner              TEXT NOT NULL,
    task_id            TEXT,
    unit_id            TEXT,
    claim              TEXT NOT NULL,
    evidence_refs_json TEXT NOT NULL,
    relation_refs_json TEXT NOT NULL,
    provenance_json    TEXT NOT NULL,
    data_class         TEXT NOT NULL,
    confidence_json    TEXT,
    revision           INTEGER NOT NULL,
    supersedes         TEXT,
    superseded_by      TEXT,
    recorded_at_utc    TEXT NOT NULL,
    valid_until_utc    TEXT,
    status             TEXT NOT NULL,
    tombstoned_at_utc  TEXT,
    tombstone_reason   TEXT,
    entry_json         TEXT NOT NULL
);

-- 证据反查表。§12.3：「撤回权限或删除个人数据时，传播到快照、摘要、向量索引、模型会话缓存
-- 和备份保留计划」。没有这张表，传播就得全表扫 entry_json 再逐个解析。
CREATE TABLE IF NOT EXISTS memory_evidence (
    evidence_ref TEXT NOT NULL,
    memory_id    TEXT NOT NULL,
    PRIMARY KEY (evidence_ref, memory_id)
);

CREATE INDEX IF NOT EXISTS idx_memory_owner ON memory_entries (owner, status);
CREATE INDEX IF NOT EXISTS idx_memory_kind ON memory_entries (kind, status);
CREATE INDEX IF NOT EXISTS idx_memory_status ON memory_entries (status, recorded_at_utc);
