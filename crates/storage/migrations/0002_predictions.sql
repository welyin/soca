-- 迁移 2：动作前预测必须独立落库（架构文档 §6.3、§7.2、§17）。
--
-- 为什么需要这张表，而不是把预测塞进动作行里：
--   §17 的闭环正确性验收要求"预测先于动作"。如果预测只作为 `ActionIntent` 的一个字段随
--   意图一起写库，那么"先"这个字就无从判定——两者是同一行，同一个时刻。
--   把预测做成独立行之后，"先"不再靠时间戳比较，而是靠**外键存在性**保证：
--   `admit_action` 在放行前必须能在本表里找到 `intent.prediction_ref`，否则动作不予受理。
--   这是构造性保证，不依赖时钟，也不依赖"同一毫秒内谁在前"这种不可判定问题。
--
-- 本表同时是重放所需的输入：重放要核对"动作实际结果是否落在预测的窗口与失败条件内"，
-- 就必须读回当初写下的预测原文。

CREATE TABLE IF NOT EXISTS predictions (
    prediction_ref          TEXT PRIMARY KEY,
    unit_id                 TEXT NOT NULL,
    task_id                 TEXT NOT NULL,
    -- 预测对象与预计变化。§6.3 要求两者都可检查。
    subject                 TEXT NOT NULL,
    expected_change         TEXT NOT NULL,
    -- 预期成立的时间窗。
    window_start_utc        TEXT NOT NULL,
    window_end_utc          TEXT NOT NULL,
    -- 失败条件。没有它预测无法证伪，契约层已经拒绝构造，这里再存一份原文供重放核对。
    failure_conditions_json TEXT NOT NULL,
    prediction_json         TEXT NOT NULL,
    recorded_at_utc         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_predictions_task ON predictions (task_id, recorded_at_utc);
CREATE INDEX IF NOT EXISTS idx_predictions_unit ON predictions (unit_id);
