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
  $("owner").textContent = state.owner + "　模型调用 " + state.model_calls + " 次";

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

$("message").addEventListener("keydown", function (event) {
  if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) $("send").click();
});

refresh();
</script>
</body>
</html>
"#;

/// 页面路径是否是控制台首页。
pub fn is_index(request: &Request) -> bool {
    request.method == "GET" && request.path == "/"
}
