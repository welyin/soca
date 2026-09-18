//! 控制台页面。
//!
//! 单文件、零外部资源。§14 要求训练台至少提供"选择游戏/规则版本、人工玩、SoCA 接管、
//! 暂停/单步、公开观测、预测/执行/结果对照、当前拓扑树和每层数量、硬件压力、冷热状态、
//! 成本、失败原因及轨迹回放"——**本版只做到其中的一部分**：公开观测、目标状态、额度消耗、
//! 证据流转与模型返回物。没做的部分不在这里假装有。
//!
//! 页面里所有数字都来自 `GET /api/state` 或一次操作的返回，不做前端推算。§13 的同一条思路：
//! 界面上显示的东西必须是系统真实持有的东西，而不是界面自己算出来的一个近似。

use crate::http::Request;

/// 渲染页面，注入本次会话令牌。
pub fn render(token: &str) -> String {
    TEMPLATE.replace("__SOCA_TOKEN__", token)
}

/// 模板。占位符替换是唯一的"服务端渲染"。
const TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SoCA 控制台</title>
<style>
  :root {
    --bg: #0f1115; --panel: #171a21; --line: #262b36; --ink: #e6e9ef;
    --dim: #8b93a3; --accent: #4c8dff; --warn: #e0a83c; --bad: #e05c5c; --ok: #3fb98a;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 0 0 48px; background: var(--bg); color: var(--ink);
    font: 14px/1.6 "Segoe UI", "Microsoft YaHei", system-ui, sans-serif;
  }
  header {
    padding: 20px 28px; border-bottom: 1px solid var(--line);
    display: flex; align-items: baseline; gap: 16px; flex-wrap: wrap;
  }
  h1 { margin: 0; font-size: 17px; font-weight: 600; letter-spacing: .3px; }
  header .sub { color: var(--dim); font-size: 12px; }
  main { max-width: 1080px; margin: 0 auto; padding: 24px 28px; display: grid; gap: 18px; }
  section { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 18px 20px; }
  section > h2 {
    margin: 0 0 14px; font-size: 12px; font-weight: 600; letter-spacing: 1.2px;
    text-transform: uppercase; color: var(--dim);
  }
  .row { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
  input, textarea, select, button {
    font: inherit; color: var(--ink); background: #0c0e13;
    border: 1px solid var(--line); border-radius: 7px; padding: 9px 12px;
  }
  input:focus, textarea:focus, select:focus { outline: 1px solid var(--accent); border-color: var(--accent); }
  textarea { flex: 1; min-height: 44px; resize: vertical; }
  button { background: var(--accent); border-color: var(--accent); color: #fff; cursor: pointer; font-weight: 500; }
  button:hover { filter: brightness(1.12); }
  button:disabled { opacity: .5; cursor: default; }
  button.ghost { background: transparent; border-color: var(--line); color: var(--ink); }
  button.danger { background: transparent; border-color: var(--bad); color: var(--bad); }
  .metrics { display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr)); gap: 12px; }
  .metric { background: #0c0e13; border: 1px solid var(--line); border-radius: 8px; padding: 12px 14px; }
  .metric .k { color: var(--dim); font-size: 11px; letter-spacing: .6px; }
  .metric .v { font-size: 22px; font-weight: 600; margin-top: 4px; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 9px 10px; border-bottom: 1px solid var(--line); vertical-align: top; }
  th { color: var(--dim); font-weight: 500; font-size: 12px; }
  tr:last-child td { border-bottom: none; }
  .tag { display: inline-block; padding: 1px 8px; border-radius: 20px; font-size: 11px; border: 1px solid var(--line); color: var(--dim); }
  .tag.active { color: var(--ok); border-color: var(--ok); }
  .tag.satisfied { color: var(--accent); border-color: var(--accent); }
  .tag.abandoned, .tag.expired { color: var(--bad); border-color: var(--bad); }
  .tag.waiting_approval, .tag.blocked { color: var(--warn); border-color: var(--warn); }
  pre {
    margin: 0; padding: 12px 14px; background: #0c0e13; border: 1px solid var(--line);
    border-radius: 8px; overflow-x: auto; font-size: 12px; white-space: pre-wrap; word-break: break-word;
  }
  .hint { color: var(--dim); font-size: 12px; margin-top: 10px; }
  .empty { color: var(--dim); padding: 8px 0; }
  .err { color: var(--bad); }
  .ok { color: var(--ok); }
  #log { max-height: 260px; overflow-y: auto; }
  #log div { padding: 6px 0; border-bottom: 1px solid var(--line); font-size: 12.5px; }
  #log div:last-child { border-bottom: none; }
</style>
</head>
<body>
<header>
  <h1>SoCA 控制台</h1>
  <span class="sub" id="owner">连接中…</span>
  <span class="sub">本页面只服务本机 loopback，令牌由服务端注入</span>
</header>

<main>
  <section>
    <h2>公开状态</h2>
    <div class="metrics" id="metrics"></div>
    <div class="hint">
      这些数字全部来自主体自己持有的状态。界面上没有任何前端推算——
      界面看到的必须和 agent 依据的是同一份东西，否则两者会开始互相解释。
    </div>
  </section>

  <section>
    <h2>模型</h2>
    <div id="model-current" class="hint" style="margin:0 0 14px">读取中…</div>
    <div class="row">
      <input id="model-base" style="flex:2" placeholder="https://api.deepseek.com">
      <input id="model-name" style="flex:1" placeholder="deepseek-chat">
    </div>
    <div class="row" style="margin-top:10px">
      <input id="model-key" type="password" style="flex:2" placeholder="API 密钥（只留在本进程内存里）">
      <button id="model-connect">连接</button>
      <button id="model-reset" class="ghost">断开</button>
    </div>
    <label class="row" style="margin-top:12px; gap:8px; color:var(--dim); font-size:12.5px">
      <input id="model-allow-personal" type="checkbox" style="width:auto">
      允许把 <b>personal</b> 级别的观测发往该端点（默认关闭）
    </label>
    <div class="hint">
      §8 的默认是"私人数据类别不得出站到云端"。勾上这一项是<b>你</b>对自己数据的处置，
      不是系统放宽了限制：它只放开 personal 这一档，<b>sensitive 与 secret 无论怎样都不出站</b>；
      换端点之后它会自动回到关闭。密钥不写入数据库，重启后需要重填。
    </div>
  </section>

  <section>
    <h2>委托一个目标</h2>
    <div class="row">
      <textarea id="message" placeholder="例如：为已授权目录生成一份摘要"></textarea>
      <button id="send">委托</button>
    </div>
    <div class="hint">
      §4.1 L6：目标只能由用户在明确通道上委托。屏幕上看到的一段文字、麦克风转写、文档内容、
      模型输出都不是指令来源，因此从这个框以外的地方产不出根目标。
    </div>
  </section>

  <section>
    <h2>目标</h2>
    <table>
      <thead><tr><th>目标</th><th>状态</th><th>额度</th><th></th></tr></thead>
      <tbody id="goals"><tr><td colspan="4" class="empty">还没有目标</td></tr></tbody>
    </table>
  </section>

  <section>
    <h2>观测</h2>
    <div class="row">
      <input id="subject" style="flex:1" value="file:D:\资料\摘要\summary.md">
      <select id="data-class">
        <option value="public">public</option>
        <option value="personal" selected>personal</option>
        <option value="sensitive">sensitive</option>
        <option value="secret">secret</option>
      </select>
      <button id="observe" class="ghost">读取一次</button>
    </div>
    <div class="hint">
      一次观测会同时进入事件账与 L2 黑板。它产生的证据引用是运行时生成的，
      因此模型只能回引它——这就是"模型不能引用它没看到的证据"能成立的原因。
    </div>
  </section>

  <section>
    <h2>模型咨询</h2>
    <div class="hint" style="margin:0 0 12px">
      选一个目标咨询。本版输出 Schema 是只读的：模型即使想提动作也过不了校验。
      接入真实模型之前，应答源是一个确定性桩，它只会把收到的证据复述一遍。
    </div>
    <div class="row">
      <select id="consult-goal" style="flex:1"></select>
      <button id="consult" class="ghost">咨询</button>
    </div>
    <pre id="consult-out" style="margin-top:14px">（尚未咨询）</pre>
  </section>

  <section>
    <h2>检验与选择</h2>
    <div class="hint" style="margin:0 0 12px">
      §6 第 4–5 步：先核对结论，再按证据门槛选一条。核对分三类——结论依据核对（命题断言的
      值必须在它自己引用的证据里）、来源核对（同源的两条证据不算两个来源）、反例搜索。
      <b>低风险不搜反例</b>：对一份纯格式的工作，反方观点不是找不到，而是根本不存在。
    </div>
    <div class="row">
      <select id="select-risk">
        <option value="a0">A0 只读</option>
        <option value="a1" selected>A1 本地计算</option>
        <option value="a2">A2 改动对象 → 高风险档</option>
        <option value="a3">A3 高风险</option>
        <option value="a4">A4 高风险</option>
      </select>
      <button id="run-select" class="ghost">检验并选择</button>
      <input id="loop-rounds" type="number" min="1" max="32" value="4" style="width:76px" title="跑几轮">
      <button id="run-loop">跑几轮闭环</button>
    </div>
    <div class="hint">
      证据门槛是风险等级的函数：A0/A1 要 1 条，A2 起要 3 条。被检验否定的候选直接出局，
      不论它有多少证据——一条被反例推翻的结论不会因为支持者多就重新成立。
    </div>
    <pre id="select-out" style="margin-top:14px">（尚未运行）</pre>
  </section>

  <section>
    <h2>执行与审批</h2>
    <div class="hint" style="margin:0 0 12px">
      §12.1／§12.2：写入是 A2，"在指定目录生成文件"。它需要一次人工批准，而批准
      <b>绑定到具体那一次动作</b>——换一份内容或换一个目录就不再被覆盖。
      批准只针对这一次，用完即尽。
    </div>
    <div class="row">
      <button id="delegate-write" class="ghost">① 委托一个可写入的任务（A2）</button>
      <span class="hint">上面聊天框的目标是 A1（只读），写入会被正确拒掉。</span>
    </div>
    <div class="row">
      <input id="write-target" type="text" placeholder="要写入的路径，例如 D:\\资料\\摘要\\out.md" style="flex:1">
      <input id="write-content" type="text" placeholder="要写入的内容" style="flex:1">
      <button id="request-write">② 投递写入</button>
    </div>
    <div class="row">
      <select id="approve-level">
        <option value="a2" selected>A2 改动对象</option>
        <option value="a3">A3 高风险</option>
      </select>
      <input id="approve-uses" type="number" min="1" max="255" value="1" style="width:76px" title="可用次数">
      <select id="approve-channel">
        <option value="approval_ui" selected>图形审批界面</option>
        <option value="chat">聊天框</option>
        <option value="push_to_talk">按键说话（A3 不接受）</option>
      </select>
      <button id="approve">③ 批准</button>
      <button id="resume" class="ghost">④ 恢复等待审批的目标</button>
    </div>
    <div class="hint">
      A3 不接受语音批准（§14：语音可能误识别，而 A3 没有别的兜底）。批准一旦用尽，
      再投递同样的动作会重新回到"等待审批"，而不是悄悄放行。
    </div>
    <pre id="exec-out" style="margin-top:14px">（尚未运行）</pre>
  </section>

  <section>
    <h2>授权与撤回</h2>
    <div class="hint" style="margin:0 0 12px">
      §12.1：「范围限定授权，<b>撤回立即生效</b>。」这句话有两半——撤回之后
      <b>不再产生新的观测</b>，而且<b>之前读到的东西要失效</b>。只做前一半，撤回就成了一句
      只对将来有效的空话。
    </div>
    <div class="row">
      <input id="capability" type="text" value="cap:read-selected-folder" style="flex:1">
      <button id="grant-cap" class="ghost">授予</button>
      <button id="revoke-cap">撤回</button>
    </div>
    <div class="hint">
      撤回走的是「事件 → 证据引用 → 记忆」这条链：只有**引用**了该授权下事件的那些记忆会失效，
      别的授权名下的记忆不受影响。内容对象不在撤回范围内——它们不携带能力归属，
      而猜一个归属然后删掉比不删更糟。
    </div>
    <pre id="revoke-out" style="margin-top:14px">（尚未运行）</pre>
  </section>

  <section>
    <h2>保留期与删除</h2>
    <div class="hint" style="margin:0 0 12px">
      §12.3：「先写 tombstone 使查询立即不可见，<b>再异步清理</b>，并给用户完成状态。」
      两步分开报——用户看到的"删除完成"指的是前者，后者可能还在排队。
    </div>
    <div class="row">
      <button id="run-retention" class="ghost">执行保留期清理</button>
      <span class="hint">隐藏超期记忆、清理已隐藏内容、剪掉超期审计</span>
    </div>
    <div class="row">
      <input id="forget-id" type="text" placeholder="要删除的记忆标识，例如 memory:..." style="flex:1">
      <button id="forget">删除这条记忆</button>
    </div>
    <div class="hint">
      用户删除是<a>立即不可见</a>，物理清理留给下一次保留期执行——§12.3 要的是"异步清理"，
      把清理塞进删除请求会让界面卡在一次可能很慢的传播上。
    </div>
    <pre id="retention-out" style="margin-top:14px">（尚未运行）</pre>
  </section>

  <section>
    <h2>事件</h2>
    <div id="log"><div class="empty">还没有操作</div></div>
  </section>
</main>

<script>
const TOKEN = "__SOCA_TOKEN__";
const $ = id => document.getElementById(id);

function log(message, kind) {
  const box = $("log");
  if (box.firstChild && box.firstChild.classList.contains("empty")) box.innerHTML = "";
  const line = document.createElement("div");
  if (kind === "err") line.className = "err";
  if (kind === "ok") line.className = "ok";
  line.textContent = new Date().toLocaleTimeString() + "  " + message;
  box.prepend(line);
}

async function api(path, body) {
  const options = {
    method: body === undefined ? "GET" : "POST",
    headers: { "x-soca-token": TOKEN },
  };
  if (body !== undefined) {
    options.headers["content-type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const response = await fetch("/api/" + path, options);
  const text = await response.text();
  let payload;
  try { payload = JSON.parse(text); } catch { payload = { detail: text }; }
  if (!response.ok) {
    throw new Error(payload.detail || payload.error || ("HTTP " + response.status));
  }
  return payload;
}

function tag(state, expired) {
  const label = expired ? "expired" : state;
  return '<span class="tag ' + label + '">' + label + "</span>";
}

function renderState(state) {
  $("owner").textContent = state.owner
    + "　后端 " + state.backend
    + "　出站 " + state.egress_policy
    + "　模型调用 " + state.model_calls + " 次";

  const metrics = [
    ["目标", state.goals.length],
    ["已观测证据", state.observed_evidence],
    ["黑板主题", state.workspace_topics],
    ["黑板证据", state.workspace_evidence],
    ["可见记忆", state.memory_entries],
    ["动作账", state.actions],
  ];
  $("metrics").innerHTML = metrics.map(function (m) {
    return '<div class="metric"><div class="k">' + m[0] + '</div><div class="v">' + m[1] + "</div></div>";
  }).join("");

  const tbody = $("goals");
  if (state.goals.length === 0) {
    tbody.innerHTML = '<tr><td colspan="4" class="empty">还没有目标</td></tr>';
  } else {
    tbody.innerHTML = state.goals.map(function (goal) {
      return "<tr><td>" + escapeHtml(goal.statement) +
        '<br><span class="tag">' + goal.goal_id + "　深度 " + goal.depth + "</span></td>" +
        "<td>" + tag(goal.state, goal.expired) + "</td>" +
        '<td><span class="tag">激活 ' + goal.remaining_activations +
        '</span> <span class="tag">探索 ' + goal.remaining_explorations + "</span></td>" +
        '<td><button class="danger" data-abandon="' + goal.goal_id + '">放弃</button></td></tr>';
    }).join("");
    tbody.querySelectorAll("[data-abandon]").forEach(function (button) {
      button.onclick = async function () {
        try {
          const result = await api("abandon", { goal_id: button.dataset.abandon });
          log("放弃 " + button.dataset.abandon + "，连带结束 " + result.abandoned + " 个目标", "ok");
        } catch (error) { log("放弃失败：" + error.message, "err"); }
        refresh();
      };
    });
  }

  const select = $("consult-goal");
  const open = state.goals.filter(function (g) { return g.state === "active" || g.state === "blocked"; });
  const chosen = select.value;
  select.innerHTML = open.length === 0
    ? '<option value="">（没有可咨询的目标）</option>'
    : open.map(function (g) {
        return '<option value="' + g.goal_id + '">' + escapeHtml(g.goal_id + "　" + g.statement) + "</option>";
      }).join("");
  if (chosen && open.some(function (g) { return g.goal_id === chosen; })) select.value = chosen;
}

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, function (c) {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
  });
}

async function refresh() {
  try { renderState(await api("state")); }
  catch (error) { log("读取状态失败：" + error.message, "err"); }
}

async function refreshModel() {
  try {
    const info = await api("model");
    if (!info.configured) {
      $("model-current").textContent = "未配置远端端点。当前应答源是确定性桩，只会复述收到的证据。";
      $("model-base").placeholder = "https://api.deepseek.com";
      $("model-name").placeholder = "deepseek-chat";
      $("model-allow-personal").checked = false;
      return;
    }
    $("model-current").innerHTML =
      "已连接 <b>" + escapeHtml(info.endpoint) + "</b>　模型 " + escapeHtml(info.model) +
      "　密钥 " + escapeHtml(info.api_key_fingerprint) +
      (info.allow_private_egress
        ? '　<span class="tag waiting_approval">已放开 personal 出站</span>'
        : '　<span class="tag active">strict 出站</span>');
    $("model-base").value = info.base_url;
    $("model-name").value = info.model;
    $("model-allow-personal").checked = info.allow_private_egress;
  } catch (error) { log("读取模型配置失败：" + error.message, "err"); }
}

$("model-connect").onclick = async function () {
  const key = $("model-key").value.trim();
  if (!key) { log("先填 API 密钥", "err"); return; }
  $("model-connect").disabled = true;
  try {
    const info = await api("model", {
      base_url: $("model-base").value.trim() || "https://api.deepseek.com",
      model: $("model-name").value.trim() || "deepseek-chat",
      api_key: key,
      allow_private_egress: $("model-allow-personal").checked,
    });
    // 填进去之后就清掉输入框：页面 DOM 里留着的密钥会被任何截图、录屏或扩展读走。
    $("model-key").value = "";
    log("已连接到 " + info.endpoint + "（密钥 " + info.api_key_fingerprint + "）", "ok");
    if (info.allow_private_egress) {
      log("注意：已放开 personal 数据出站。sensitive 与 secret 仍然不出站。");
    }
  } catch (error) { log("连接失败：" + error.message, "err"); }
  $("model-connect").disabled = false;
  refreshModel();
  refresh();
};

$("model-reset").onclick = async function () {
  try {
    await api("model/reset", {});
    log("已断开远端端点，回到离线桩", "ok");
  } catch (error) { log("断开失败：" + error.message, "err"); }
  refreshModel();
  refresh();
};

$("send").onclick = async function () {
  const message = $("message").value.trim();
  if (!message) return;
  $("send").disabled = true;
  try {
    const result = await api("chat", { message: message });
    log("委托了 " + result.goal_id, "ok");
    $("message").value = "";
  } catch (error) { log("委托失败：" + error.message, "err"); }
  $("send").disabled = false;
  refresh();
};

$("observe").onclick = async function () {
  const target = $("subject").value.trim();
  if (!target) return;
  try {
    const result = await api("observe", { subject: target, data_class: $("data-class").value });
    log("观测到 " + result.subject + " = " + result.value + "（证据 " + result.evidence_ref + "）", "ok");
  } catch (error) { log("观测失败：" + error.message, "err"); }
  refresh();
};

$("consult").onclick = async function () {
  const goalId = $("consult-goal").value;
  if (!goalId) { log("先委托并受理一个目标", "err"); return; }
  $("consult").disabled = true;
  try {
    const result = await api("consult", { goal_id: goalId });
    $("consult-out").textContent = JSON.stringify(result, null, 2);
    log("咨询完成，" + result.proposals.length + " 条候选，用了 " +
        (result.input_tokens + result.output_tokens) + " token", "ok");
  } catch (error) {
    $("consult-out").textContent = error.message;
    log("咨询被拒：" + error.message, "err");
  }
  $("consult").disabled = false;
  refresh();
};

$("run-select").onclick = async function () {
  $("run-select").disabled = true;
  try {
    const result = await api("select", { risk: $("select-risk").value });
    $("select-out").textContent = JSON.stringify(result, null, 2);
    const checks = result.reviews.reduce(function (total, review) {
      return total + review.outcomes.length;
    }, 0);
    const chosen = result.outcome.kind === "selected"
      ? ("选中候选 #" + result.outcome.index)
      : ("未选中：" + result.outcome.kind);
    log(chosen + "　跑了 " + checks + " 项检验，证据门槛 " + result.required_evidence + " 条", "ok");
  } catch (error) {
    $("select-out").textContent = error.message;
    log("检验失败：" + error.message, "err");
  }
  $("run-select").disabled = false;
  refresh();
};

function showExec(label, result) {
  $("exec-out").textContent = JSON.stringify(result, null, 2);
  log(label, "ok");
}

$("delegate-write").onclick = async function () {
  await execAction("delegate_write", { message: "在已授权目录里写入摘要文件" }, "① 已委托可写入的目标（A2）");
};

$("request-write").onclick = async function () {
  const target = $("write-target").value.trim();
  if (!target) { log("先填一个写入路径", "err"); return; }
  await execAction("write", {
    subject_ref: "file:" + target,
    content: $("write-content").value,
  }, "② 已投递写入");
};

$("approve").onclick = async function () {
  await execAction("approve", {
    level: $("approve-level").value,
    max_uses: parseInt($("approve-uses").value, 10) || 1,
    channel: $("approve-channel").value,
  }, "③ 已批准");
};

$("resume").onclick = async function () {
  await execAction("resume", {}, "④ 已恢复等待审批的目标");
};

async function execAction(path, payload, label) {
  try {
    showExec(label, await api(path, payload));
  } catch (error) {
    $("exec-out").textContent = error.message;
    log(label + "失败：" + error.message, "err");
  }
  refresh();
}

$("grant-cap").onclick = async function () {
  await capabilityAction("grant", "已授予");
};

$("revoke-cap").onclick = async function () {
  await capabilityAction("revoke", "已撤回");
};

async function capabilityAction(path, label) {
  const capability = $("capability").value.trim();
  if (!capability) { log("先填一个能力策略标识", "err"); return; }
  try {
    const result = await api(path, { capability: capability });
    $("revoke-out").textContent = JSON.stringify(result, null, 2);
    if (path === "revoke") {
      log(
        label + " " + capability + "：覆盖 " + result.events_covered +
        " 个事件、失效 " + result.memories_invalidated + " 条记忆",
        "ok"
      );
    } else {
      log(label + " " + capability + "；当前生效：" + result.granted.join("、"), "ok");
    }
  } catch (error) {
    $("revoke-out").textContent = error.message;
    log(label + "失败：" + error.message, "err");
  }
  refresh();
}

$("run-retention").onclick = async function () {
  try {
    const result = await api("retention", {});
    $("retention-out").textContent = JSON.stringify(result, null, 2);
    log(
      "保留期：隐藏 " + result.tombstoned + " 条、清理 " + result.purged +
      " 条、裁掉审计 " + result.audit_pruned + " 条；仍待清理 " + result.awaiting_purge,
      "ok"
    );
  } catch (error) {
    $("retention-out").textContent = error.message;
    log("保留期执行失败：" + error.message, "err");
  }
  refresh();
};

$("forget").onclick = async function () {
  const id = $("forget-id").value.trim();
  if (!id) { log("先填一个记忆标识", "err"); return; }
  try {
    const result = await api("forget", { memory_id: id });
    $("retention-out").textContent = JSON.stringify(result, null, 2);
    log(
      result.tombstoned > 0
        ? "已删除，它现在不可见了"
        : "这条此前已经删过（重复删除是幂等的）",
      "ok"
    );
  } catch (error) {
    $("retention-out").textContent = error.message;
    log("删除失败：" + error.message, "err");
  }
  refresh();
};

$("run-loop").onclick = async function () {
  $("run-loop").disabled = true;
  try {
    const result = await api("loop", {
      risk: $("select-risk").value,
      rounds: parseInt($("loop-rounds").value, 10) || 1,
    });
    $("select-out").textContent = JSON.stringify(result, null, 2);
    result.rounds.forEach(function (round) {
      log("第 " + round.round + " 轮：" + describeStep(round.outcome), "ok");
    });
  } catch (error) {
    $("select-out").textContent = error.message;
    log("闭环失败：" + error.message, "err");
  }
  $("run-loop").disabled = false;
  refresh();
};

function describeStep(outcome) {
  switch (outcome.kind) {
    case "advanced":
      switch (outcome.step.kind) {
        case "observation":
          return "补了一次观测：" + outcome.step.subject_ref;
        case "claim":
          return "记下结论" + (outcome.step.recorded ? "（新增）" : "（已记过，跳过）") +
            "：" + outcome.step.statement;
        default:
          return "未能推进（" + outcome.step.candidate_kind + "）：" + outcome.step.reason;
      }
    case "needs_input":
      return "需要输入：" + outcome.missing.join("；");
    case "idle":
      return "空转：没有可推进的候选";
    case "finished":
      return "结束：" + outcome.reason;
    default:
      return outcome.kind;
  }
}

$("message").addEventListener("keydown", function (event) {
  if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) $("send").click();
});

refresh();
refreshModel();
</script>
</body>
</html>
"#;

/// 页面路径是否是控制台首页。
pub fn is_index(request: &Request) -> bool {
    request.method == "GET" && request.path == "/"
}
