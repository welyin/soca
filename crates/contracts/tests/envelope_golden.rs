//! 信封与单元快照的冻结样本测试。
//!
//! P0 门槛要求"冻结最小消息版本"。本文件把信封和单元快照各钉在一份检入版本库的 JSON 上：
//! 字段集合、字段名、序列化形态一旦变化，测试立刻失败。要合法地改 schema，必须先提升
//! [`soca_contracts::SCHEMA_VERSION`] 再重生成样本。
//!
//! 重生成方式（仅在确认变更是有意的之后执行）：
//!
//! ```powershell
//! $env:UPDATE_GOLDEN=1; cargo test -p soca-contracts --test envelope_golden; Remove-Item Env:UPDATE_GOLDEN
//! ```

use std::path::PathBuf;

use soca_contracts::*;

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("基准时间必须是合法 RFC 3339")
}

const BOOT: &str = "00000000-0000-4000-8000-0000000000a1";

fn boot() -> BootId {
    BootId::parse(BOOT).expect("固定 UUID 必须可解析")
}

fn golden_envelope() -> Envelope {
    let event_id = EventId::parse("11111111-1111-4111-8111-111111111111").expect("固定 UUID");
    let adapter = SourceId::new("device:file-watcher").expect("固定来源");

    Envelope::new(
        event_id,
        adapter.clone(),
        1,
        boot(),
        827,
        TaskId::new("task:42").expect("固定任务"),
        Vec::new(),
        base_time(),
        Monotonic::new(boot(), 1_000_000),
        Provenance::Sensor { adapter },
        PayloadRef::Blob {
            blob_ref: BlobRef::new("blob:obs-193").expect("固定对象"),
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            bytes: 512,
            sha256: Sha256Hex::of_bytes(b"soca-observation-193"),
        },
        PermissionScope {
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            max_action_level: ActionLevel::A1,
        },
        DataClass::Personal,
        None,
        IdempotencyKey::new("idem:task-42-seq-827").expect("固定幂等键"),
    )
}

fn golden_unit() -> UnitSnapshot {
    UnitSnapshot {
        unit_id: UnitId::new("unit:file-summary:07").expect("固定单元"),
        kind: UnitKind::Leaf,
        schema_version: SCHEMA_VERSION,
        scope: Scope {
            domain: DomainId::new("document-summary").expect("固定领域"),
            task_contract: TaskContractVersion::new("summary-v1").expect("固定合同"),
        },
        goal_refs: vec![GoalId::new("goal:42").expect("固定目标")],
        belief_revision: 18,
        belief_snapshot_ref: BlobRef::new("blob:belief-18").expect("固定对象"),
        evidence_refs: vec![
            EvidenceRef::new("obs:193").expect("固定证据引用"),
            EvidenceRef::new("tool-result:64").expect("固定证据引用"),
        ],
        relation_refs: vec![RelationRef::new("relation:source-lineage-8").expect("固定关系")],
        strategy_version: StrategyVersion::new("summary-policy-v1").expect("固定策略"),
        model_profile_ref: ModelProfileRef::new("profile:reasoning-local").expect("固定画像"),
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        budget_ref: BudgetRef::new("budget:task-42").expect("固定预算"),
        pending_action_ids: Vec::new(),
        last_applied_sequence: 827,
        state: UnitState::Cold,
    }
}

/// 比对样本；`UPDATE_GOLDEN=1` 时改为写入。
fn assert_golden<T: serde::Serialize>(file: &str, value: &T) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(file);
    let actual = serde_json::to_string_pretty(value).expect("契约类型必须可序列化");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("必须能创建 golden 目录");
        }
        std::fs::write(&path, format!("{actual}\n")).expect("必须能写入 golden 文件");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "缺少冻结样本 {}：{error}\n确认变更是有意的之后，用 UPDATE_GOLDEN=1 重生成",
            path.display()
        )
    });
    assert_eq!(
        expected.trim_end(),
        actual.trim_end(),
        "{} 与冻结样本不一致；要合法改 schema 必须先提升 SCHEMA_VERSION",
        file
    );
}

#[test]
fn envelope_schema_v1_is_frozen() {
    let envelope = golden_envelope();
    envelope.validate().expect("冻结样本必须自洽");
    assert_golden("envelope_v1.json", &envelope);

    // 冻结样本必须能原样读回来，且字段级相等。
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/envelope_v1.json");
    if std::env::var_os("UPDATE_GOLDEN").is_none() {
        let text = std::fs::read_to_string(&path).expect("冻结样本必须存在");
        let decoded: Envelope = serde_json::from_str(&text).expect("冻结样本必须可解析");
        assert_eq!(decoded, envelope);
    }
}

#[test]
fn unit_snapshot_schema_v1_is_frozen() {
    let unit = golden_unit();
    unit.validate().expect("冻结样本必须自洽");
    assert_golden("unit_snapshot_v1.json", &unit);

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/unit_snapshot_v1.json");
    if std::env::var_os("UPDATE_GOLDEN").is_none() {
        let text = std::fs::read_to_string(&path).expect("冻结样本必须存在");
        let decoded: UnitSnapshot = serde_json::from_str(&text).expect("冻结样本必须可解析");
        assert_eq!(decoded, unit);
    }
}
