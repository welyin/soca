-- 迁移 5：人工批准的持久化（架构文档 §12.1、§12.2）。
--
-- 为什么批准必须落盘，而不能只是一个进程内的对象：
--
--   §12.1 要求"A2/A3 动作**必须能追溯到一次明确的用户批准**"。一次批准签发之后，
--   消耗它的是一个执行许可；而许可、动作、回执都会写进动作账并长期保留。如果批准本身
--   重启就没了，那么账上留下的 `approval_id` 就是一个悬空引用——追溯链恰好断在最需要
--   它回答的那个问题上："这次写入是谁批的？"
--
-- 结构与 memory_entries 同一条思路：**可变的那部分只以列为准**。
--
--   approval_json 保存批准的全部绑定（工具、范围、参数摘要、等级、TTL、通道），写入后
--   不再改动；used 会被消费递增，因此它只写在列上。两处都存同一份计数必然分叉，
--   而分叉的方向恰好危险：JSON 里是 0、列里是 1 时，读的人以为还能再用一次。
--
-- 这里**不**给已消费的批准建"历史表"。消费次数本身就是历史（used 从 0 走到 max_uses），
-- 而每一次消费都对应动作账里的一条记录——那条记录才是"这一次是谁批的"的答案。

CREATE TABLE IF NOT EXISTS approvals (
    approval_id       TEXT PRIMARY KEY,
    subject_id        TEXT NOT NULL,
    max_action_level  TEXT NOT NULL,
    tool_id           TEXT,
    object_scope      TEXT,
    parameters_digest TEXT,
    granted_at_utc    TEXT NOT NULL,
    expires_at_utc    TEXT,
    channel           TEXT NOT NULL,
    max_uses          INTEGER NOT NULL,
    -- 唯一的可变列。
    used              INTEGER NOT NULL,
    approval_json     TEXT NOT NULL
);

-- 按主体列出仍然可用的批准。这个索引服务的是一次真实查询：许可签发时要找"有没有一份
-- 覆盖这次动作的批准"。没有它，每次签发都要全表扫再逐个解析 JSON。
CREATE INDEX IF NOT EXISTS idx_approvals_subject ON approvals (subject_id, expires_at_utc);
