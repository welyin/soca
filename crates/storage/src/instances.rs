//! 单元实例读取、拓扑世代与迁移事务（实施规格 §6、§7）。
//!
//! 三条本模块负责保证的语义：
//!
//! 1. **世代只能前进**：路由切换是 CAS。`expected_epoch` 不等于当前世代即失败，
//!    因为那说明另一个迁移已经提交过（§6.2"同分片只允许一个可执行所有者"）。
//! 2. **回退靠更大的世代**，不靠重用旧 fencing token（§6.3）。因此没有"把世代改小"的接口。
//! 3. **消息去重键是 `(unit_id, event_id)`**，与动作幂等键相互独立。一条事件被同一单元
//!    消费一次，与它是否触发过外部动作无关；把两者混成一个键，会让"重复投递"与"重复副作用"
//!    互相掩护。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{
    BlobRef, EventId, PartitionKey, ReservationId, ScaleTransaction, ScaleTransactionState,
    SubjectId, SubjectRoute, TemplateId, TopologyEpoch, TransactionId, UnitId, UnitInstance,
    UnitState, WallClock,
};

use crate::error::StorageError;
use crate::Store;

/// 单元注册信息的一行原始数据。
type RawInstanceRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
    i64,
    i64,
);

/// 注册信息查询的列顺序。读取与解码必须共用它，避免两处走偏。
const INSTANCE_COLUMNS: &str = "unit_id, subject_id, parent_id, template_id, partition_key,
                                state, state_revision, last_applied_sequence, topology_epoch";

impl Store {
    /// 读取单元注册信息。
    ///
    /// 返回 `None` 有两种含义，调用方都应视为"这个单元没有可迁移的身份"：没有这一行，
    /// 或者所有权字段缺失（例如只写过快照但没有登记）。
    pub fn instance(&self, unit_id: &UnitId) -> Result<Option<UnitInstance>, StorageError> {
        let sql = format!("SELECT {INSTANCE_COLUMNS} FROM units WHERE unit_id = ?1");
        let row = self
            .connection()
            .query_row(&sql, params![unit_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .optional()?;
        decode_instance(row)
    }

    /// 列出某个主体下的单元注册信息。
    pub fn instances_of_subject(
        &self,
        subject_id: &SubjectId,
        limit: usize,
    ) -> Result<Vec<UnitInstance>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM units WHERE subject_id = ?1 ORDER BY unit_id LIMIT ?2"
        );
        let mut stmt = self.connection().prepare(&sql)?;
        let rows = stmt.query_map(params![subject_id.to_string(), limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;

        let mut instances = Vec::new();
        for row in rows {
            if let Some(instance) = decode_instance(Some(row?))? {
                instances.push(instance);
            }
        }
        Ok(instances)
    }

    /// 推进状态修订号，返回新值。
    ///
    /// 修订号是乐观并发的依据：迁移前的快照记录了它，影子恢复时比对它。
    pub fn bump_revision(&mut self, unit_id: &UnitId, at: WallClock) -> Result<u64, StorageError> {
        let tx = self.connection_mut().transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT state_revision FROM units WHERE unit_id = ?1",
                params![unit_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            });
        };

        let next = u64::try_from(current).unwrap_or(u64::MAX).saturating_add(1);
        tx.execute(
            "UPDATE units SET state_revision = ?2, updated_at_utc = ?3 WHERE unit_id = ?1",
            params![unit_id.to_string(), next as i64, at.to_string()],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// 确保主体有一条路由记录，返回当前路由。
    pub fn ensure_route(
        &mut self,
        subject_id: &SubjectId,
        graph_ref: &BlobRef,
        at: WallClock,
    ) -> Result<SubjectRoute, StorageError> {
        self.connection().execute(
            "INSERT OR IGNORE INTO subject_routes
                 (subject_id, current_epoch, graph_ref, updated_at_utc)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                subject_id.to_string(),
                TopologyEpoch::INITIAL.get() as i64,
                graph_ref.to_string(),
                at.to_string()
            ],
        )?;
        self.route(subject_id)?
            .ok_or_else(|| StorageError::RouteNotFound {
                subject_id: subject_id.to_string(),
            })
    }

    /// 读取主体路由。
    pub fn route(&self, subject_id: &SubjectId) -> Result<Option<SubjectRoute>, StorageError> {
        let row = self
            .connection()
            .query_row(
                "SELECT current_epoch, graph_ref FROM subject_routes WHERE subject_id = ?1",
                params![subject_id.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;

        let Some((epoch, graph_ref)) = row else {
            return Ok(None);
        };
        Ok(Some(SubjectRoute {
            subject_id: subject_id.clone(),
            current_epoch: TopologyEpoch::try_from(u64::try_from(epoch).unwrap_or(0))?,
            graph_ref: BlobRef::new(graph_ref)?,
        }))
    }

    /// 用 CAS 切换主体路由世代。
    ///
    /// `expected_epoch` 必须等于当前世代，不等即失败，**不做"以新压旧"的宽容处理**：
    /// 两个迁移同时提交会让旧 actor 继续持有可执行所有权（§6.2）。
    pub fn commit_route(
        &mut self,
        subject_id: &SubjectId,
        expected_epoch: TopologyEpoch,
        new_epoch: TopologyEpoch,
        graph_ref: &BlobRef,
        at: WallClock,
    ) -> Result<SubjectRoute, StorageError> {
        let tx = self.connection_mut().transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT current_epoch FROM subject_routes WHERE subject_id = ?1",
                params![subject_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StorageError::RouteNotFound {
                subject_id: subject_id.to_string(),
            });
        };

        let current = u64::try_from(current).unwrap_or(0);
        if current != expected_epoch.get() {
            return Err(StorageError::EpochMismatch {
                subject_id: subject_id.to_string(),
                expected: expected_epoch.get(),
                actual: current,
            });
        }

        tx.execute(
            "UPDATE subject_routes SET current_epoch = ?2, graph_ref = ?3, updated_at_utc = ?4
              WHERE subject_id = ?1",
            params![
                subject_id.to_string(),
                new_epoch.get() as i64,
                graph_ref.to_string(),
                at.to_string()
            ],
        )?;
        tx.commit()?;

        Ok(SubjectRoute {
            subject_id: subject_id.clone(),
            current_epoch: new_epoch,
            graph_ref: graph_ref.clone(),
        })
    }

    /// 打开一次迁移事务。
    ///
    /// `UNIQUE(subject_id, new_epoch)` 让"同一主体同一世代只能有一个事务"由数据库保证，
    /// 而不是靠调用方的纪律。
    pub fn open_scale_transaction(
        &mut self,
        transaction: &ScaleTransaction,
        at: WallClock,
    ) -> Result<(), StorageError> {
        transaction.validate()?;
        let inserted = self.connection().execute(
            "INSERT INTO scale_transactions (
                 transaction_id, subject_id, old_epoch, new_epoch, state, plan_json,
                 resource_reservation_id, deadline_utc, created_at_utc, updated_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                transaction.transaction_id.to_string(),
                transaction.subject_id.to_string(),
                transaction.old_epoch.get() as i64,
                transaction.new_epoch.get() as i64,
                transaction.state.as_str(),
                serde_json::to_string(&transaction.plan)?,
                transaction.resource_reservation_id.to_string(),
                transaction.deadline.to_string(),
                at.to_string(),
            ],
        );

        match inserted {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StorageError::TransactionAlreadyExists {
                    subject_id: transaction.subject_id.to_string(),
                    new_epoch: transaction.new_epoch.get(),
                })
            }
            Err(other) => Err(other.into()),
        }
    }

    /// 读取一次迁移事务。
    pub fn scale_transaction(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<Option<ScaleTransaction>, StorageError> {
        let row = self
            .connection()
            .query_row(
                "SELECT subject_id, old_epoch, new_epoch, state, plan_json,
                        resource_reservation_id, deadline_utc
                   FROM scale_transactions WHERE transaction_id = ?1",
                params![transaction_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;

        let Some((subject_id, old_epoch, new_epoch, state, plan_json, reservation, deadline)) = row
        else {
            return Ok(None);
        };

        Ok(Some(ScaleTransaction {
            transaction_id: transaction_id.clone(),
            subject_id: SubjectId::new(subject_id)?,
            old_epoch: TopologyEpoch::try_from(u64::try_from(old_epoch).unwrap_or(0))?,
            new_epoch: TopologyEpoch::try_from(u64::try_from(new_epoch).unwrap_or(0))?,
            state: parse_transaction_state(&state)?,
            plan: serde_json::from_str(&plan_json)?,
            resource_reservation_id: ReservationId::new(reservation)?,
            deadline: WallClock::from_rfc3339(&deadline)?,
        }))
    }

    /// 推进迁移事务状态。
    ///
    /// 迁移合法性由契约层的状态机判定：提交点之前只能顺序前进或回滚，提交点之后只能前进
    /// 或补偿（§6.2、§6.3）。
    pub fn advance_scale_transaction(
        &mut self,
        transaction_id: &TransactionId,
        next: ScaleTransactionState,
        at: WallClock,
    ) -> Result<ScaleTransactionState, StorageError> {
        let tx = self.connection_mut().transaction()?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM scale_transactions WHERE transaction_id = ?1",
                params![transaction_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Err(StorageError::TransactionNotFound {
                transaction_id: transaction_id.to_string(),
            });
        };

        let current = parse_transaction_state(&state)?;
        if !current.can_transition_to(next) {
            return Err(StorageError::IllegalTransactionTransition {
                transaction_id: transaction_id.to_string(),
                from: current.as_str(),
                to: next.as_str(),
            });
        }

        tx.execute(
            "UPDATE scale_transactions SET state = ?2, updated_at_utc = ?3 WHERE transaction_id = ?1",
            params![transaction_id.to_string(), next.as_str(), at.to_string()],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// 登记一条已处理的消息。
    ///
    /// 返回 `false` 表示 `(unit_id, event_id)` 之前已经登记过，本次没有重复处理。
    /// 这是 §6.3 的消息去重键，与动作幂等键相互独立。
    pub fn mark_event_processed(
        &mut self,
        unit_id: &UnitId,
        event_id: &EventId,
        result_ref: Option<&str>,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        let inserted = self.connection().execute(
            "INSERT OR IGNORE INTO processed_events
                 (unit_id, event_id, result_ref, processed_at_utc)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                unit_id.to_string(),
                event_id.to_string(),
                result_ref,
                at.to_string()
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 该单元是否已经处理过这条消息。
    pub fn has_processed(
        &self,
        unit_id: &UnitId,
        event_id: &EventId,
    ) -> Result<bool, StorageError> {
        let found: Option<i64> = self
            .connection()
            .query_row(
                "SELECT 1 FROM processed_events WHERE unit_id = ?1 AND event_id = ?2",
                params![unit_id.to_string(), event_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// 已处理消息总数，用于对账。
    pub fn processed_event_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM processed_events", [], |row| row.get(0))?;
        Ok(count)
    }
}

/// 把一行原始数据解成注册信息。
///
/// 所有权字段缺失时返回 `None`：这一行可能只写过快照而没有登记，因此没有可迁移的身份。
fn decode_instance(row: Option<RawInstanceRow>) -> Result<Option<UnitInstance>, StorageError> {
    let Some((
        unit_id,
        Some(subject_id),
        parent_id,
        Some(template_id),
        Some(partition_key),
        lifecycle,
        state_revision,
        consumed_sequence,
        topology_epoch,
    )) = row
    else {
        return Ok(None);
    };

    Ok(Some(UnitInstance {
        unit_id: UnitId::new(unit_id)?,
        subject_id: SubjectId::new(subject_id)?,
        parent_id: match parent_id {
            Some(text) => Some(UnitId::new(text)?),
            None => None,
        },
        template_id: TemplateId::new(template_id)?,
        partition_key: PartitionKey::new(partition_key)?,
        lifecycle: parse_unit_state(&lifecycle)?,
        state_revision: u64::try_from(state_revision).unwrap_or(0),
        consumed_sequence: u64::try_from(consumed_sequence).unwrap_or(0),
        topology_epoch: TopologyEpoch::try_from(u64::try_from(topology_epoch).unwrap_or(0))?,
    }))
}

fn parse_unit_state(text: &str) -> Result<UnitState, StorageError> {
    UnitState::ALL
        .into_iter()
        .find(|state| state.as_str() == text)
        .ok_or(StorageError::CorruptRow {
            field: "units.state",
        })
}

fn parse_transaction_state(text: &str) -> Result<ScaleTransactionState, StorageError> {
    use ScaleTransactionState::{
        Done, Draining, Planned, Recovering, Reserved, Retiring, RolledBack, RouteCommitted,
        ShadowReady, Snapshotted,
    };
    const ALL: [ScaleTransactionState; 10] = [
        Planned,
        Reserved,
        Draining,
        Snapshotted,
        ShadowReady,
        RouteCommitted,
        Retiring,
        Done,
        RolledBack,
        Recovering,
    ];
    ALL.into_iter()
        .find(|state| state.as_str() == text)
        .ok_or(StorageError::CorruptRow {
            field: "scale_transactions.state",
        })
}
