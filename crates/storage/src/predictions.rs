//! 动作前预测的独立落库（§6.3、§17）。
//!
//! §6.3 要求"单元读取授权证据，在动作前记录可检查的预测"；§17 把"预测先于动作"列为闭环
//! 正确性验收的第一条。本模块让这条要求可以不依赖时钟地被检查：
//!
//! * 预测以 `prediction_ref` 为主键独立成行；
//! * [`Store::admit_action`] 在放行任何动作之前，必须能在本表里找到被引用的预测；
//! * 于是"先预测后动作"由**存在性**保证，而不是"同一毫秒里谁排前面"。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{Prediction, TaskId, UnitId, WallClock};

use crate::error::StorageError;
use crate::Store;

/// 读回的一条预测记录。
#[derive(Debug, Clone, PartialEq)]
pub struct PredictionRecord {
    /// 写下预测的单元。
    pub unit_id: String,
    /// 所属任务。
    pub task_id: String,
    /// 预测原文。
    pub prediction: Prediction,
    /// 记录时刻。
    pub recorded_at: WallClock,
}

impl Store {
    /// 记录一条动作前预测。
    ///
    /// 返回 `true` 表示新写入；返回 `false` 表示同一 `prediction_ref` 的完全相同内容已经
    /// 存在（幂等重放）。同一引用被改写成不同内容会被拒绝：预测是"当时写下的判断"，
    /// 事后修改它就等于伪造证据。
    pub fn record_prediction(
        &mut self,
        unit_id: &UnitId,
        task_id: &TaskId,
        prediction: &Prediction,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        let reference = prediction.prediction_ref.to_string();

        if let Some(existing) = load_prediction(self.connection(), &reference)? {
            if existing.prediction == *prediction {
                return Ok(false);
            }
            return Err(StorageError::PredictionAlreadyRecorded {
                prediction_ref: reference,
            });
        }

        // 校准字段本身也要过契约校验：未校准的数值不许当概率写进库。
        if let Some(probability) = &prediction.uncertainty.probability {
            probability.validate()?;
        }

        self.connection().execute(
            "INSERT INTO predictions (
                 prediction_ref, unit_id, task_id, subject, expected_change,
                 window_start_utc, window_end_utc, failure_conditions_json,
                 prediction_json, recorded_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                reference,
                unit_id.to_string(),
                task_id.to_string(),
                prediction.subject,
                prediction.expected_change,
                prediction.window.start.to_string(),
                prediction.window.end.to_string(),
                serde_json::to_string(&prediction.failure_conditions)?,
                serde_json::to_string(prediction)?,
                at.to_string(),
            ],
        )?;
        Ok(true)
    }

    /// 按引用读取一条预测。
    pub fn prediction(&self, prediction_ref: &str) -> Result<Option<PredictionRecord>, StorageError> {
        load_prediction(self.connection(), prediction_ref)
    }

    /// 读取某个任务下的全部预测。
    pub fn predictions_for_task(
        &self,
        task_id: &TaskId,
    ) -> Result<Vec<PredictionRecord>, StorageError> {
        let mut stmt = self.connection().prepare(
            "SELECT unit_id, task_id, prediction_json, recorded_at_utc
               FROM predictions
              WHERE task_id = ?1
              ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![task_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (unit_id, task_id, prediction_json, recorded_at) = row?;
            records.push(PredictionRecord {
                unit_id,
                task_id,
                prediction: serde_json::from_str(&prediction_json)?,
                recorded_at: WallClock::from_rfc3339(&recorded_at)?,
            });
        }
        Ok(records)
    }
}

/// 在任意连接（含事务）上读取预测。供 `admit_action` 在同一事务内做存在性检查。
pub(crate) fn load_prediction(
    conn: &rusqlite::Connection,
    prediction_ref: &str,
) -> Result<Option<PredictionRecord>, StorageError> {
    let row = conn
        .query_row(
            "SELECT unit_id, task_id, prediction_json, recorded_at_utc
               FROM predictions WHERE prediction_ref = ?1",
            params![prediction_ref],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    let Some((unit_id, task_id, prediction_json, recorded_at)) = row else {
        return Ok(None);
    };

    Ok(Some(PredictionRecord {
        unit_id,
        task_id,
        prediction: serde_json::from_str(&prediction_json)?,
        recorded_at: WallClock::from_rfc3339(&recorded_at)?,
    }))
}
