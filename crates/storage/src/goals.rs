//! L6 目标栈的持久化（§4.1 L6、§13.3）。
//!
//! 一个所有者一条记录、整栈一份文档。理由见 `migrations/0005_goals.sql`：目标栈的不变量是
//! 整体的，拆行存储会让崩溃后重新拼装出的东西可能只是一部分满足不变量。
//!
//! 写入前**必然**调用 [`GoalStack::validate`]。这不是多余的：一份从别处（旧版本、测试、
//! 手改过的库）拿来的栈，可能带着"父目标已结束而子目标还开着"这种状态，而那种状态一旦
//! 落库，下一次读出来就被当成正常输入了。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{GoalStack, SubjectId, WallClock};

use crate::error::StorageError;
use crate::Store;

impl Store {
    /// 写入某个所有者的目标栈，返回新的修订号。
    pub fn save_goal_stack(
        &mut self,
        stack: &GoalStack,
        at: WallClock,
    ) -> Result<u64, StorageError> {
        stack.validate()?;

        let current = self.goal_stack_revision(stack.owner())?;
        let revision = current.unwrap_or(0).saturating_add(1);
        let json = serde_json::to_string(stack)?;

        self.connection().execute(
            "INSERT INTO goal_stacks (owner, stack_json, revision, updated_at_utc)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner) DO UPDATE SET
                 stack_json = excluded.stack_json,
                 revision = excluded.revision,
                 updated_at_utc = excluded.updated_at_utc",
            params![
                stack.owner().to_string(),
                json,
                i64::try_from(revision).unwrap_or(i64::MAX),
                at.to_string(),
            ],
        )?;
        Ok(revision)
    }

    /// 读取某个所有者的目标栈。从未写入过时返回 `None`。
    pub fn goal_stack(&self, owner: &SubjectId) -> Result<Option<GoalStack>, StorageError> {
        let json: Option<String> = self
            .connection()
            .query_row(
                "SELECT stack_json FROM goal_stacks WHERE owner = ?1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(serde_json::from_str(&text)?)),
            None => Ok(None),
        }
    }

    /// 读取某个所有者目标栈的修订号。
    pub fn goal_stack_revision(&self, owner: &SubjectId) -> Result<Option<u64>, StorageError> {
        let revision: Option<i64> = self
            .connection()
            .query_row(
                "SELECT revision FROM goal_stacks WHERE owner = ?1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(revision.map(|value| value as u64))
    }

    /// 读取目标栈上次写入的时刻。
    pub fn goal_stack_updated_at(
        &self,
        owner: &SubjectId,
    ) -> Result<Option<WallClock>, StorageError> {
        let stamp: Option<String> = self
            .connection()
            .query_row(
                "SELECT updated_at_utc FROM goal_stacks WHERE owner = ?1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match stamp {
            Some(text) => Ok(Some(WallClock::from_rfc3339(&text)?)),
            None => Ok(None),
        }
    }
}
