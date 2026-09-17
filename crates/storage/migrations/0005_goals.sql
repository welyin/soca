-- 迁移 5：L6 受托目标栈的持久化（架构文档 §4.1 L6、§13.3）。
--
-- 为什么一个所有者一条记录、整栈存成一份 JSON，而不是按目标拆行：
--   [`GoalStack`] 的不变量是**整体**的——子目标的额度之和不得超过父目标、父目标结束时
--   子目标必须一起结束、子目标的权限不得宽于父目标。拆成多行之后，一次崩溃就可能留下
--   一个只满足了一部分不变量的中间状态，而重新拼装时没有任何东西能发现它。
--   一份文档 + 一次事务写入，让"栈要么是旧的、要么是新的"成立。
--
--   §13.3 也把目标划成每主体私有（"每主体拥有自己的完整目标、belief、邮箱、权限及记忆域"），
--   所以按所有者分片正是它的自然粒度，不需要跨所有者查询。
--
-- revision 与 updated_at_utc 不是不变量的一部分，只是给界面与审计看的：
--   用户要能回答"我刚才取消的那个目标，是什么时候被写回去的"。
--
-- [`GoalStack`]: soca_contracts::GoalStack

CREATE TABLE IF NOT EXISTS goal_stacks (
    owner          TEXT PRIMARY KEY,
    stack_json     TEXT NOT NULL,
    revision       INTEGER NOT NULL,
    updated_at_utc TEXT NOT NULL
);
