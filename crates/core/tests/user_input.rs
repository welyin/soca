//! §6 第 1 步前半句的回归测试：**用户输入写成事件**。
//!
//! 在此之前这一句只对"设备结果"成立——观测会写成事件，而用户的话只活在目标陈述里。
//! 那不是"存下来了"，那是"被引用过一次"：目标改了、结束了、被放弃了，那句话就再没有地方
//! 可以回看；§7.1 的信封（来源、时刻、权限范围、数据类别）更是完全没施加到它身上。
//!
//! 本文件盯的另一件事是**方向**：用户输入是整个系统里唯一一个"是指令"的出处（§6.1／§11.1），
//! 而判断它是不是指令的依据只有一处。写反了，一段被拍到的文字就能冒充用户授权。

use soca_contracts::{
    ActionLevel, CapabilityPolicyRef, DataClass, ExplorationQuota, GoalBudget, ModelBackend,
    ModelBudget, ModelVersion, PayloadRef, UserChannel, WallClock,
};
use soca_core::{ActionBroker, RetentionPolicy, SimulatedOs, Subject};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";
const CAPABILITY: &str = "cap:read-selected-folder";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn scope(level: ActionLevel) -> soca_contracts::PermissionScope {
    soca_contracts::PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new(CAPABILITY).expect("固定能力策略"),
        max_action_level: level,
    }
}

fn budget() -> GoalBudget {
    GoalBudget::new(16, 32, 8192, 3_600_000).expect("合法额度")
}

fn subject() -> Subject {
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(SimulatedOs::new()),
        DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇"),
        soca_contracts::SubjectId::new("user:local").expect("固定主体"),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
        Box::new(DeterministicTransport::new(Vec::new())),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("装配主体")
}

/// 取出正文，忽略事件元数据。保留期把正文清掉之后那些位置的 `text` 是 `None`。
fn texts(subject: &Subject, limit: usize) -> Vec<String> {
    subject
        .user_inputs(limit)
        .expect("读回")
        .into_iter()
        .filter_map(|input| input.text)
        .collect()
}

fn delegate(subject: &mut Subject, text: &str, channel: UserChannel, level: ActionLevel) -> bool {
    subject
        .delegate(
            text,
            channel,
            scope(level),
            budget(),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .is_ok()
}

// ---------------------------------------------------------------------------
// 输入成为事件
// ---------------------------------------------------------------------------

#[test]
fn a_users_message_becomes_an_event_with_the_envelope_it_should_have() {
    let mut subject = subject();
    assert!(delegate(&mut subject, "把摘要写进已授权目录", UserChannel::Chat, ActionLevel::A2));

    let events = subject.store().read_events_after(0, 16).expect("读事件");
    assert_eq!(events.len(), 1, "一次用户输入先写成一条事件");

    let envelope = &events[0].envelope;
    assert_eq!(envelope.source_id.as_str(), "channel:chat");
    assert!(
        envelope.provenance.is_instruction_authority(),
        "用户在明确通道上的直接输入就是指令来源"
    );
    assert_eq!(envelope.permission_scope.max_action_level, ActionLevel::A2);
    assert_eq!(envelope.data_class, DataClass::Personal);
    assert!(
        envelope.causal_parent_ids.is_empty(),
        "用户输入没有因果父事件——它是这条链的起点"
    );
}

#[test]
fn the_message_content_lives_in_the_content_store_not_in_the_ledger() {
    // 内容进内容仓、信封只带引用，**哪怕只有一句话也一样**。理由不是大小：§12.3 给对话与
    // 转写定了保留期，而保留期要能把内容撤下来。内联在事件行上的话，"删掉这段对话"就得
    // 改写一条 append-only 的记录——那不是删除，那是篡改历史。
    let mut subject = subject();
    let message = "这是一句不该被写进账本正文的话";
    assert!(delegate(&mut subject, message, UserChannel::Chat, ActionLevel::A1));

    let events = subject.store().read_events_after(0, 8).expect("读事件");
    let PayloadRef::Blob {
        blob_ref, bytes, ..
    } = &events[0].envelope.payload_ref
    else {
        panic!("用户输入必须走内容仓：{:?}", events[0].envelope.payload_ref);
    };
    assert_eq!(*bytes as usize, message.len());
    assert!(
        !serde_json::to_string(&events[0].envelope)
            .expect("可序列化")
            .contains(message),
        "信封里不该有正文"
    );

    // 而它确实在内容仓里，读得回来。
    let stored = subject
        .store()
        .read_content(subject.content(), blob_ref)
        .expect("读内容");
    assert_eq!(String::from_utf8_lossy(&stored), message);
}

#[test]
fn an_event_is_written_even_when_the_goal_cannot_be_created() {
    // 话是用户说的，这件事已经发生了。"我们没能把它变成一个任务"是另一件事，不该让前者
    // 消失——否则用户会说第二遍，而系统会以为自己第一次没听见。
    let mut subject = subject();
    assert!(
        !delegate(&mut subject, "   ", UserChannel::Chat, ActionLevel::A1),
        "空白陈述建不出目标"
    );

    assert_eq!(texts(&subject, 8), vec!["   "]);
    assert_eq!(subject.goals().len(), 0);
}

#[test]
fn different_channels_get_separate_event_streams() {
    // 序号按（来源，epoch，boot）分配。共用一条流的话，两个通道的序号会互相穿插，
    // 而 §7.1 的"同设备同 epoch 内用序列号排序"就不再成立。
    let mut subject = subject();
    assert!(delegate(&mut subject, "甲", UserChannel::Chat, ActionLevel::A1));
    assert!(delegate(&mut subject, "乙", UserChannel::ApprovalUi, ActionLevel::A1));

    let sources: Vec<String> = subject
        .store()
        .read_events_after(0, 8)
        .expect("读事件")
        .iter()
        .map(|event| event.envelope.source_id.to_string())
        .collect();
    assert_eq!(sources, vec!["channel:chat", "channel:approval_ui"]);
    assert_eq!(texts(&subject, 8), vec!["甲", "乙"]);
}

// ---------------------------------------------------------------------------
// 方向
// ---------------------------------------------------------------------------

#[test]
fn a_sensor_observation_is_not_returned_as_a_user_message() {
    // §6.1／§11.1：屏幕文字、麦克风转写、文档内容都不是指令来源。把它们混进"用户说过的话"
    // 里，等于让一段被拍到的文字冒充用户授权。
    let mut subject = subject();
    subject
        .observe(WATCHED, DataClass::Personal, at(0))
        .expect("观测");
    assert!(delegate(&mut subject, "真的用户输入", UserChannel::Chat, ActionLevel::A1));

    assert_eq!(texts(&subject, 16), vec!["真的用户输入"]);

    // 反向断言：那条观测确实进了事件账，只是它不是用户输入。
    let events = subject.store().read_events_after(0, 16).expect("读事件");
    assert_eq!(events.len(), 2, "观测和用户输入各写了一条");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.envelope.provenance.is_instruction_authority())
            .count(),
        1,
        "账上只有一条有指令权限"
    );
}

#[test]
fn an_expired_conversation_keeps_its_event_but_loses_its_content() {
    // §12.3："对话与转写 | 本地可配置保留，初值 7 天。"
    //
    // 到期之后**事件还在**——"用户说过这句话"这件事发生过，抹掉它等于改写历史；走掉的是内容。
    // 而读回来是 `text: None` 而不是一个错误：一段按约定到期的对话不是故障。
    let mut subject = subject();
    assert!(delegate(&mut subject, "说过的话", UserChannel::Chat, ActionLevel::A1));
    assert_eq!(texts(&subject, 8), vec!["说过的话"]);

    let report = subject
        .enforce_retention(&RetentionPolicy::default(), at(9 * 86_400))
        .expect("执行保留期");
    assert_eq!(report.retired_content.len(), 1);
    assert_eq!(report.content_purged, 1);

    let inputs = subject.user_inputs(8).expect("读回");
    assert_eq!(inputs.len(), 1, "事件还在");
    assert_eq!(inputs[0].text, None, "内容是 `None`，不是错误");
    assert_eq!(inputs[0].channel, "chat");
    assert_eq!(inputs[0].at, at(0), "时刻也还在");
}

#[test]
fn the_same_message_from_two_channels_stays_two_events() {
    // 内容按内容寻址，所以两段一样的文字在仓里只有一个对象。而**事件是两条**——
    // 用户说了两遍这件事发生过两次，把它按内容去重会把"他说了两次"抹掉，
    // 而"说了两次"往往正是需要被看见的那件事。
    let mut subject = subject();
    assert!(delegate(&mut subject, "同样的话", UserChannel::Chat, ActionLevel::A1));
    assert!(delegate(&mut subject, "同样的话", UserChannel::ApprovalUi, ActionLevel::A1));

    let events = subject.store().read_events_after(0, 8).expect("读事件");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].envelope.payload_ref, events[1].envelope.payload_ref,
        "内容相同，所以引用相同"
    );
    assert_eq!(subject.store().blob_count().expect("内容对象数"), 1);
}
