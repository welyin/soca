-- 迁移 8：事件的权限范围（架构文档 §12.1、§12.3）。
--
-- 为什么要冗余成列：§12.1 对 A1 的要求是"范围限定授权，**撤回立即生效**"，而"撤回"要回答
-- 一个反查问题——**哪些事件是在这个授权下产生的**。信封里本来就有这份信息
-- （`permission_scope.capability_policy_ref`），但它只存在 JSON 里；反查就得把整张事件表
-- 读出来逐个解析，而事件表是只增不减的。`instruction_authority` 那一列是同一个理由下的先例，
-- 表定义里写得很清楚："冗余成列，使审计查询不必解析 JSON"。
--
-- 回填用 json_extract 而不是留空。迁移完成之后，"哪些事件属于哪个授权"这个问题不能有
-- 第二份答案：不回填的话，升级之前的事件全都归不了档，而撤回会让它们**静默地**逃过失效
-- 传播。那种缺口不会报错，只会让一份本该失效的记忆继续被检索到——正是这一节要防的事。
--
-- `json_extract` 是 SQLite 的内建函数（3.38 起进入核心），rusqlite 的 bundled 版本远新于它。

ALTER TABLE events ADD COLUMN capability_policy_ref TEXT;

UPDATE events
   SET capability_policy_ref = json_extract(
           envelope_json, '$.permission_scope.capability_policy_ref'
       )
 WHERE capability_policy_ref IS NULL;

CREATE INDEX IF NOT EXISTS idx_events_capability
    ON events (capability_policy_ref, sequence);
