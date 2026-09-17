//! 控制台持有的模型端点设置。
//!
//! 三件本模块负责的事：
//!
//! 1. **凭据不落盘。** [`ModelCredentials`] 只活在进程内存里，控制台不把它写进 SQLite。
//!    §12.3 那条"不保存密码"说的就是这个；重启之后需要重新填一次，那点麻烦换来的是
//!    "密钥不会静静地躺在某个备份里"。
//! 2. **换传输层与授权是一次动作。** [`ConsoleModel::apply`] 同时设置后端、云端授权与出站
//!    策略，不提供只改其中一项的入口——"换了端点但沿用旧授权"那样的中间态本身就是缺陷。
//! 3. **界面只拿到指纹。** [`ConsoleModel::summary`] 输出的是 `sk-****abcd`，不是密钥。
//!    界面能回答"我配的是哪一把钥匙"，而这个回答不需要交出钥匙。

use serde_json::{json, Value};
use soca_contracts::{
    Candidate, ModelBackend, ModelOutput, ModelProposal, ModelSelfReport, ModelVersion, TokenUsage,
    MODEL_OUTPUT_SCHEMA_VERSION,
};
use soca_core::{CoreError, Subject};
use soca_model_gateway::{DeterministicTransport, ModelCredentials, RemoteTransport};

/// 离线桩的模型版本标识。
///
/// 用哈希形状而不是一个普通名字，是为了让审计账里一眼能看出"这一次没有走任何模型"。
pub const STUB_MODEL: &str = "sha256:deterministic-stub";

/// 未接入真实模型时使用的应答源。
///
/// 它做的事只有一件：把上下文里的每一条证据复述成一条带证据的结论。这不是"假装有推理"，
/// 恰恰相反——它**不可能**引用它没看到的东西，因此 §8 那条边界在它身上永远不会被触发，
/// 而整条通路（编译 → 网关 → 预算 → 校验 → 候选）是真的在跑。
pub fn offline_stub() -> DeterministicTransport {
    DeterministicTransport::from_fn(|request| {
        let proposals = request
            .context
            .evidence
            .iter()
            .map(|slice| ModelProposal {
                candidate: Candidate::Claim {
                    statement: format!("{} 的值是 {}", slice.subject_ref, slice.observed_value),
                    evidence_refs: vec![slice.evidence_ref.clone()],
                },
                self_report: ModelSelfReport {
                    // §3.2：自报数值不是概率。0.5 在这里只是"我没有任何把握"的占位，
                    // 它没有校准来源，也不会变成 CalibratedProbability。
                    reported_value: 0.5,
                    model_version: ModelVersion::new(STUB_MODEL).expect("固定模型版本"),
                    rationale: "确定性桩：不是判断，只是复述".to_string(),
                },
                rationale: "未接入真实模型；本桩把收到的证据原样复述，用来证明通路可用"
                    .to_string(),
            })
            .collect();

        Ok(ModelOutput {
            schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
            model_version: ModelVersion::new(STUB_MODEL).expect("固定模型版本"),
            proposals,
            // 桩不消耗 token，记 0 而不是编一个数。编一个数会让成本对照失去意义。
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            claims_finished: false,
        })
    })
}

/// 控制台当前的模型配置。
#[derive(Debug, Default)]
pub struct ConsoleModel {
    credentials: Option<ModelCredentials>,
}

impl ConsoleModel {
    /// 初始状态：没有配置任何远端端点，用离线桩。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前端点。未配置时为 `None`。
    pub fn current(&self) -> Option<&ModelCredentials> {
        self.credentials.as_ref()
    }

    /// 是否已配置远端端点。
    pub fn is_configured(&self) -> bool {
        self.credentials.is_some()
    }

    /// 应用一份新配置。
    ///
    /// `remote_authorized` 传 `true`：用户在界面上刚刚亲手填了这个端点，那一次填写就是 §4.3
    /// 所说的"已获得云端出站授权"。它不是一个默认开启的开关，而是一次具体、可追溯的决定。
    ///
    /// `allow_personal` 默认应为 `false`。它为 `true` 时，[`Subject`] 的出站策略放宽到
    /// [`soca_contracts::EgressPolicy::AllowPersonal`]——**只放开个人数据这一档**，
    /// `sensitive` 与 `secret` 无论在哪种策略下都不出站。
    pub fn apply(
        &mut self,
        subject: &mut Subject,
        credentials: ModelCredentials,
        allow_personal: bool,
    ) -> Result<(), CoreError> {
        // 记的是**实际生效的**凭据，不是用户填的那份。差别就在 allow_private_egress：
        // 记用户填的那份，界面会在放开之后仍然显示 strict——而那正是"界面显示的状态与实际
        // 不符"这一类最难查的错误。这个缺陷是被 the_grant_is_visible_in_the_summary 抓到的。
        let effective = credentials.with_private_egress(allow_personal);

        subject.set_transport(
            Box::new(RemoteTransport::new(effective.clone())),
            ModelBackend::Remote,
            true,
            ModelVersion::new(format!("remote:{}", effective.model()))?,
        )?;
        if allow_personal {
            subject.grant_personal_egress();
        }
        // 只有换成功了才记住它。先记后换的话，一次失败的连接会让界面显示"已连接"，
        // 而实际用的是上一个端点——那种错误比直接失败难查得多。
        self.credentials = Some(effective);
        Ok(())
    }

    /// 断开远端端点，回到离线桩。
    pub fn reset(&mut self, subject: &mut Subject) -> Result<(), CoreError> {
        subject.set_transport(
            Box::new(offline_stub()),
            ModelBackend::Cpu,
            false,
            ModelVersion::new(STUB_MODEL)?,
        )?;
        self.credentials = None;
        Ok(())
    }

    /// 给界面看的摘要。**不含密钥。**
    pub fn summary(&self) -> Value {
        match &self.credentials {
            None => json!({
                "configured": false,
                "mode": "offline_stub",
                "note": "未配置远端端点。当前应答源是确定性桩，只会把收到的证据复述一遍。",
            }),
            Some(credentials) => json!({
                "configured": true,
                "mode": "remote",
                "base_url": credentials.base_url(),
                "model": credentials.model(),
                "endpoint": credentials.endpoint(),
                // 只给指纹。界面能回答"是哪把钥匙"，而这个回答不需要交出钥匙。
                "api_key_fingerprint": credentials.fingerprint(),
                "allow_private_egress": credentials.allow_private_egress(),
                "note": "密钥只保存在本进程内存中，不落盘；重启后需要重新填写。",
            }),
        }
    }
}
