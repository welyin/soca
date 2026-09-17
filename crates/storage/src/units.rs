//! 单元目录：注册、所有权与运行快照（§3.2、§9.2、§9.3；实施规格 §7）。
//!
//! §9.3 把"单元目录、目标、权限引用、动作状态、游标、事件元数据"归入同一个 SQLite 库，
//! §7 进一步给出 `unit_instances` 的字段。本模块把两份要求落在**同一行**上：
//!
//! | 列 | 含义 | 谁在读 |
//! |---|---|---|
//! | `subject_id`、`parent_id`、`template_id`、`partition_key` | 所有权与可迁移性 | 迁移协调器 |
//! | `state_revision`、`topology_epoch` | 乐观并发与世代 | 协调器、fencing |
//! | `state`、`last_applied_sequence` | 生命周期与游标 | 注册信息与快照共用的**同一列** |
//! | `snapshot_json` | 运行状态（§3.2） | 单元自己 |
//!
//! 两条不变量：
//!
//! 1. `state` 与 `last_applied_sequence` 各只有一列，注册信息与快照读的是同一份数据，
//!    因此不可能出现"注册说它在跑、快照说它是冷的"。
//! 2. 登记必须先于写入快照。§9.2 的冷态"只在注册表和持久邮箱中存在"，所以一个没有注册
//!    记录的快照就是一个没有身份的槽。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{UnitId, UnitInstance, UnitSnapshot, UnitState, WallClock};

use crate::error::StorageError;
use crate::Store;

impl Store {
    /// 登记一个冷实例。
    ///
    /// 必须同时给出初始运行快照：登记的是一份可恢复的状态，而不是一个空壳。
    /// 返回 `false` 表示该标识已经登记过（幂等）。
    pub fn register_instance(
        &mut self,
        snapshot: &UnitSnapshot,
        instance: &UnitInstance,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        snapshot.validate()?;
        instance.validate()?;

        if snapshot.unit_id != instance.unit_id {
            return Err(StorageError::InstanceInconsistent {
                unit_id: instance.unit_id.to_string(),
                reason: "快照与注册信息的 unit_id 不一致",
            });
        }
        // §9.2："冷态只在注册表和持久邮箱中存在"。刚登记就自称在跑的单元，其"运行"没有承载物。
        if snapshot.state != UnitState::Cold || instance.lifecycle != UnitState::Cold {
            return Err(StorageError::UnitMustBeColdAtRegistration {
                state: instance.lifecycle.as_str(),
            });
        }
        // 游标是同一列，两边必须给出同一个值；不一致说明调用方在拼两套状态。
        if snapshot.last_applied_sequence != instance.consumed_sequence {
            return Err(StorageError::InstanceInconsistent {
                unit_id: instance.unit_id.to_string(),
                reason: "快照游标与注册信息消费游标不一致",
            });
        }

        if self.instance(&instance.unit_id)?.is_some() {
            return Ok(false);
        }

        insert_unit_row(self.connection(), snapshot, instance, at)?;
        Ok(true)
    }

    /// 覆盖写入运行快照。
    ///
    /// 只更新快照列以及与快照共享的生命周期与游标列；所有权列（模板、分片、世代）保持不变。
    /// 未登记的单元不能写快照——那会让"这个槽是谁的"无从回答。
    pub fn save_unit(&mut self, snapshot: &UnitSnapshot, at: WallClock) -> Result<(), StorageError> {
        snapshot.validate()?;
        let updated = update_snapshot_columns(self.connection(), snapshot, at)?;
        if updated == 0 {
            return Err(StorageError::UnitNotRegistered {
                unit_id: snapshot.unit_id.to_string(),
            });
        }
        Ok(())
    }

    /// 读取运行快照。
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

    /// 按状态列出运行快照。
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
        let rows = stmt.query_map(params![state.as_str(), limit as i64], |row| {
            row.get::<_, String>(0)
        })?;

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

    /// 处于热态的单元数。
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
}

pub(crate) fn insert_unit_row(
    conn: &rusqlite::Connection,
    snapshot: &UnitSnapshot,
    instance: &UnitInstance,
    at: WallClock,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO units (
             unit_id, kind, scope_domain, scope_task_contract, state, belief_revision,
             last_applied_sequence, strategy_version, model_profile_ref, capability_policy_ref,
             budget_ref, snapshot_json, updated_at_utc,
             subject_id, parent_id, template_id, partition_key, state_revision, topology_epoch
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
         )",
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
            instance.subject_id.to_string(),
            instance.parent_id.as_ref().map(ToString::to_string),
            instance.template_id.to_string(),
            instance.partition_key.to_string(),
            instance.state_revision as i64,
            instance.topology_epoch.get() as i64,
        ],
    )?;
    Ok(())
}

pub(crate) fn update_snapshot_columns(
    conn: &rusqlite::Connection,
    snapshot: &UnitSnapshot,
    at: WallClock,
) -> Result<usize, StorageError> {
    let updated = conn.execute(
        "UPDATE units SET
             kind                  = ?2,
             scope_domain          = ?3,
             scope_task_contract   = ?4,
             state                 = ?5,
             belief_revision       = ?6,
             last_applied_sequence = ?7,
             strategy_version      = ?8,
             model_profile_ref     = ?9,
             capability_policy_ref = ?10,
             budget_ref            = ?11,
             snapshot_json         = ?12,
             updated_at_utc        = ?13
         WHERE unit_id = ?1",
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
    Ok(updated)
}

fn unit_kind_str(kind: soca_contracts::UnitKind) -> &'static str {
    match kind {
        soca_contracts::UnitKind::Leaf => "leaf",
        soca_contracts::UnitKind::Cluster => "cluster",
        soca_contracts::UnitKind::Subject => "subject",
    }
}
