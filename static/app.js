/* ==========================================================================
   SteamPilot 前端逻辑
   - Tauri 环境下调用真实命令；浏览器直开时走同构 mock，界面完全可点
   - 首页是四步引导：装证书 → 优选 IP → 选服务 → 启动
   ========================================================================== */

const NODE_DATA = window.NODE_DATA || [];

const CDN_META = {
  akamai: {
    short: "AK",
    domains: "Steam 商店 / 社区 / 创意工坊",
    source: "302 精选池 + 热段 /24",
    speed: "origin-a.akamaihd.net/EA app.dmg",
  },
  fastly: {
    short: "FY",
    domains: "GitHub 静态资源",
    source: "Fastly 段 + api.github.com/meta",
    speed: "desktop.githubusercontent.com/GitHubDesktop.exe",
  },
  "epic-fastly": {
    short: "E1",
    domains: "Epic 下载（主）",
    source: "Epic 实测 Fastly 节点",
    speed: "fastly-download.epicgames.com/EpicInstaller.msi",
  },
  "epic-akamai": {
    short: "E2",
    domains: "Epic 下载（备）",
    source: "Epic 实测 Akamai 节点",
    speed: "epicgames-download1.akamaized.net/EpicInstaller.msi",
  },
  cloudflare: {
    short: "CF",
    domains: "Nexus Mods 等 CF 站点",
    source: "cloudflare.com/ips-v4（15 段）",
    speed: "speed.cloudflare.com/__down",
  },
  cloudfront: {
    short: "CFr",
    domains: "AWS 资源",
    source: "ip-ranges.amazonaws.com（117 段）",
    speed: "images-assets.nasa.gov/PIA12348~orig.jpg",
  },
};

// 首页用大白话说明服务，不堆域名
const SITE_HINT = {
  steam: "商店、社区、创意工坊",
  epic: "Epic 商店与游戏下载",
  nexus: "模组下载站",
  github: "代码仓库与文件下载",
};

const GROUPS = [
  { id: "game", name: "游戏平台" },
  { id: "dev", name: "开发者工具" },
  { id: "custom", name: "自定义" },
];

const ICON = {
  game: '<rect x="2.8" y="8" width="18.4" height="9" rx="4.2"/><path d="M7.6 11.2v3.4M5.9 12.9h3.4"/><circle cx="16.2" cy="12" r=".9"/><circle cx="18.1" cy="14" r=".9"/>',
  dev: '<path d="m8.5 8.5-4 3.5 4 3.5M15.5 8.5l4 3.5-4 3.5M13.4 6.5l-2.8 11"/>',
  custom: '<circle cx="12" cy="12" r="8.2"/><path d="M12 8.6v6.8M8.6 12h6.8"/>',
  info: '<circle cx="12" cy="12" r="8"/><path d="M12 11v5M12 8h.01"/>',
  chevron: '<path d="m9 6 6 6-6 6"/>',
  play: '<path d="M8 5.5v13l11-6.5z" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6.5" y="6.5" width="11" height="11" rx="2.2" fill="currentColor" stroke="none"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M15 5.5H6a2 2 0 0 0-2 2V16"/>',
};

const svg = (name) => `<svg viewBox="0 0 24 24">${ICON[name] || ""}</svg>`;
const clone = (o) => JSON.parse(JSON.stringify(o));
const inTauri = !!(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke);

/* ------------------------------- 初始状态 ------------------------------- */
let state = {
  running: false,
  caInstalled: false,
  nodesOptimized: false,
  startup: false,
  silentStart: true,
  theme: "light",
  providers: [
    { id: "akamai", name: "Akamai", enabled: true, status: "已优选", nodes: 14, latencyMs: 471, note: "Steam 商店 / 社区 / 创意工坊" },
    { id: "fastly", name: "Fastly", enabled: true, status: "已优选", nodes: 56, latencyMs: 329, note: "GitHub 静态资源" },
    { id: "epic-fastly", name: "Epic / Fastly", enabled: true, status: "已优选", nodes: 3, latencyMs: 250, note: "Epic 下载（真域名）" },
    { id: "epic-akamai", name: "Epic / Akamai", enabled: true, status: "已优选", nodes: 15, latencyMs: 433, note: "Epic 下载备用" },
    { id: "cloudflare", name: "Cloudflare", enabled: true, status: "已优选", nodes: 10, latencyMs: 659, note: "Nexus Mods 等 CF 站点（Anycast）" },
    { id: "cloudfront", name: "CloudFront", enabled: true, status: "已优选", nodes: 12, latencyMs: 672, note: "AWS 资源（Anycast）" },
  ],
  sites: [
    {
      id: "steam",
      name: "Steam 商店与社区",
      group: "game",
      domains: "store.steampowered.com · steamcommunity.com · api.steampowered.com · avatars.steamstatic.com",
      provider: "akamai",
      enabled: true,
      note: "别名 SNI + 真实 Host，证书按别名严格校验",
      custom: false,
      features: [
        {
          id: "workshop-fix",
          name: "创意工坊预览大图修复",
          note: "工坊页 iframe_api 加载失败导致预览不显示，注入一次 onYouTubeIframeAPIReady() 调用即可",
          enabled: false,
        },
      ],
    },
    {
      id: "epic",
      name: "Epic Games",
      group: "game",
      domains: "store.epicgames.com · fastly-download.epicgames.com · epicgames-download1.akamaized.net",
      provider: "epic-fastly",
      enabled: true,
      note: "下载走 Epic 独立 Fastly/Akamai 池（真域名）；网页直连",
      custom: false,
      features: [],
    },
    {
      id: "nexus",
      name: "Nexus Mods",
      group: "game",
      domains: "www.nexusmods.com · staticdelivery.nexusmods.com · cdn.nexusmods.com",
      provider: "cloudflare",
      enabled: true,
      note: "Cloudflare Anycast 优选池",
      custom: false,
      features: [],
    },
    {
      id: "github",
      name: "GitHub",
      group: "dev",
      domains: "github.com · api.github.com · raw.githubusercontent.com · github.githubassets.com",
      provider: "fastly",
      enabled: true,
      note: "B 组 Fastly 优选为主，A 组微软自有点位备份",
      custom: false,
      features: [],
    },
  ],
  nodes: NODE_DATA,
  policy: {
    stage1Keep: 8,
    stage2Keep: 4,
    stage3Keep: 2,
    concurrency: 8,
    handshakeTimeoutMs: 4000,
    sampleBytes: 8,
    alias: "auto",
    reuseRecent: true,
    randomizeTail: true,
    batchSize: 1,
    minSpeedMbps: 10,
    totalLimitMbps: 1000,
    // 每条线路用自己的测速标的，加 Cloudflare 时只加这一条映射即可
    speedTestUrls: {
      akamai: "https://origin-a.akamaihd.net/EA%20app.dmg",
      fastly: "https://desktop.githubusercontent.com/GitHubDesktop.exe",
      "epic-fastly": "https://fastly-download.epicgames.com/Builds/UnrealEngineLauncher/Installers/Windows/EpicInstaller-20.3.3.msi",
      "epic-akamai": "https://epicgames-download1.akamaized.net/Builds/UnrealEngineLauncher/Installers/Windows/EpicInstaller-20.3.3.msi",
      cloudflare: "https://speed.cloudflare.com/__down?bytes=8388608",
      cloudfront: "https://images-assets.nasa.gov/image/PIA12348/PIA12348~orig.jpg",
    },
  },
  capture: { autoRestore: true },
};

const ui = {
  view: "home",
  closedGroups: new Set(),
  openRows: new Set(),
  adding: false,
  autoStart: false,
  updateCheck: true,
  optimizing: false,
  retesting: null,
  lastTested: "",
  drawerOpen: false,
  drawerDraft: null,
  updateInfo: null,
};

/* --------------------------------- mock --------------------------------- */
function mockInvoke(cmd, args = {}) {
  switch (cmd) {
    case "get_state":
      break;
    case "set_running":
      if (args.running && !state.caInstalled) break;
      state.running = !!args.running;
      break;
    case "toggle_provider": {
      const p = state.providers.find((x) => x.id === args.id);
      if (p) p.enabled = !p.enabled;
      break;
    }
    case "toggle_site": {
      const s = state.sites.find((x) => x.id === args.id);
      if (s) s.enabled = !s.enabled;
      break;
    }
    case "set_sites_enabled":
      state.sites.forEach((s) => {
        if (args.ids.includes(s.id)) s.enabled = !!args.enabled;
      });
      break;
    case "toggle_feature": {
      const s = state.sites.find((x) => x.id === args.site);
      const f = s && s.features.find((x) => x.id === args.feature);
      if (f) f.enabled = !f.enabled;
      break;
    }
    case "add_custom_site":
      state.sites.push({
        id: `custom-${state.sites.length + 1}`,
        name: args.name,
        group: "custom",
        domains: args.domains,
        provider: "auto",
        enabled: true,
        note: "用户自定义规则",
        custom: true,
        features: [],
      });
      break;
    case "remove_site": {
      const i = state.sites.findIndex((x) => x.id === args.id && x.custom);
      if (i >= 0) state.sites.splice(i, 1);
      break;
    }
    case "set_theme":
      state.theme = args.theme;
      break;
    case "set_flags":
      if (args.startup != null) state.startup = args.startup;
      if (args.silentStart != null) state.silentStart = args.silentStart;
      break;
    case "update_policy": {
      const p = clone(args.patch);
      p.stage1Keep = Math.min(256, Math.max(0, p.stage1Keep));
      p.stage2Keep = Math.min(p.stage1Keep || 1, Math.max(1, p.stage2Keep));
      p.stage3Keep = Math.min(p.stage2Keep, Math.max(1, p.stage3Keep));
      p.concurrency = Math.min(128, Math.max(1, p.concurrency));
      p.batchSize = Math.min(64, Math.max(1, p.batchSize));
      p.minSpeedMbps = Math.min(10000, Math.max(1, p.minSpeedMbps));
      p.totalLimitMbps = Math.max(10, p.totalLimitMbps);
      p.speedTestUrls = Object.fromEntries(
        Object.entries(p.speedTestUrls || {}).map(([k, v]) => [k, String(v).trim()])
      );
      state.policy = p;
      break;
    }
    case "update_capture":
      state.capture = clone(args.patch);
      break;
    case "install_ca":
      state.caInstalled = true;
      break;
    case "uninstall_ca":
      state.caInstalled = false;
      state.running = false;
      break;
    case "refresh_nodes":
      state.nodesOptimized = true;
      break;
    case "check_update":
      return { current: "0.1.0", latest: "0.1.0", available: false, url: "https://github.com/Linyumoe/SteamPilot/releases/latest", notes: "", error: "" };
    case "open_releases":
      break;
    default:
      console.warn("未实现的命令", cmd, args);
  }
  return clone(state);
}

async function invoke(cmd, args = {}) {
  const result = inTauri ? await window.__TAURI__.core.invoke(cmd, args) : mockInvoke(cmd, args);
  if (result && result.providers) {
    state = result;
    renderAll();
  }
  return result;
}

/* -------------------------------- 工具函数 -------------------------------- */
const esc = (v) =>
  String(v == null ? "" : v)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

const enabledSites = () => state.sites.filter((s) => s.enabled);
const providerOf = (id) => state.providers.find((p) => p.id === id);
const nodesOf = (id) => state.nodes.filter((n) => n.provider === id);

function setVal(id, value) {
  const el = document.getElementById(id);
  if (!el || document.activeElement === el) return;
  el.value = value;
}

function setChecked(id, value) {
  const el = document.getElementById(id);
  if (!el) return;
  el.checked = !!value;
}

/* --------------------------------- 渲染 --------------------------------- */
function renderNav() {
  const enabled = enabledSites().length;
  const cdnOn = state.providers.filter((p) => p.enabled).length;
  document.getElementById("nav-sub-home").textContent = state.running ? "正在加速" : "4 步即可开始";
  document.getElementById("nav-sub-services").textContent = `${enabled} / ${state.sites.length}`;
  document.getElementById("nav-sub-cdn").textContent = `${cdnOn} / ${state.providers.length} 线路启用`;
  document.getElementById("side-status").textContent = state.running ? "正在加速" : "程序已就绪";
  document.getElementById("side-dot").className = `dot${state.running ? " on" : ""}`;
}

function renderHome() {
  const on = enabledSites().length;
  const caDone = !!state.caInstalled;
  const optDone = !!state.nodesOptimized;
  const mark = (id, done) => {
    const el = document.querySelector(`#${id} .step-no`);
    el.textContent = done ? "✓" : el.dataset.num;
    el.classList.toggle("done", done);
  };
  mark("step-ca", caDone);
  mark("step-nodes", optDone);
  mark("step-sites", on > 0);
  mark("step-run", state.running);

  // 1. 证书
  const caState = document.getElementById("step-ca-state");
  caState.textContent = caDone ? "已装好" : "未安装";
  caState.className = `step-state${caDone ? " ok" : ""}`;
  document.getElementById("step-ca-btn").textContent = caDone ? "已装好" : "安装证书";
  document.getElementById("step-ca-btn").disabled = caDone;

  // 2. 线路测速
  const optState = document.getElementById("step-nodes-state");
  optState.textContent = ui.optimizing ? "正在测速…" : optDone ? `找到 ${state.nodes.length} 条可用线路` : "未测速";
  optState.className = `step-state${optDone ? " ok" : ""}`;
  document.getElementById("step-nodes-btn").textContent = optDone ? "重新测速" : "开始测速";
  document.getElementById("step-nodes-btn").disabled = ui.optimizing;

  // 3. 网站
  document.getElementById("step-sites-state").textContent = `已勾选 ${on} 个`;
  document.getElementById("home-sites").innerHTML = state.sites
    .map(
      (s) => `<div class="home-site">
        <div class="home-site-txt">
          <strong>${esc(s.name)}</strong>
          <small>${esc(SITE_HINT[s.id] || s.domains.split(" · ")[0])}</small>
        </div>
        <input type="checkbox" class="sw sm" data-site="${esc(s.id)}" ${s.enabled ? "checked" : ""} />
      </div>`
    )
    .join("");

  // 4. 启动
  const ready = caDone && optDone && on > 0;
  const runBtn = document.getElementById("home-run");
  document.getElementById("home-run-label").textContent = state.running ? "停止加速" : "启动加速";
  runBtn.classList.toggle("running", state.running);
  runBtn.querySelector("svg").innerHTML = state.running ? ICON.stop : ICON.play;
  runBtn.disabled = !state.running && !ready;
  document.getElementById("home-hint").textContent = state.running
    ? `正在为 ${on} 个网站加速，其他网站不受影响。`
    : ready
      ? "准备好了，点上面的按钮就能打开网站了。"
      : !caDone
        ? "第 1 步：先安装证书。"
        : !optDone
          ? "第 2 步：先测一遍线路。"
          : "第 3 步：至少勾选一个网站。";
}

function addFormHtml() {
  const row = `<button class="add-row" data-add-custom>${svg("custom")}<span>添加自定义服务</span></button>`;
  if (!ui.adding) return row;
  return `${row}
    <form class="add-form" id="custom-form">
      <input id="custom-name" placeholder="名称，例如 Nexus Mods" autocomplete="off" required />
      <input id="custom-domains" placeholder="域名，逗号分隔" autocomplete="off" required />
      <div class="form-actions">
        <button type="button" class="btn-sm" data-cancel-custom>取消</button>
        <button type="submit" class="btn-sm accent">添加</button>
      </div>
    </form>`;
}

function featureHtml(site, f) {
  return `<div class="feat">
    <div class="feat-txt">
      <strong>${esc(f.name)}</strong>
      <small>${esc(f.note)}</small>
    </div>
    <input type="checkbox" class="sw sm" data-feature="${esc(site.id)}:${esc(f.id)}" ${f.enabled ? "checked" : ""} />
  </div>`;
}

function renderServices() {
  const tree = document.getElementById("service-tree");
  let html = "";

  GROUPS.forEach((g) => {
    const all = state.sites.filter((s) => s.group === g.id);
    if (!all.length) return;

    const on = all.filter((s) => s.enabled).length;
    const collapsed = ui.closedGroups.has(g.id);

    html += `<div class="grp${collapsed ? " collapsed" : ""}" data-group="${g.id}">
      <div class="grp-head">
        <button class="chev" data-collapse="${g.id}">${svg("chevron")}</button>
        <input type="checkbox" class="cb" data-group-check="${g.id}" />
        <span class="grp-ico">${svg(g.id)}</span>
        <span class="grp-name">${g.name}</span>
        <span class="grp-count">${on} / ${all.length}</span>
      </div>
      <div class="grp-body">`;

    all.forEach((s) => {
      const open = ui.openRows.has(s.id);
      const hasFix = s.features && s.features.length > 0;
      html += `<div class="row${s.enabled ? "" : " dim"}${open ? " open" : ""}" data-row="${s.id}">
        <input type="checkbox" class="cb" data-site="${s.id}" ${s.enabled ? "checked" : ""} />
        <span class="row-name">${esc(s.name)}</span>
        ${hasFix ? `<span class="chip fix">含修复</span>` : ""}
        <span class="chip ${esc(s.provider)}">${esc(s.provider)}</span>
        <button class="row-info" data-info="${s.id}" title="查看规则详情">${svg("info")}</button>
      </div>`;
      if (open) {
        html += `<div class="row-detail">
          <div><span>域名</span><code>${esc(s.domains)}</code></div>
          <div><span>线路</span><code>${esc(s.provider)}</code></div>
          <div><span>说明</span><code>${esc(s.note)}</code></div>
          ${(s.features || []).map((f) => featureHtml(s, f)).join("")}
          ${s.custom ? `<div class="detail-actions"><button class="btn-sm" data-remove="${esc(s.id)}">删除这条规则</button></div>` : ""}
        </div>`;
      }
    });

    html += `</div></div>`;
  });

  tree.innerHTML = html + addFormHtml();

  tree.querySelectorAll("[data-group-check]").forEach((cb) => {
    const list = state.sites.filter((s) => s.group === cb.dataset.groupCheck);
    const on = list.filter((s) => s.enabled).length;
    cb.checked = on === list.length && list.length > 0;
    cb.indeterminate = on > 0 && on < list.length;
  });

  document.getElementById("svc-count").textContent = `已启用 ${enabledSites().length} / ${state.sites.length}`;
}

function renderCdn() {
  const refreshIco = '<path d="M20 11a8 8 0 1 0-2.3 6.3"/><path d="M20 5v6h-6"/>';
  document.getElementById("cdn-grid").innerHTML = state.providers
    .map((p) => {
      const meta = CDN_META[p.id] || {};
      const nodes = nodesOf(p.id);
      const tip = nodes.length
        ? nodes.slice(0, 3).map((n) => `${n.ip}  ·  ${n.latencyMs}ms  ·  ${n.status}`).join("\n")
        : "暂无优选结果，先跑一次测速";
      const time = ui.retesting === p.id ? "测速中…" : ui.lastTested ? `测速 ${ui.lastTested}` : "尚未测速";

      return `<article class="cdn${p.enabled ? "" : " off"}" title="${esc(tip)}">
        <span class="cdn-logo ${esc(p.id)}">${esc(meta.short || "CDN")}</span>
        <div class="cdn-id">
          <div class="cdn-title"><strong>${esc(p.name)}</strong><span class="cdn-count">${p.nodes || nodes.length} 个优选 IP</span></div>
          <small>${esc(meta.domains || p.note)} · ${esc(time)} · 自动检测：关闭</small>
        </div>
        <div class="cdn-actions">
          <button class="btn-icon sm" data-cdn-retest="${esc(p.id)}" title="重新测速" ${ui.optimizing ? "disabled" : ""}><svg viewBox="0 0 24 24">${refreshIco}</svg></button>
        </div>
        <input type="checkbox" class="sw" data-provider="${esc(p.id)}" ${p.enabled ? "checked" : ""} />
      </article>`;
    })
    .join("");
  document.getElementById("cdn-grid").setAttribute("data-ready", "1");

  setVal("cdn-total-limit", state.policy.totalLimitMbps);
  document.getElementById("cdn-last-test").textContent = ui.lastTested ? `上次测速 ${ui.lastTested}` : "尚未测速";
}

function renderTester(src) {
  const p = src || state.policy;
  setChecked("ts-reuse", p.reuseRecent);
  setChecked("ts-random-tail", p.randomizeTail);
  setVal("ts-min-speed", p.minSpeedMbps);
  setVal("ts-keep", p.stage3Keep);
  setVal("ts-stage1", p.stage1Keep);
  setVal("ts-batch", p.batchSize);
  setVal("ts-concurrency", p.concurrency);
  setVal("ts-timeout", p.handshakeTimeoutMs);
  setVal("ts-sample", p.sampleBytes);
  setVal("ts-alias", p.alias);
  setVal("ts-stage2", p.stage2Keep);

  // 测速标的按线路渲染：每条线路读自己的 URL，互不影响。
  const urls = p.speedTestUrls || {};
  document.getElementById("ts-url-list").innerHTML = state.providers
    .map((prov) => {
      const meta = CDN_META[prov.id] || {};
      return `<label class="url-row${prov.enabled ? "" : " dim"}">
        <span class="url-tag ${esc(prov.id)}">${esc(meta.short || prov.id)}</span>
        <span class="url-name">${esc(prov.name)}</span>
        <input class="text-input url-input" data-url-provider="${esc(prov.id)}" type="url" spellcheck="false" value="${esc(urls[prov.id] || "")}" placeholder="https://…" />
      </label>`;
    })
    .join("");
  document.getElementById("ts-ip-count").textContent = `${state.nodes.length} 个 IP`;
}

function renderSettings() {
  setVal("set-theme", state.theme);
  setChecked("set-startup", state.startup);
  setChecked("set-silent", state.silentStart);
  setChecked("set-auto", ui.autoStart);
  setChecked("set-update", ui.updateCheck);

  setChecked("capture-restore", state.capture.autoRestore);

  const caOn = state.caInstalled;
  document.getElementById("ca-pill").textContent = caOn ? "已信任" : "未安装";
  document.getElementById("ca-pill").className = `pill ${caOn ? "ok" : "wait"}`;
  document.getElementById("ca-status").textContent = caOn ? "本地根证书已安装并被系统信任" : "本地根证书未安装";
  renderUpdateStatus();
}

function applyTheme() {
  document.documentElement.dataset.theme = state.theme;
}

function renderAll() {
  applyTheme();
  renderNav();
  renderHome();
  renderServices();
  renderCdn();
  renderTester();
  renderSettings();
}

/* --------------------------------- 交互 --------------------------------- */
function showToast(msg) {
  const el = document.getElementById("toast");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => el.classList.remove("show"), 1800);
}

function renderUpdateStatus() {
  const el = document.getElementById("update-status");
  const btn = document.getElementById("btn-check-update");
  if (!el) return;
  const info = ui.updateInfo;
  el.className = "";
  if (!info) {
    el.textContent = "当前版本 v0.1.0 · GitHub Releases 通道";
    return;
  }
  if (info.available) {
    el.textContent = `发现新版本 v${info.latest} · 点右侧查看`;
    el.className = "has-update";
    if (btn) btn.textContent = "前往下载";
  } else if (info.error) {
    // 检查失败（常见于 GitHub 访问受限）时保留手动入口，不让用户卡死。
    el.textContent = `${info.error} · 可手动打开发布页`;
    el.className = "check-error";
    if (btn) btn.textContent = "发布页";
  } else {
    el.textContent = `当前版本 v${info.current} · 已是最新版本`;
    if (btn) btn.textContent = "检查";
  }
}

async function checkUpdate(manual = true) {
  const btn = document.getElementById("btn-check-update");
  if (btn) btn.disabled = true;
  if (manual) showToast("正在检查更新…");
  try {
    const info = await invoke("check_update");
    ui.updateInfo = info;
    renderUpdateStatus();
    if (manual) {
      if (info.available) showToast(`发现新版本 v${info.latest}`);
      else if (info.error) showToast(info.error);
      else showToast("已是最新版本");
    }
  } catch (err) {
    ui.updateInfo = { current: "0.1.0", latest: "", available: false, url: "", notes: "", error: "检查更新失败" };
    renderUpdateStatus();
    if (manual) showToast("检查更新失败，请稍后重试");
  } finally {
    if (btn) btn.disabled = false;
  }
}

function switchView(view) {
  if (view === "home") {
    document.body.classList.remove("advanced");
  } else {
    document.body.classList.add("advanced");
    if (view === "advanced" || !["services", "cdn", "settings"].includes(view)) {
      view = "services";
    }
  }
  ui.view = view;
  document.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  document.querySelectorAll(".view").forEach((v) => v.classList.toggle("active", v.id === `view-${view}`));
}

function readPolicy() {
  const num = (id, fb) => {
    const el = document.getElementById(id);
    const v = el ? parseInt(el.value, 10) : NaN;
    return Number.isFinite(v) ? v : fb;
  };
  const bool = (id, fb) => {
    const el = document.getElementById(id);
    return el ? !!el.checked : fb;
  };
  const val = (id, fb) => {
    const el = document.getElementById(id);
    return el ? el.value : fb;
  };
  // URL 以当前 state 为底，只覆盖抽屉里实际渲染出来的线路，避免关掉抽屉后丢字段
  const urls = { ...(state.policy.speedTestUrls || {}) };
  document.querySelectorAll("[data-url-provider]").forEach((input) => {
    urls[input.dataset.urlProvider] = input.value.trim() || urls[input.dataset.urlProvider] || "";
  });
  return {
    ...state.policy,
    stage1Keep: num("ts-stage1", state.policy.stage1Keep),
    stage2Keep: num("ts-stage2", state.policy.stage2Keep),
    stage3Keep: num("ts-keep", state.policy.stage3Keep),
    concurrency: num("ts-concurrency", state.policy.concurrency),
    handshakeTimeoutMs: num("ts-timeout", state.policy.handshakeTimeoutMs),
    sampleBytes: num("ts-sample", state.policy.sampleBytes),
    alias: val("ts-alias", state.policy.alias),
    reuseRecent: bool("ts-reuse", state.policy.reuseRecent),
    randomizeTail: bool("ts-random-tail", state.policy.randomizeTail),
    batchSize: num("ts-batch", state.policy.batchSize),
    minSpeedMbps: num("ts-min-speed", state.policy.minSpeedMbps),
    totalLimitMbps: num("cdn-total-limit", state.policy.totalLimitMbps),
    speedTestUrls: urls,
  };
}

function openTester() {
  ui.drawerDraft = clone(state.policy);
  ui.drawerOpen = true;
  renderTester(ui.drawerDraft);
  document.getElementById("test-drawer").classList.add("open");
  document.getElementById("drawer-mask").classList.add("open");
  document.getElementById("test-drawer").setAttribute("aria-hidden", "false");
}

function closeTester() {
  ui.drawerOpen = false;
  ui.drawerDraft = null;
  document.getElementById("test-drawer").classList.remove("open");
  document.getElementById("drawer-mask").classList.remove("open");
  document.getElementById("test-drawer").setAttribute("aria-hidden", "true");
  // 不在这里 renderCdn：抽屉开合不该让整张 CDN 列表重播入场动画
}

async function saveTester() {
  await invoke("update_policy", { patch: readPolicy() });
  closeTester();
  showToast("测速设置已保存");
}

async function retest(providerId) {
  if (ui.optimizing) return showToast("正在测速中，请稍候");
  ui.optimizing = true;
  ui.retesting = providerId || null;
  renderCdn();
  renderHome();
  try {
    await invoke("refresh_nodes");
    ui.lastTested = new Date().toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
    showToast(providerId ? "该线路测速完成" : "测速完成，已挑出可用线路");
  } finally {
    ui.optimizing = false;
    ui.retesting = null;
    renderCdn();
    renderHome();
  }
}

function readCapture() {
  return { autoRestore: document.getElementById("capture-restore").checked };
}

async function quitApp() {
  if (state.running) await invoke("set_running", { running: false });
  try {
    if (inTauri && window.__TAURI__.window) await window.__TAURI__.window.getCurrentWindow().close();
    else showToast("已停止服务（浏览器预览下不关闭页面）");
  } catch {
    showToast("已退出");
  }
}

/* 事件：点击 */
document.addEventListener("click", async (e) => {
  const nav = e.target.closest("[data-view]");
  if (nav) return switchView(nav.dataset.view);

  const goto = e.target.closest("[data-goto]");
  if (goto) return switchView(goto.dataset.goto);

  // 首页第 1 步：证书
  if (e.target.closest("#step-ca-btn") || e.target.closest("#install-ca")) {
    await invoke("install_ca");
    return showToast("证书装好了，可以继续下一步");
  }
  if (e.target.closest("#uninstall-ca")) {
    await invoke("uninstall_ca");
    return showToast("已取消证书信任");
  }

  // 首页第 2 步 / CDN 页：重新测速
  if (e.target.closest("#step-nodes-btn") || e.target.closest("#btn-refresh")) {
    return retest();
  }
  const cdnRetest = e.target.closest("[data-cdn-retest]");
  if (cdnRetest) return retest(cdnRetest.dataset.cdnRetest);

  // 设置页：检查更新 / 有新版或检查失败时打开发布页（保证任何状态都有手动入口）
  if (e.target.closest("#btn-check-update")) {
    if (ui.updateInfo && (ui.updateInfo.available || ui.updateInfo.error)) {
      if (inTauri) await invoke("open_releases");
      else window.open("https://github.com/Linyumoe/SteamPilot/releases/latest", "_blank");
      return;
    }
    return checkUpdate(true);
  }

  // 测速设置抽屉：统一从页头按钮进入
  if (e.target.closest("#cdn-settings-btn")) return openTester();
  if (e.target.closest("#drawer-close") || e.target.closest("#drawer-cancel") || e.target.closest("#drawer-mask")) return closeTester();
  if (e.target.closest("#drawer-save")) return saveTester();

  // 首页第 4 步：启动
  if (e.target.closest("#home-run")) {
    if (!state.running) {
      if (!state.caInstalled) return showToast("第 1 步：先安装证书");
      if (!state.nodesOptimized) return showToast("第 2 步：先测一遍线路");
      if (enabledSites().length === 0) return showToast("第 3 步：至少勾选一个网站");
    }
    await invoke("set_running", { running: !state.running });
    return showToast(state.running ? "加速已启动，现在可以打开网站试试" : "已停止加速，网络恢复原样");
  }

  if (e.target.closest("#home-theme")) {
    const next = state.theme === "dark" ? "light" : "dark";
    await invoke("set_theme", { theme: next });
    return showToast(next === "dark" ? "已切换深色主题" : "已切换浅色主题");
  }
  if (e.target.closest("#home-quit") || e.target.closest("#quit-app")) {
    return quitApp();
  }

  const collapse = e.target.closest("[data-collapse]");
  if (collapse) {
    const id = collapse.dataset.collapse;
    ui.closedGroups.has(id) ? ui.closedGroups.delete(id) : ui.closedGroups.add(id);
    return renderServices();
  }

  const info = e.target.closest("[data-info]");
  if (info) {
    const id = info.dataset.info;
    ui.openRows.has(id) ? ui.openRows.delete(id) : ui.openRows.add(id);
    return renderServices();
  }

  const rm = e.target.closest("[data-remove]");
  if (rm) {
    await invoke("remove_site", { id: rm.dataset.remove });
    return showToast("已删除该自定义规则");
  }

  if (e.target.closest("[data-add-custom]")) {
    ui.adding = !ui.adding;
    renderServices();
    const inp = document.getElementById("custom-name");
    if (inp) inp.focus();
    return;
  }
  if (e.target.closest("[data-cancel-custom]")) {
    ui.adding = false;
    return renderServices();
  }

});

/* 事件：勾选 */
document.addEventListener("change", async (e) => {
  const t = e.target;

  if (t.dataset.site) {
    await invoke("toggle_site", { id: t.dataset.site });
    return;
  }
  if (t.dataset.feature) {
    const [site, feature] = t.dataset.feature.split(":");
    await invoke("toggle_feature", { site, feature });
    return showToast(t.checked ? "已开启该修复" : "已关闭该修复");
  }
  if (t.dataset.groupCheck) {
    const ids = state.sites.filter((s) => s.group === t.dataset.groupCheck).map((s) => s.id);
    await invoke("set_sites_enabled", { ids, enabled: t.checked });
    return showToast(t.checked ? "整组已启用" : "整组已停用");
  }
  if (t.dataset.provider) {
    await invoke("toggle_provider", { id: t.dataset.provider });
    return showToast("线路开关已更新");
  }
  if (t.id === "set-theme") {
    await invoke("set_theme", { theme: t.value });
    return showToast(t.value === "dark" ? "已切换深色主题" : "已切换浅色主题");
  }
  if (t.id === "set-startup" || t.id === "set-silent") {
    await invoke("set_flags", {
      startup: document.getElementById("set-startup").checked,
      silentStart: document.getElementById("set-silent").checked,
    });
    return showToast("启动设置已更新");
  }
  if (t.id === "set-auto") {
    ui.autoStart = t.checked;
    return showToast(t.checked ? "启动后将自动加速" : "启动后不自动加速");
  }
  if (t.id === "set-update") {
    ui.updateCheck = t.checked;
    return;
  }

  if (t.id === "cdn-total-limit") {
    const limit = parseInt(document.getElementById("cdn-total-limit").value, 10);
    await invoke("update_policy", {
      patch: {
        ...state.policy,
        totalLimitMbps: Number.isFinite(limit) ? limit : state.policy.totalLimitMbps,
      },
    });
    return showToast("测速总限速已更新");
  }

  if (t.id === "capture-restore") {
    await invoke("update_capture", { patch: readCapture() });
    return showToast("代理恢复设置已更新");
  }
});

/* 事件：提交自定义规则 */
document.addEventListener("submit", async (e) => {
  if (e.target.id !== "custom-form") return;
  e.preventDefault();
  const name = document.getElementById("custom-name").value.trim();
  const domains = document.getElementById("custom-domains").value.trim();
  if (!name || !domains) return;
  ui.adding = false;
  await invoke("add_custom_site", { name, domains });
  showToast(`已添加 ${name}`);
});

/* --------------------------------- 启动 --------------------------------- */
document.addEventListener("DOMContentLoaded", async () => {
  if (inTauri) {
    try {
      await window.__TAURI__.event.listen("state-changed", (ev) => {
        state = ev.payload;
        renderAll();
      });
    } catch (err) {
      console.warn("事件监听失败", err);
    }
  }
  await invoke("get_state");
  const qTheme = new URLSearchParams(location.search).get("theme");
  if (qTheme === "light" || qTheme === "dark") state.theme = qTheme;
  renderAll();
  switchView("home");
  // 启动后静默检查一次更新（设置页「检查更新」开关控制），有新版会在设置页标出。
  if (ui.updateCheck) setTimeout(() => checkUpdate(false), 1200);
});
