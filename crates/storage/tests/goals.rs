//! L6 目标栈持久化的回归测试（§4.1 L6、§13.3）。

use soca_contracts::{
    ActionLevel, CapabilityPolicyRef, ContractError, ExplorationQuota, GoalBudget, GoalId,
    GoalStack, GoalState, PermissionScope, Provenance, SubjectId, UnitId, UserChannel, WallClock,
};
use soca_storage::{StorageError, Store};

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn owner() -> SubjectId {
    SubjectId::new("user:alice").expect("固定主体")
}

fn goal_id(name: &str) -> GoalId {
    GoalId::new(format!("goal:{name}")).expect("固定目标")
}

fn scope(level: ActionLevel) -> PermissionScope {
    PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        max_action_level: level,
    }
}

fn user_provenance() -> Provenance {
    Provenance::User {
        channel: UserChannel::Chat,
    }
}

fn populated() -> GoalStack {
    let mut stack = GoalStack::new(owner());
    stack
        .delegate(
            goal_id("summary"),
            "为已授权目录生成摘要",
            user_provenance(),
            scope(ActionLevel::A2),
            GoalBudget::new(8, 16, 4096, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(4),
            at(0),
            Some(at(3600)),
        )
        .expect("委托");
    stack
        .transition(&goal_id("summary"), GoalState::Active)
        .expect("受理");
    stack
        .decompose(
            goal_id("child"),
            &goal_id("summary"),
            "先核对文件版本",
            UnitId::new("unit:file-version").expect("固定单元"),
            scope(ActionLevel::A1),
            GoalBudget::new(2, 4, 1024, 600_000).expect("合法额度"),
            ExplorationQuota::new(1),
            at(0),
            Some(at(3600)),
        )
        .expect("拆分");
    stack
}

#[test]
fn an_absent_stack_reads_back_as_none() {
    let store = Store::open_in_memory(at(0)).expect("内存存储");
    assert!(store.goal_stack(&owner()).expect("读取").is_none());
    assert!(store.goal_stack_revision(&owner()).expect("读取").is_none());
}

#[test]
fn a_delegated_goal_survives_a_restart() {
    // "关闭程序再打开，目标还在"是用户能直接看见的行为，所以它值得一个跨进程的测试，
    // 而不是只测内存里的那份结构。
    let dir = tempfile::TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");
    let stack = populated();

    {
        let mut store = Store::open(&path, at(0)).expect("首次打开");
        assert_eq!(store.save_goal_stack(&stack, at(0)).expect("写入"), 1);
    }

    let store = Store::open(&path, at(3600)).expect("再次打开");
    let restored = store
        .goal_stack(&owner())
        .expect("读取")
        .expect("目标栈还在");

    assert_eq!(restored, stack, "整栈原样回来，包括父子关系与额度用量");
    assert_eq!(restored.len(), 2);
    assert_eq!(
        restored.goal(&goal_id("summary")).expect("存在").state,
        GoalState::Active
    );
    assert_eq!(
        restored
            .goal(&goal_id("child"))
            .expect("存在")
            .depth,
        1
    );
    assert_eq!(
        restored
            .goal(&goal_id("child"))
            .expect("存在")
            .permission_scope
            .max_action_level,
        ActionLevel::A1,
        "权限收窄必须被持久化，不能读回来变成父目标的等级"
    );
    restored.validate().expect("读回来的栈仍然自洽");
}

#[test]
fn the_revision_increases_on_every_save() {
    let mut store = Store::open_in_memory(at(0)).expect("内存存储");
    let mut stack = populated();

    assert_eq!(store.save_goal_stack(&stack, at(0)).expect("写入"), 1);
    assert_eq!(store.save_goal_stack(&stack, at(1)).expect("写入"), 2);
    assert_eq!(
        store.goal_stack_revision(&owner()).expect("读取"),
        Some(2)
    );
    assert_eq!(
        store.goal_stack_updated_at(&owner()).expect("读取"),
        Some(at(1))
    );

    // 放弃一个目标之后再存一次，读回来应当也是放弃之后的样子。
    stack.abandon(&goal_id("summary")).expect("放弃");
    assert_eq!(store.save_goal_stack(&stack, at(2)).expect("写入"), 3);
    let restored = store.goal_stack(&owner()).expect("读取").expect("存在");
    assert!(
        restored
            .goal(&goal_id("summary"))
            .expect("存在")
            .is_terminal()
    );
    assert!(
        restored
            .goal(&goal_id("child"))
            .expect("存在")
            .is_terminal(),
        "放弃父目标必须连带子目标，而这一点要能从库里读回来"
    );
}

#[test]
fn saving_validates_the_stack_before_it_reaches_the_database() {
    // 一份带着"父目标已结束、子目标仍开着"的栈一旦落库，下一次读出来就会被当成正常输入。
    // 所以写入前必须过一遍校验——校验不是可选的卫生习惯，它是唯一能拦住这种状态的地方。
    let mut store = Store::open_in_memory(at(0)).expect("内存存储");
    let mut stack = populated();
    stack
        .goal_mut(&goal_id("summary"))
        .expect("存在")
        .state = GoalState::Abandoned;

    let result = store.save_goal_stack(&stack, at(0));
    assert!(matches!(result, Err(StorageError::Contract(ContractError::FailClosed(_)))));
    assert!(
        store.goal_stack(&owner()).expect("读取").is_none(),
        "被拒绝的栈不留下半截记录"
    );
}

#[test]
fn stacks_are_scoped_to_the_owner() {
    // §13.3：每主体拥有自己的目标域。写一个所有者的栈不会影响另一个。
    let mut store = Store::open_in_memory(at(0)).expect("内存存储");
    let alice_stack = populated();
    store.save_goal_stack(&alice_stack, at(0)).expect("写入");

    let bob = SubjectId::new("user:bob").expect("固定主体");
    assert!(store.goal_stack(&bob).expect("读取").is_none());

    let mut bob_stack = GoalStack::new(bob.clone());
    bob_stack
        .delegate(
            goal_id("bob-task"),
            "为 bob 生成摘要",
            user_provenance(),
            scope(ActionLevel::A1),
            GoalBudget::new(2, 4, 1024, 600_000).expect("合法额度"),
            ExplorationQuota::new(1),
            at(0),
            None,
        )
        .expect("委托");
    store.save_goal_stack(&bob_stack, at(0)).expect("写入");

    assert_eq!(
        store.goal_stack(&owner()).expect("读取").expect("存在").len(),
        2,
        "alice 的栈没被 bob 的写入影响"
    );
    assert_eq!(
        store.goal_stack(&bob).expect("读取").expect("存在").len(),
        1
    );
    // 两个所有者各自的修订号独立计数。
    assert_eq!(store.goal_stack_revision(&owner()).expect("读取"), Some(1));
    assert_eq!(store.goal_stack_revision(&bob).expect("读取"), Some(1));
}

#[test]
fn rewriting_a_stack_replaces_rather_than_merges() {
    // 整栈一份文档意味着"写入即整体替换"。这一点需要被钉住：如果实现改成了合并，
    // 那么一个被放弃的目标会因为"没在新文档里"而奇怪地复活。
    let mut store = Store::open_in_memory(at(0)).expect("内存存储");
    let stack = populated();
    store.save_goal_stack(&stack, at(0)).expect("写入");

    let mut shrunk = GoalStack::new(owner());
    shrunk
        .delegate(
            goal_id("only-one"),
            "只剩一个目标",
            user_provenance(),
            scope(ActionLevel::A2),
            GoalBudget::new(8, 16, 4096, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(1),
            at(1),
            None,
        )
        .expect("委托");
    store.save_goal_stack(&shrunk, at(1)).expect("写入");

    let restored = store.goal_stack(&owner()).expect("读取").expect("存在");
    assert_eq!(restored.len(), 1);
    assert!(restored.goal(&goal_id("summary")).is_none());
    assert!(restored.goal(&goal_id("only-one")).is_some());
}

#[test]
fn a_goal_serializes_to_exactly_the_documented_fields() {
    // 目标里不该出现工具句柄、密钥或模型权重。§3.2 对单元快照的做法是把字段集合钉成一份
    // 清单，这里用同样的办法：往 `Goal` 上加一个能放句柄的字段，这条测试就会失败。
    let goal = populated().goal(&goal_id("summary")).expect("存在").clone();
    let value = serde_json::to_value(&goal).expect("可序列化");
    let object = value.as_object().expect("目标是一个 JSON 对象");

    let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec![
            "budget",
            "created_at",
            "deadline",
            "depth",
            "exploration",
            "goal_id",
            "origin",
            "owner",
            "parent",
            "permission_scope",
            "state",
            "statement",
        ],
        "目标字段集合发生变化：若是新增字段，请确认它不可能承载句柄或密钥"
    );

    // 出处只有两种来源，都必须能指认到具体的人或具体的父目标。
    let delegated = populated()
        .goal(&goal_id("child"))
        .expect("存在")
        .origin
        .clone();
    assert_eq!(delegated.as_str(), "decomposed");
}

#[test]
fn an_expired_goal_is_still_loadable() {
    // 过期不是畸形。把两者混起来，一个持久化的目标栈会在截止时间到点之后再也读不回来——
    // 而那正是"长期未完成的目标"最常见的样子。
    let mut store = Store::open_in_memory(at(0)).expect("内存存储");
    let stack = populated();
    store.save_goal_stack(&stack, at(0)).expect("写入");

    let restored = store.goal_stack(&owner()).expect("读取").expect("存在");
    restored.validate().expect("过期不影响自洽性");
    let goal = restored.goal(&goal_id("summary")).expect("存在");
    assert!(goal.is_expired_at(at(7200)), "确实已经过期");
    assert!(
        !goal.is_terminal(),
        "过期不等于自动放弃：否则「我刚回来，任务就没了」会变成默认行为"
    );
}
