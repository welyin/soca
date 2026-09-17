//! 单元目录（§3.2、§9.2、§9.3）。
//!
//! §9.3 把"单元目录、目标、权限引用、动作状态、游标、事件元数据"归入同一个 SQLite 库。
//! 本模块负责其中的单元目录部分。
//!
//! 一条关键约定：`units.state` 列与 `snapshot_json.state` 必须一致。列存在的意义是让
//! 资源控制器能便宜地统计"现在有多少热单元"，而不是每行都解析一遍 JSON；一旦两者可以
//! 不一致，统计就会说谎。因此本模块的写入路径只从快照取状态，不接受调用方单独传一个状态。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{UnitId, UnitSnapshot, UnitState, WallClock};

use crate::error::StorageError;
use crate::Store;

impl Store {
    /// 登记一个冷单元。
    ///
    /// §9.2 明确"冷态只在注册表和持久邮箱中存在"，因此登记进来的单元必须是 `COLD`：
    /// 一个刚登记就声称自己 `RUNNING` 的单元，其"运行"没有任何承载物。
    pub fn register_unit(
        &mut self,
        snapshot: &UnitSnapshot,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        snapshot.validate()?;
        if snapshot.state != UnitState::Cold {
            return Err(StorageError::UnitMustBeColdAtRegistration {
                state: snapshot.state.as_str(),
            });
        }
        if self.unit(&snapshot.unit_id)?.is_some() {
            return Ok(false);
        }
        self.write_unit(snapshot, at)?;
        Ok(true)
    }

    /// 写入（或覆盖）单元快照。
    ///
    /// 覆盖是刻意的：快照就是单元的持久身份，checkpoint 就是覆盖它。但状态列与 JSON 里的
    /// 状态始终取自同一份数据。
    pub fn save_unit(&mut self, snapshot: &UnitSnapshot, at: WallClock) -> Result<(), StorageError> {
        snapshot.validate()?;
        self.write_unit(snapshot, at)?;
        Ok(())
    }

    /// 读取单元快照。
    pub fn unit(&self, unit_id: &UnitId) -> Result<Option<UnitSnapshot>, StorageError> {
        let json: Option<String> = self
            .connection()
            .query_row(
                "SELECT snapshot_json FROM units WHERE unit_id = ?1",
                params![unit_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(serde_json::from_str(&text)?)),
            None => Ok(None),
        }
    }

    /// 按状态列出单元。
    pub fn units_in_state(
        &self,
        state: UnitState,
        limit: usize,
    ) -> Result<Vec<UnitSnapshot>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.connection().prepare(
            "SELECT snapshot_json FROM units WHERE state = ?1 ORDER BY unit_id LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![state.as_str(), limit as i64],
            |row| row.get::<_, String>(0),
        )?;

        let mut units = Vec::new();
        for row in rows {
            units.push(serde_json::from_str(&row?)?);
        }
        Ok(units)
    }

    /// 单元总数。
    pub fn unit_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM units", [], |row| row.get(0))?;
        Ok(count)
    }

    /// 处于热态（LOADING/READY/RUNNING/WAITING/CHECKPOINTING）的单元数。
    ///
    /// 资源控制器按它做准入，因此不能靠"把所有快照读出来再数一遍"。
    pub fn hot_unit_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM units
              WHERE state IN ('LOADING', 'READY', 'RUNNING', 'WAITING', 'CHECKPOINTING')",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    fn write_unit(&self, snapshot: &UnitSnapshot, at: WallClock) -> Result<(), StorageError> {
        self.connection().execute(
            "INSERT INTO units (
                 unit_id, kind, scope_domain, scope_task_contract, state, belief_revision,
                 last_applied_sequence, strategy_version, model_profile_ref, capability_policy_ref,
                 budget_ref, snapshot_json, updated_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(unit_id) DO UPDATE SET
                 kind                  = excluded.kind,
                 scope_domain          = excluded.scope_domain,
                 scope_task_contract   = excluded.scope_task_contract,
                 state                 = excluded.state,
                 belief_revision       = excluded.belief_revision,
                 last_applied_sequence = excluded.last_applied_sequence,
                 strategy_version      = excluded.strategy_version,
                 model_profile_ref     = excluded.model_profile_ref,
                 capability_policy_ref = excluded.capability_policy_ref,
                 budget_ref            = excluded.budget_ref,
                 snapshot_json         = excluded.snapshot_json,
                 updated_at_utc        = excluded.updated_at_utc",
            params![
                snapshot.unit_id.to_string(),
                unit_kind_str(snapshot.kind),
                snapshot.scope.domain.to_string(),
                snapshot.scope.task_contract.to_string(),
                snapshot.state.as_str(),
                snapshot.belief_revision as i64,
                snapshot.last_applied_sequence as i64,
                snapshot.strategy_version.to_string(),
                snapshot.model_profile_ref.to_string(),
                snapshot.capability_policy_ref.to_string(),
                snapshot.budget_ref.to_string(),
                serde_json::to_string(snapshot)?,
                at.to_string(),
            ],
        )?;
        Ok(())
    }
}

fn unit_kind_str(kind: soca_contracts::UnitKind) -> &'static str {
    match kind {
        soca_contracts::UnitKind::Leaf => "leaf",
        soca_contracts::UnitKind::Cluster => "cluster",
        soca_contracts::UnitKind::Subject => "subject",
    }
}
