//! schema 迁移与连接级 PRAGMA。
//!
//! §9.3 要求"SQLite 同步级别、WAL 检查点、磁盘缓存策略和备份恢复需经过断电/进程崩溃测试；
//! 只做一次内存映射 flush 不宣称实现事务耐久性"。因此 PRAGMA 在这里显式设置并有注释说明
//! 取舍，而不是靠默认值碰运气。

use rusqlite::{Connection, OptionalExtension};

use soca_contracts::WallClock;

use crate::error::StorageError;

/// 本程序支持的最新 schema 版本。
pub const LATEST_SCHEMA_VERSION: u32 = 2;

/// 迁移 1：初始表结构。
const MIGRATION_0001: &str = include_str!("../migrations/0001_init.sql");

/// 迁移 2：动作前预测独立落库。
const MIGRATION_0002: &str = include_str!("../migrations/0002_predictions.sql");

/// 迁移清单。按版本升序，只增不改：已发布的迁移一旦被编辑，旧库就会与新代码不一致。
const MIGRATIONS: &[(u32, &str)] = &[(1, MIGRATION_0001), (2, MIGRATION_0002)];

/// 打开连接后、迁移前必须设置的连接级参数。
///
/// * `journal_mode = WAL`：读写并发，写者仍只有一个（§9.3）。
/// * `synchronous = FULL`：每笔事务都 fsync。P0 阶段选择"慢但可测"，等断电测试通过、
///   并用实测数据说明取舍之后再考虑降到 NORMAL。
/// * `foreign_keys = ON`：SQLite 默认关闭外键，显式打开。
/// * `busy_timeout`：单写者下遇到瞬时锁竞争时等待而不是立刻失败。
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<(), StorageError> {
    // 赋值形式的 journal_mode 会返回新值，必须用 query_row 读掉。
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.execute("PRAGMA synchronous = FULL", [])?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    Ok(())
}

/// 读取当前 schema 版本。未迁移的库返回 `0`。
pub(crate) fn current_version(conn: &Connection) -> Result<u32, StorageError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version        INTEGER PRIMARY KEY,
             applied_at_utc TEXT NOT NULL
         )",
        [],
    )?;
    let version: Option<u32> = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    Ok(version.unwrap_or(0))
}

/// 把数据库迁移到最新版本。
///
/// 版本高于本程序支持的范围时**拒绝打开**，而不是尝试向下兼容：schema 变更必须显式提升
/// [`crate::schema::LATEST_SCHEMA_VERSION`]（与契约层的 `SCHEMA_VERSION` 同一原则）。
pub(crate) fn migrate(conn: &mut Connection, applied_at: WallClock) -> Result<u32, StorageError> {
    let found = current_version(conn)?;
    if found > LATEST_SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            found,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    let tx = conn.transaction()?;
    for (version, sql) in MIGRATIONS {
        if *version > found {
            tx.execute_batch(sql)?;
            record_version(&tx, *version, applied_at)?;
        }
    }
    tx.commit()?;
    Ok(LATEST_SCHEMA_VERSION)
}

fn record_version(
    tx: &rusqlite::Transaction<'_>,
    version: u32,
    applied_at: WallClock,
) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at_utc) VALUES (?1, ?2)",
        rusqlite::params![i64::from(version), applied_at.to_string()],
    )?;
    Ok(())
}

/// 执行一次 WAL 检查点。
///
/// §9.3 把"WAL 检查点"列为需要单独测试的项目。P0 只提供显式入口，不做后台自动检查点：
/// 谁在什么时候截断 WAL 必须是有意的动作，而不是影响面不清的后台行为。
pub(crate) fn checkpoint(conn: &Connection) -> Result<(), StorageError> {
    // 检查点会返回一行 (busy, log_frames, checkpointed_frames)，必须用 query_row 读掉。
    let outcome = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });

    match outcome {
        Ok((busy, _log_frames, _checkpointed_frames)) => {
            if busy != 0 {
                // 有读者占用导致检查点未完成。这不是错误状态，但调用方必须知道没截断。
                return Err(StorageError::WriterBusy);
            }
            Ok(())
        }
        // 内存库没有 WAL 文件，检查点不适用，不是失败。
        Err(rusqlite::Error::SqliteFailure(_, _)) => Ok(()),
        Err(other) => Err(other.into()),
    }
}
