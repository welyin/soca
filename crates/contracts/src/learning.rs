//! 策略改进的准入（§13.2 的第二种学习）。
//!
//! §13.2 把学习分成三种，第二种是：
//!
//! > **记忆/策略改进**：摘要、规则和技能作为候选，经**回放**与**保留任务集**检验后
//! > **版本化启用**；**不覆盖原证据**。
//!
//! 而同一节的末段把它与"自我进化"划清界限：
//!
//! > 新单元、子目标和**候选策略必须经过准入**。系统可建议新的单元或拓扑，但**没有自行安装
//! > 可执行代码、修改签名策略或提高权限的能力**。
//!
//! 本模块就是那句"必须经过准入"的执行形式。三条约束各有各的落点：
//!
//! | §13.2 的要求 | 本模块的落点 |
//! |---|---|
//! | 没有自行安装**可执行代码** | [`StrategyCandidate`] 里只有数据，没有函数；改动只能表达成 [`SelectionPolicy`] 的一个取值 |
//! | 没有修改**签名策略**的能力 | 候选里没有 [`crate::PolicyVersion`] 这个字段，也没有通向它的路 |
//! | 没有**提高权限**的能力 | [`admit`] 第一步就查 [`SelectionPolicy::is_at_least_as_strict_as`]：**只能变严** |
//!
//! "只能变严"听起来像把学习阉割掉了，其实不是。它挡住的只是"把门槛降下来"这一类改动，
//! 而**犯错之后提高门槛**恰恰是这一版最该学的东西——见 [`propose`]。
//!
//! 还有一条不在上表里，因为它不是一条检查而是一个事实：候选**不覆盖原证据**。
//! 它是数据，引用着证据，从不改写证据；准入过程也不碰证据。

use serde::{Deserialize, Serialize};

use crate::{ActionLevel, ContractError, RetryWhen, SelectionPolicy, Sha256Hex, StrategyVersion};

/// 策略版本标识里截取的摘要长度。
///
/// [`StrategyVersion`] 上限 64 字符，而 `sha256:` 加 64 位十六进制本身就占满了。
/// 截到 16 位在"同一策略永远同一个版本号"这条上是够的——它要挡的是**同名不同内容**，
/// 而不是密码学意义上的碰撞。
const VERSION_DIGEST_CHARS: usize = 16;

/// 保留任务集里的一条结论（§13.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedConclusion {
    /// 结论的标识。
    pub memory_id: String,
    /// 它被提出来时，手上有多少条证据。
    ///
    /// **唯一**参与比较的量。理由：结论类候选的门槛就是拿它比的
    /// （[`SelectionPolicy::evidence_bar`] 对 `Candidate::Claim` 的
    /// `evidence_refs.len()`）。记下别的量（相关度、当时的评分）会让这条检验没法只靠
    /// 已有的记录跑起来，而"检验要用到记录里没有的东西"通常意味着记录少了，
    /// 而不是检验该换个算法。
    pub evidence_count: usize,
    /// 怎么知道它是对的／错的。
    pub why: String,
}

/// 保留任务集（§13.2 的"回放与保留任务集检验"）。
///
/// 它是一批**事后已知对错**的结论。§13.2 要求候选策略经它检验才能启用，而这条检验要回答的
/// 问题很具体：
///
/// > **这个改动会不会把对的也一起挡掉？**
///
/// 这个问题不问"新策略是不是更好"——那要一个收益模型，而本版没有（§13.1 那一节列了六项，
/// 一项也没实现）。它问的是一个**有确定答案**的问题：既然已经知道哪几条是错的、哪几条是对的，
/// 那么新门槛会不会误伤。答不上来的改动不该上线，而答得上来的那些，答案已经够用了。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutSet {
    /// 事后确认过是对的（仍然可见的结论）。
    pub known_right: Vec<RecordedConclusion>,
    /// 事后被否掉的（被用户的纠错撤掉，或者它引用的证据已经不可用了）。
    pub known_wrong: Vec<RecordedConclusion>,
}

impl HoldoutSet {
    /// 一共多少条。
    pub fn len(&self) -> usize {
        self.known_right.len() + self.known_wrong.len()
    }

    /// 是否为空。
    ///
    /// 空集**不是**"检验通过"。没有任何已知答案时，准入闸没有任何依据——
    /// 而"没有依据"与"依据支持它"在报告上必须长得不一样，所以 [`admit`] 会拒绝它。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 候选策略在保留任务集上的表现（§13.2）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutReport {
    /// 本次比较用的门槛。
    pub bar: usize,
    /// 候选**会拦下、而事后知道是对的**那些。
    ///
    /// 这一项一非空就是拒绝：那是**误伤**，而误伤是这类改动唯一真正的代价。
    pub would_block_right: Vec<String>,
    /// 候选**仍然放行、而事后知道是错的**那些。
    ///
    /// 这一项非空**不算失败**——一次改动只声称解决它依据的那一条，别的错案要另想办法
    /// （而"另想办法"可能根本不是提高门槛）。所以它进报告，供人看还有多少账没清。
    pub still_admitted_wrong: Vec<String>,
}

/// 一次策略改进的候选（§13.2 的"系统可建议"）。
///
/// 这份结构里**没有函数、没有脚本、没有表达式**。§13.2 说系统"没有自行安装可执行代码的
/// 能力"，而这里它就是字面意义上的：能表达的东西只有 [`SelectionPolicy`] 的四个数字。
/// 想让系统学会一件这四个数字表达不了的事，得先有人往这里加一种表示——那是一次代码改动，
/// 要过一次编译、一次评审。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyCandidate {
    /// 候选策略的版本。必须与 [`StrategyCandidate::policy`] 的内容一致（见 [`version_of`]）。
    pub version: StrategyVersion,
    /// 候选要启用的选择策略。
    pub policy: SelectionPolicy,
    /// 为什么提它。给人读。
    pub rationale: String,
    /// 依据的实际记录（结论标识）。**必须非空**——没有依据的改动是一次猜测，
    /// 而猜测不该借着"学习"的名义改掉系统的判定标准。
    pub based_on: Vec<String>,
}

/// 准入判定（§13.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategyAdmission {
    /// 准予启用。
    Admitted {
        /// 启用的版本。
        version: StrategyVersion,
        /// 它在保留任务集上的表现。
        report: HoldoutReport,
    },
    /// 拒绝。与 §13.1 一样，**理由之外还要有可重试条件**。
    Refused {
        /// 为什么不行。给人读。
        reason: String,
        /// 什么条件下可以重来。
        retry_when: RetryWhen,
        /// 它在保留任务集上的表现（如果有跑过）。
        report: HoldoutReport,
    },
}

impl StrategyAdmission {
    /// 是否准予启用。
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }

    /// 本次判定用到的报告。
    pub fn report(&self) -> &HoldoutReport {
        match self {
            Self::Admitted { report, .. } | Self::Refused { report, .. } => report,
        }
    }
}

/// 由策略内容派生的版本标识（§13.2 的"版本化启用"）。
///
/// **内容寻址**：同一个策略永远得到同一个版本号，不同的策略一定不同。
///
/// 用随机标识或递增计数的话，"这一轮用的到底是哪一版策略"就成了一个只能靠时间线去推的
/// 问题——而它恰恰是出事之后第一个要回答的问题。而且递增计数经不起一次"改回去再改回来"：
/// 那会得到两个版本号指向同一份内容，于是"哪一版更严"再也比不出来。
pub fn version_of(policy: &SelectionPolicy) -> Result<StrategyVersion, ContractError> {
    // 通篇序列化整份策略，而不是手写一段字段拼接。手写的话，将来给 `SelectionPolicy`
    // 加一个字段而忘了加进这段拼接，会得到**两种内容、同一个版本号**——
    // 而那正是版本号存在的意义所在。
    let encoded = serde_json::to_vec(policy)
        .map_err(|_| ContractError::EncodingFailed { field: "strategy.policy" })?;
    let digest = Sha256Hex::of_bytes(&encoded);
    let digest = digest.as_str();
    let head = &digest[..VERSION_DIGEST_CHARS.min(digest.len())];
    StrategyVersion::new(format!("strategy:sha256:{head}"))
}

/// 候选策略在保留任务集上会不会踩到已经知道答案的地方（§13.2）。
pub fn evaluate_holdout(
    policy: &SelectionPolicy,
    holdout: &HoldoutSet,
    risk: ActionLevel,
) -> HoldoutReport {
    let bar = policy.evidence_bar(risk);
    let blocked = |entries: &[RecordedConclusion]| -> Vec<String> {
        entries
            .iter()
            .filter(|entry| entry.evidence_count < bar)
            .map(|entry| entry.memory_id.clone())
            .collect()
    };

    HoldoutReport {
        bar,
        // 会把对的挡掉的那些：证据条数低于新门槛。
        would_block_right: blocked(&holdout.known_right),
        // 仍然放行的错案：证据条数达到或超过新门槛。
        still_admitted_wrong: holdout
            .known_wrong
            .iter()
            .filter(|entry| entry.evidence_count >= bar)
            .map(|entry| entry.memory_id.clone())
            .collect(),
    }
}

/// 准入（§13.2 的那句"必须经过准入"）。
///
/// 判定顺序是有意的：**先查候选本身合不合法，再看它在保留集上的表现**。反过来做的话，
/// 一个"放宽门槛"的候选会先拿到一份好看的回放报告（放宽当然不会误伤），
/// 而那正是最不该被放行的东西。这一条与 §12.1 把"范围越界"归入 `Refused` 是同一个道理：
/// **有些属性是候选本身的，与它表现好不好无关。**
pub fn admit(
    candidate: &StrategyCandidate,
    incumbent: &SelectionPolicy,
    holdout: &HoldoutSet,
    risk: ActionLevel,
) -> Result<StrategyAdmission, ContractError> {
    let empty_report = HoldoutReport::default();

    // 一、版本号必须与内容一致。
    //
    // 不查这一条的话，"同一版号、两种内容"是构造得出来的——而版本号存在的全部意义就是
    // 回答"这一轮用的是哪一版"（§7.2 的"标识不得复用"在这里也成立）。
    let expected = version_of(&candidate.policy)?;
    if candidate.version != expected {
        return Ok(StrategyAdmission::Refused {
            reason: format!(
                "版本号与内容对不上：候选自称 {}，而这份策略的实际版本是 {expected}；\
                 版本号必须由内容派生",
                candidate.version
            ),
            // 换一个版本号就行，而那是改一行的事，不是等一等的事。
            retry_when: RetryWhen::Never,
            report: empty_report,
        });
    }

    // 二、必须有依据。
    if candidate.based_on.is_empty() {
        return Ok(StrategyAdmission::Refused {
            reason: "候选策略没有引用任何实际记录；没有依据的改动是一次猜测，\
                     而猜测不该借着「学习」的名义改掉判定标准"
                .to_string(),
            // 补一条依据就行。
            retry_when: RetryWhen::MoreEvidence { short_by: 1 },
            report: empty_report,
        });
    }

    // 三、**只能变严**。
    //
    // §13.2 说系统"没有自行……提高权限的能力"。这一行就是那句话。归入 `RetryWhen::Never`
    // 而不是"等一等"：放宽不是等出来的，它需要人改配置——而"等人来改"与"此路不通"在调度上
    // 是一回事，在报告上是两回事，所以理由里写清了这一点。
    if !candidate.policy.is_at_least_as_strict_as(incumbent) {
        return Ok(StrategyAdmission::Refused {
            reason: format!(
                "候选策略比当前生效的更松（当前：证据门槛 {}／高风险加 {}／高风险起于 {}／核验预算 {}；\
                 候选：{}／{}／{}／{}）。§13.2 要求系统没有自行提高权限的能力——\
                 学习可以变严，变松要由人改配置",
                incumbent.base_evidence,
                incumbent.high_risk_extra,
                incumbent.high_risk_from.as_str(),
                incumbent.max_checks,
                candidate.policy.base_evidence,
                candidate.policy.high_risk_extra,
                candidate.policy.high_risk_from.as_str(),
                candidate.policy.max_checks,
            ),
            retry_when: RetryWhen::Never,
            report: empty_report,
        });
    }

    // 四、保留任务集必须非空。
    if holdout.is_empty() {
        return Ok(StrategyAdmission::Refused {
            reason: "保留任务集是空的。没有任何已知答案时，准入闸没有依据——\
                     而「没有依据」与「依据支持它」不是一回事"
                .to_string(),
            // 攒够记录再来。
            retry_when: RetryWhen::MoreEvidence { short_by: 1 },
            report: empty_report,
        });
    }

    // 五、回放检验。
    let report = evaluate_holdout(&candidate.policy, holdout, risk);

    // 5a、不许误伤。
    if !report.would_block_right.is_empty() {
        return Ok(StrategyAdmission::Refused {
            reason: format!(
                "会误伤：新门槛 {} 条会挡下 {} 条事后确认是对的结论（{}）。\
                 这些改动解决不了——它连对的都一起挡了",
                report.bar,
                report.would_block_right.len(),
                report.would_block_right.join("、"),
            ),
            // 等这些样本本身发生变化：被更正、被撤回，或者补上了更多证据。
            retry_when: RetryWhen::WhenEvidenceChanges,
            report,
        });
    }

    // 5b、它得真的解决了它声称解决的问题。
    //
    // 只看"没误伤"是不够的：一个把门槛提到天上、把**所有**结论都挡掉的策略也不会误伤，
    // 它连依据的那条错案都不放行了——但那不是学习，那是停止工作。
    let addressed = holdout
        .known_wrong
        .iter()
        .filter(|entry| candidate.based_on.contains(&entry.memory_id))
        .collect::<Vec<_>>();
    if addressed.is_empty() {
        return Ok(StrategyAdmission::Refused {
            reason: format!(
                "依据里提到的记录没有一条在保留任务集里被认定为错的：{}；\
                 一次改动要能指出它解决了哪一条",
                candidate.based_on.join("、")
            ),
            retry_when: RetryWhen::Never,
            report,
        });
    }
    let unfixed: Vec<&RecordedConclusion> = addressed
        .iter()
        .copied()
        .filter(|entry| entry.evidence_count >= report.bar)
        .collect();
    if !unfixed.is_empty() {
        let needed = unfixed
            .iter()
            .map(|entry| entry.evidence_count + 1)
            .max()
            .unwrap_or(report.bar);
        return Ok(StrategyAdmission::Refused {
            reason: format!(
                "依据的那几条错案仍然会被放行：{}（门槛 {} 条，而它们手上有 {} 条）。\
                 这个改动没有解决它声称解决的问题",
                unfixed
                    .iter()
                    .map(|entry| entry.memory_id.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
                report.bar,
                unfixed
                    .iter()
                    .map(|entry| entry.evidence_count.to_string())
                    .collect::<Vec<_>>()
                    .join("、"),
            ),
            // 门槛至少要提到这一档——而**提到不提到得了**是另一回事（见 5a）。
            retry_when: RetryWhen::MoreEvidence {
                short_by: needed.saturating_sub(report.bar),
            },
            report,
        });
    }

    Ok(StrategyAdmission::Admitted {
        version: candidate.version.clone(),
        report,
    })
}

/// 从实际记录里提一个候选（§13.2 的"系统可**建议**"）。
///
/// 提的是最直白的一种学习：**有一条结论被证明是错的，而它当时手上的证据条数刚好够过门槛。
/// 那就把门槛提到它之上。**
///
/// 它不去猜"应该改多少"——只提刚好挡住那条错案的量，剩下的交给 [`admit`]。这样做的理由很实在：
/// 提得多一点看起来更安全，但"更安全"是**相对什么**没有答案（本版没有收益模型），
/// 而"刚好挡住那一条"是一个能验证的陈述——要么挡住了，要么没挡住。
///
/// 返回 `None` 表示没有可提的：没有错案，或者门槛已经高到能挡住手上最严重的那一条。
pub fn propose(
    incumbent: &SelectionPolicy,
    holdout: &HoldoutSet,
    risk: ActionLevel,
) -> Option<StrategyCandidate> {
    // 挑最"够格"的那一条错案：它手上的证据最多，也就是最容易蒙混过去的那个。
    let worst = holdout
        .known_wrong
        .iter()
        .max_by_key(|entry| entry.evidence_count)?;

    let bar = incumbent.evidence_bar(risk);
    let needed = worst.evidence_count.saturating_add(1);
    if needed <= bar {
        return None;
    }

    let mut policy = *incumbent;
    // 门槛与 `base_evidence` 的关系在两条路径上都成立，所以提多少都加在 `base_evidence` 上：
    // 低风险档 bar = base，高风险档 bar = base + extra，两边同向。
    policy.base_evidence = policy.base_evidence.saturating_add(needed - bar);

    Some(StrategyCandidate {
        version: version_of(&policy).ok()?,
        policy,
        rationale: format!(
            "结论 {} 当时手上有 {} 条证据、通过了 {} 条的门槛，事后被证明是错的；\
             把证据门槛从 {bar} 提到 {needed}。",
            worst.memory_id, worst.evidence_count, bar
        ),
        based_on: vec![worst.memory_id.clone()],
    })
}
