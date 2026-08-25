const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// DOM
const $ = (id) => document.getElementById(id);

const views = {
  main: $("main-view"),
  settings: $("settings-view"),
};

const modal = $("edit-modal");
const confirmModal = $("confirm-modal");
const toast = $("toast");

let currentEditId = null;
let editingProviderId = null;
let confirmAction = null;
let currentView = "main";
let lastSnapshot = [];
let discoveredCpaAccounts = [];
let cpaDiscoveryGeneration = 0;
const expandedProviderGroups = new Set();
const consumingCodexProviders = new Set();
let resizeFrame = null;

// ==================== utilities ====================

function showToast(msg, isError = false) {
  toast.textContent = msg;
  toast.classList.remove("hidden", "error");
  if (isError) toast.classList.add("error");
  setTimeout(() => toast.classList.add("hidden"), 2400);
}

function setStatus(level, text, target = "status-bar") {
  const bar = $(target);
  if (!bar) return;
  const dot = bar.querySelector(".status-dot");
  const txt = bar.querySelector(".status-text");
  if (dot) {
    dot.classList.remove("ok", "warn", "err");
    if (level) dot.classList.add(level);
  }
  if (txt) txt.textContent = text;
}

function setCustomSelectValue(input, value, fallbackLabel = value) {
  const root = input.closest("[data-custom-select]");
  if (!root) return;
  const options = Array.from(root.querySelectorAll(".custom-select-option"));
  let selected = options.find((option) => option.dataset.value === String(value));
  if (!selected) {
    selected = document.createElement("button");
    selected.type = "button";
    selected.className = "custom-select-option";
    selected.setAttribute("role", "option");
    selected.dataset.value = String(value);
    selected.textContent = fallbackLabel;
    root.querySelector(".custom-select-menu").appendChild(selected);
    bindCustomSelectOption(root, selected);
  }
  input.value = String(value);
  root.querySelector("[data-select-label]").textContent = selected.textContent;
  options.concat(selected).forEach((option) => {
    const active = option === selected;
    option.classList.toggle("selected", active);
    option.setAttribute("aria-selected", String(active));
  });
}

function closeCustomSelect(root) {
  root.querySelector(".custom-select-menu").classList.add("hidden");
  root.querySelector(".custom-select-trigger").setAttribute("aria-expanded", "false");
  root.classList.remove("open");
}

function bindCustomSelectOption(root, option) {
  option.addEventListener("click", () => {
    const input = root.querySelector("input[type='hidden']");
    const changed = input.value !== option.dataset.value;
    setCustomSelectValue(input, option.dataset.value, option.textContent);
    closeCustomSelect(root);
    if (changed) input.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

function initializeCustomSelects() {
  document.querySelectorAll("[data-custom-select]").forEach((root) => {
    const trigger = root.querySelector(".custom-select-trigger");
    const menu = root.querySelector(".custom-select-menu");
    trigger.addEventListener("click", () => {
      const opening = menu.classList.contains("hidden");
      document.querySelectorAll("[data-custom-select].open").forEach(closeCustomSelect);
      if (opening) {
        menu.classList.remove("hidden");
        trigger.setAttribute("aria-expanded", "true");
        root.classList.add("open");
      }
    });
    root.querySelectorAll(".custom-select-option").forEach((option) => {
      bindCustomSelectOption(root, option);
    });
  });
  document.addEventListener("click", (event) => {
    document.querySelectorAll("[data-custom-select].open").forEach((root) => {
      if (!root.contains(event.target)) closeCustomSelect(root);
    });
  });
}

function toggleTypeFields() {
  const type = $("add-type").value;
  document.querySelectorAll(".type-fields").forEach((fields) => {
    const active = fields.id === `${type.replace("_", "-")}-fields`;
    fields.classList.toggle("hidden", !active);
    fields.querySelectorAll("input, textarea, button").forEach((control) => {
      control.disabled = !active;
    });
  });
}

function fmtResetAt(iso) {
  if (!iso) return "";
  try {
    const d = new Date(iso);
    const now = new Date();
    const sameYear = d.getFullYear() === now.getFullYear();
    const date = new Intl.DateTimeFormat("zh-CN", {
      ...(sameYear ? {} : { year: "numeric" }),
      month: "numeric",
      day: "numeric",
    }).format(d);
    const weekday = new Intl.DateTimeFormat("zh-CN", { weekday: "short" }).format(d);
    const time = new Intl.DateTimeFormat("zh-CN", {
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    }).format(d);
    return `${date} ${weekday} ${time}`;
  } catch {
    return iso;
  }
}

function fmtResetRelative(iso) {
  if (!iso) return "";
  const target = new Date(iso).getTime();
  if (!Number.isFinite(target)) return "";
  const seconds = Math.max(0, Math.round((target - Date.now()) / 1000));
  if (seconds < 30) return "即将重置";
  if (seconds < 60) return `还剩 ${seconds} 秒`;
  if (seconds < 3600) return `还剩 ${Math.ceil(seconds / 60)} 分钟`;
  if (seconds < 86400) {
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.ceil((seconds % 3600) / 60);
    return `还剩 ${hours} 小时${minutes ? ` ${minutes} 分钟` : ""}`;
  }
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  return `还剩 ${days} 天${hours ? ` ${hours} 小时` : ""}`;
}

function fmtAgo(iso) {
  if (!iso) return "";
  try {
    const d = new Date(iso);
    const diff = Math.max(0, (Date.now() - d.getTime()) / 1000);
    if (diff < 60) return "刚刚";
    if (diff < 3600) return `${Math.floor(diff / 60)}分钟前`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}小时前`;
    return `${Math.floor(diff / 86400)}天前`;
  } catch {
    return "";
  }
}

function updateRelativeTimes() {
  document.querySelectorAll("[data-relative-time]").forEach((element) => {
    const suffix = element.id === "global-updated-at" ? "更新" : "";
    element.textContent = `${fmtAgo(element.dataset.relativeTime)}${suffix}`;
  });
}

function updateGlobalRefreshTime(snapshots) {
  const timestamps = (snapshots || [])
    .map((snapshot) => snapshot.usage?.fetched_at)
    .filter(Boolean)
    .map((value) => new Date(value).getTime())
    .filter(Number.isFinite);
  const element = $("global-updated-at");
  if (!element || timestamps.length === 0) {
    element?.classList.add("hidden");
    return;
  }
  const latest = new Date(Math.max(...timestamps)).toISOString();
  element.dataset.relativeTime = latest;
  element.textContent = `${fmtAgo(latest)}更新`;
  element.classList.remove("hidden");
}

function resizeWindowToContent() {
  if (resizeFrame) cancelAnimationFrame(resizeFrame);
  resizeFrame = requestAnimationFrame(async () => {
    resizeFrame = null;
    const activeView = currentView === "settings" ? views.settings : views.main;
    if (!activeView || activeView.classList.contains("hidden")) return;
    const headerHeight = activeView.querySelector(".popup-header")?.offsetHeight || 0;
    const content = currentView === "settings" ? activeView.querySelector(".settings-body") : $("providers");
    const footer = currentView === "settings"
      ? activeView.querySelector(":scope > .status-bar")
      : activeView.querySelector(".popup-footer");
    const contentHeight = naturalChildrenHeight(content);
    const desiredHeight = headerHeight + contentHeight + (footer?.offsetHeight || 0) + 13;
    try {
      const actualHeight = await invoke("resize_window_to_content", { contentHeight: desiredHeight });
      if (currentView === "main") {
        $("providers").classList.toggle("is-overflowing", desiredHeight > actualHeight + 1);
      } else {
        activeView.querySelector(".settings-body")?.classList.toggle("is-overflowing", desiredHeight > actualHeight + 1);
      }
    } catch (err) {
      console.warn("resize window failed", err);
    }
  });
}

function naturalChildrenHeight(container) {
  if (!container) return 0;
  const style = getComputedStyle(container);
  let height = (Number.parseFloat(style.paddingTop) || 0) + (Number.parseFloat(style.paddingBottom) || 0);
  Array.from(container.children).forEach((child) => {
    const childStyle = getComputedStyle(child);
    if (childStyle.display === "none") return;
    height += child.getBoundingClientRect().height;
    height += (Number.parseFloat(childStyle.marginTop) || 0) + (Number.parseFloat(childStyle.marginBottom) || 0);
  });
  return height;
}

// ==================== rendering ====================

function renderProviders(snapshots) {
  lastSnapshot = snapshots;
  const container = $("providers");
  if (!snapshots || snapshots.length === 0) {
    container.innerHTML = `
      <div class="empty-state">
        <p class="empty-hint">尚未配置任何订阅</p>
        <p class="empty-subhint">点「设置」添加</p>
      </div>
    `;
    setStatus(null, "");
    updateGlobalRefreshTime([]);
    resizeWindowToContent();
    return;
  }
  container.innerHTML = groupProviders(snapshots)
    .map((group) => renderProviderGroup(group))
    .join("");

  container.querySelectorAll('[data-action="toggle-provider"]').forEach((button) => {
    button.addEventListener("click", () => {
      const key = button.dataset.groupKey;
      if (expandedProviderGroups.has(key)) {
        expandedProviderGroups.delete(key);
      } else {
        expandedProviderGroups.add(key);
      }
      const scrollTop = container.scrollTop;
      renderProviders(lastSnapshot);
      container.scrollTop = scrollTop;
    });
  });

  // 绑定 manual provider 点击编辑
  container.querySelectorAll(".quota-row.clickable").forEach((row) => {
    row.addEventListener("click", () => {
      openEditModal(row.dataset.id);
    });
  });
  container.querySelectorAll('[data-action="consume-codex-reset"]').forEach((button) => {
    button.addEventListener("click", () => {
      const id = button.dataset.id;
      if (consumingCodexProviders.has(id)) return;
      const count = Number(button.dataset.count) || 0;
      openConfirm(`将立即消耗 1 次 Codex 完整重置（当前剩余 ${count} 次），此操作不可撤销。继续？`, async () => {
        consumingCodexProviders.add(id);
        renderProviders(lastSnapshot);
        try {
          await invoke("consume_cpa_codex_reset", { id });
        } catch (err) {
          consumingCodexProviders.delete(id);
          renderProviders(lastSnapshot);
          showToast(`重置失败: ${err}`, true);
          return;
        }
        consumingCodexProviders.delete(id);
        showToast("已使用一次完整重置");
        try {
          const snapshots = await invoke("list_providers");
          renderProviders(snapshots);
        } catch (err) {
          showToast(`重置已生效，但刷新失败: ${err}`, true);
        }
      }, {
        title: "确认使用完整重置",
        confirmText: "使用一次",
      });
    });
  });
  const okCount = snapshots.filter((s) => !s.error).length;
  const errCount = snapshots.length - okCount;
  setStatus(errCount > 0 ? "warn" : null, errCount > 0 ? `${okCount} 正常 · ${errCount} 失败` : "");
  updateGlobalRefreshTime(snapshots);
  resizeWindowToContent();
}

function groupProviders(snapshots) {
  const groups = [];
  const byKey = new Map();
  snapshots.forEach((provider) => {
    const displayName = providerGroupName(provider);
    const key = provider.kind === "cpa_keeper"
      ? `${provider.kind}:${displayName.toLowerCase()}`
      : `${provider.kind}:${provider.id}`;
    let group = byKey.get(key);
    if (!group) {
      group = {
        ...provider,
        group_key: key,
        display_name: displayName,
        entries: [],
      };
      byKey.set(key, group);
      groups.push(group);
    }
    group.entries.push(provider);
  });
  return groups;
}

function providerGroupName(provider) {
  if (!["cpa_direct", "cpa_keeper"].includes(provider.kind)) return provider.display_name;
  const name = provider.display_name
    .replace(/\s*\(\s*CPA\s*\)\s*/gi, " ")
    .replace(/\b(?:5h|weekly|week|7d)\b/gi, " ")
    .replace(/(?:5\s*小时|每周|周限额|周额度)/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return name || provider.display_name;
}

function renderProviderGroup(group) {
  let codexDetailsIncluded = false;
  const items = group.entries.flatMap(providerDisplayItems).filter((item) => {
    if (item.type !== "codex-details") return true;
    if (codexDetailsIncluded) return false;
    codexDetailsIncluded = true;
    return true;
  });
  const expandable = items.length > 1;
  const expanded = expandable && expandedProviderGroups.has(group.group_key);
  const summary = expandable ? providerSummaryItem(items, group.kind) : items[0];
  const visibleItems = expanded ? items : summary ? [summary] : [];
  const hiddenCount = Math.max(0, items.length - 1);
  const codexDetailsItem = items.find((item) => item.type === "codex-details");
  const resetCreditCount = codexDetailsItem?.usage?.reset_credits
    ? Math.max(0, Number(codexDetailsItem.usage.reset_credits.available_count) || 0)
    : null;
  const planLabel = formatCodexPlan(codexDetailsItem?.usage?.codex_account?.plan_type);
  const codexMeta = [
    planLabel,
    resetCreditCount == null ? "" : `${resetCreditCount} 次重置`,
  ].filter(Boolean).join(" · ");
  const otherHiddenCount = Math.max(0, hiddenCount - (codexDetailsItem ? 1 : 0));
  const summaryMeta = items.some((item) => item.type === "balance-details")
    ? "余额明细"
    : codexDetailsItem
      ? `${codexMeta || "账户明细"}${otherHiddenCount > 0 ? ` · 另有 ${otherHiddenCount} 项` : ""}`
      : `另有 ${hiddenCount} 项`;
  const titlebar = expandable
    ? `<button class="provider-titlebar provider-toggle" type="button" data-action="toggle-provider" data-group-key="${escape(group.group_key)}" aria-expanded="${expanded}" title="${expanded ? "收起明细" : "展开全部明细"}">
        <span class="provider-title-copy">
          <h2>${escape(group.display_name)}</h2>
          <span class="provider-summary-meta">${expanded ? "收起明细" : summaryMeta}</span>
        </span>
        <svg class="provider-toggle-chevron" viewBox="0 0 20 20" aria-hidden="true"><path d="m6 8 4 4 4-4" /></svg>
      </button>`
    : `<div class="provider-titlebar">
        <span class="provider-title-copy">
          <h2>${escape(group.display_name)}</h2>
        </span>
      </div>`;
  return `
    <section class="provider-group${expanded ? " expanded" : ""}" data-kind="${escape(group.kind)}">
      ${titlebar}
      <div class="quota-list">
        ${visibleItems.map(renderProviderItem).join("")}
      </div>
    </section>
  `;
}

function providerDisplayItems(p) {
  const clickable = p.kind === "manual";
  if (p.error || !p.usage) {
    return [{ type: "error", provider: p, clickable }];
  }
  if (p.usage.balance) {
    return [
      { type: "balance-summary", provider: p, usage: p.usage },
      { type: "balance-details", provider: p, usage: p.usage },
    ];
  }
  const windows = Array.isArray(p.usage.windows) && p.usage.windows.length > 0
    ? p.usage.windows.map((window) => ({
        ...window,
        fetched_at: p.usage.fetched_at,
      }))
    : [p.usage];
  const items = windows.map((usage) => ({ type: "quota", provider: p, usage, clickable }));
  if (p.kind === "cpa_direct" && (p.usage.reset_credits || p.usage.codex_account)) {
    items.push({
      type: "codex-details",
      provider: p,
      usage: p.usage,
    });
  }
  return items;
}

function providerSummaryItem(items, providerKind) {
  const error = items.find((item) => item.type === "error");
  if (error) return error;
  const quotas = items.filter((item) => item.type === "quota");
  if (quotas.length > 0) {
    if (providerKind === "cpa_direct") {
      const codexFiveHour = quotas.find((item) =>
        /^codex-(?:primary|secondary)-five-hour$/.test(item.usage.key || "")
      );
      if (codexFiveHour) return codexFiveHour;
    }
    return quotas.reduce((selected, item) => {
      const selectedRemaining = quotaRemainingPercent(selected.usage);
      const itemRemaining = quotaRemainingPercent(item.usage);
      return itemRemaining < selectedRemaining ? item : selected;
    });
  }
  const balance = items.find((item) => item.type === "balance-summary");
  if (balance) return balance;
  return items[0];
}

function quotaRemainingPercent(usage) {
  if (/(?:^|\s)[·-]?\s*无限(?:额度)?\s*$/u.test(usage.label || "")) return Number.POSITIVE_INFINITY;
  if (!(usage.total > 0)) return 100;
  return 100 - Math.max(0, Math.min(100, (usage.used / usage.total) * 100));
}

function renderProviderItem(item) {
  if (item.type === "error") {
    const p = item.provider;
    return `
      <div class="quota-row error${item.clickable ? " clickable" : ""}" data-id="${escape(p.id)}">
        <span class="quota-label">${escape(quotaLabel(p, p.usage))}</span>
        <div class="provider-error">${escape(p.error)}</div>
      </div>
    `;
  }
  if (item.type === "codex-details") {
    return renderCodexDetails(item.provider, item.usage);
  }
  if (item.type === "balance-summary") return renderBalanceSummary(item.provider, item.usage);
  if (item.type === "balance-details") return renderBalanceDetails(item.provider, item.usage);
  return renderQuotaRow(item.provider, item.usage, item.clickable);
}

function formatCodexPlan(planType) {
  const plan = (planType || "").toString().trim().toLowerCase();
  if (!plan) return "";
  if (plan === "pro") return "Pro";
  if (plan === "plus") return "Plus";
  if (plan === "team") return "Team";
  if (plan === "free") return "Free";
  return planType;
}

function renderCodexDetails(p, usage) {
  const resetCredits = usage.reset_credits;
  const account = usage.codex_account || {};
  const count = resetCredits ? Math.max(0, Number(resetCredits.available_count) || 0) : 0;
  const applicableCount = resetCredits?.applicable_available_count == null
    ? null
    : Math.max(0, Number(resetCredits.applicable_available_count) || 0);
  const credits = Array.isArray(resetCredits?.credits) ? resetCredits.credits : [];
  const rows = credits.map((credit, index) => {
    const expiresAt = credit.expires_at;
    return `
      <div class="reset-credit-item">
        <span>第 ${index + 1} 次重置</span>
        ${expiresAt
          ? `<time datetime="${escape(expiresAt)}">${escape(fmtResetAt(expiresAt))} 到期</time>`
          : "<time>到期时间未知</time>"}
      </div>
    `;
  });
  const missingCount = Math.max(0, count - credits.length);
  if (missingCount > 0) {
    rows.push(`
      <div class="reset-credit-item muted">
        <span>其余 ${missingCount} 次重置</span>
        <time>到期时间未返回</time>
      </div>
    `);
  }
  if (rows.length === 0) {
    rows.push('<div class="reset-credit-empty">暂无可用重置次数</div>');
  }
  const plan = formatCodexPlan(account.plan_type);
  const subscriptionUntil = account.subscription_active_until;
  return `
    <div class="reset-credit-details-row" data-id="${escape(p.id)}">
      <div class="reset-credit-details-head">
        <span class="quota-label">Codex 账户</span>
        ${plan ? `<span class="codex-plan-badge">${escape(plan)}</span>` : ""}
      </div>
      ${subscriptionUntil
        ? `<div class="codex-subscription-row"><span>订阅到期</span><time datetime="${escape(subscriptionUntil)}">${escape(fmtResetAt(subscriptionUntil))}</time></div>`
        : ""}
      ${resetCredits ? `
        <div class="reset-credit-summary-row">
          <span>剩余重置次数 <strong>${count}</strong></span>
          ${applicableCount == null || applicableCount === count
            ? ""
            : `<span>当前可使用 ${applicableCount}</span>`}
        </div>
        <div class="reset-credit-list">${rows.join("")}</div>
        <div class="reset-credit-actions">
          ${resetCredits.immediate_reset_purchase_eligible
            ? '<span class="reset-credit-purchase">支持购买额外重置</span>'
            : "<span></span>"}
          ${count > 0
            ? `<button class="reset-credit-action" type="button" data-action="consume-codex-reset" data-id="${escape(p.id)}" data-count="${count}"${consumingCodexProviders.has(p.id) ? " disabled" : ""}>${consumingCodexProviders.has(p.id) ? "正在重置…" : "使用一次完整重置"}</button>`
            : ""}
        </div>
      ` : '<div class="reset-credit-empty">未返回重置次数信息</div>'}
    </div>
  `;
}

function balanceDisplay(usage) {
  const balance = usage.balance;
  const symbol = balance.currency === "CNY" ? "¥" : balance.currency === "USD" ? "$" : "";
  const amount = (value) => `${symbol}${Number(value).toFixed(2)}`;
  const statusClass = balance.available ? "available" : "unavailable";
  const statusText = balance.available ? "API 可用" : "余额不足";
  return { balance, amount, statusClass, statusText };
}

function renderBalanceSummary(p, usage) {
  const { balance, amount, statusClass, statusText } = balanceDisplay(usage);
  return `
    <div class="balance-summary-row provider-summary-card ${statusClass}" data-id="${escape(p.id)}">
      <div class="balance-head">
        <span class="quota-label">${escape(balance.currency)} 账户余额</span>
        <span class="balance-status">${statusText}</span>
      </div>
      <div class="balance-summary-main">
        <strong class="balance-total">${escape(amount(balance.total))}</strong>
        <span>可用余额</span>
      </div>
    </div>
  `;
}

function renderBalanceDetails(p, usage) {
  const { balance, amount } = balanceDisplay(usage);
  return `
    <div class="balance-details-row" data-id="${escape(p.id)}">
      <span><small>充值余额</small><strong>${escape(amount(balance.topped_up))}</strong></span>
      <span><small>赠送余额</small><strong>${escape(amount(balance.granted))}</strong></span>
    </div>
  `;
}

function renderQuotaRow(p, u, clickable) {
  const unlimited = /(?:^|\s)[·-]?\s*无限(?:额度)?\s*$/u.test(u.label || "");
  const usedPct = u.total > 0 ? Math.max(0, Math.min(100, (u.used / u.total) * 100)) : 0;
  const remainingPct = unlimited ? 100 : 100 - usedPct;
  const level = remainingPct <= 25
    ? "quota-low"
    : remainingPct <= 50
      ? "quota-medium"
      : remainingPct <= 75
        ? "quota-good"
        : "quota-high";
  const remainingValue = Math.max(0, u.total - u.used);
  const absolute = !(u.unit === "%" && u.total === 100)
    ? `剩余 ${fmtNum(remainingValue)} / ${fmtNum(u.total)} ${escape(u.unit || "")}`
    : "";
  const reset = u.reset_at
    ? fmtResetRelative(u.reset_at)
    : absolute || (u.fetched_at ? `更新于 ${fmtAgo(u.fetched_at)}` : "");
  const resetExact = u.reset_at ? `${fmtResetAt(u.reset_at)} 重置` : "";
  const content = `
      <div class="quota-head">
        <span class="quota-label">${escape(quotaLabel(p, u))}</span>
        <span class="provider-percent${unlimited ? " unlimited" : ""}">
          <strong>${unlimited ? "∞" : remainingPct.toFixed(0)}</strong>
          ${unlimited ? "" : '<small>% 剩余</small>'}
        </span>
      </div>
      <div class="provider-progress" role="meter" aria-label="${escape(quotaLabel(p, u))}剩余量" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${remainingPct.toFixed(0)}">
        <div class="provider-progress-fill" style="width: ${remainingPct}%"></div>
      </div>
      <div class="quota-reset" title="${escape(u.reset_at || u.fetched_at || "")}">
        <span class="quota-reset-relative">${escape(reset)}</span>
        ${resetExact
          ? `<time class="quota-reset-exact" datetime="${escape(u.reset_at)}">${escape(resetExact)}</time>`
          : clickable ? '<span class="provider-edit-hint">点击更新</span>' : ""}
      </div>
  `;
  const rowClass = `quota-row provider-summary-card ${level}${clickable ? " clickable" : ""}`;
  return clickable
    ? `<button type="button" class="${rowClass}" data-id="${escape(p.id)}">${content}</button>`
    : `<div class="${rowClass}" data-id="${escape(p.id)}">${content}</div>`;
}

function quotaLabel(p, usage) {
  if (usage?.label) return usage.label.replace(/\s*[·-]\s*无限(?:额度)?\s*$/u, "");
  if (p.kind === "minimax") return "限额";
  if (["cpa_direct", "cpa_keeper"].includes(p.kind)) {
    const text = `${p.id} ${p.display_name}`.toLowerCase();
    if (/weekly|week|7d|周/.test(text)) return "7 天";
    if (/5h|五小时|5小时/.test(text)) return "5 小时";
    return "额度";
  }
  if (p.kind === "manual") return "手动用量";
  return "用量";
}

function renderSettingsProviders(snapshots) {
  const container = $("settings-providers");
  if (!snapshots || snapshots.length === 0) {
    container.innerHTML = `<p class="empty-subhint" style="padding: 12px 0;">尚未添加任何 Provider</p>`;
    return;
  }
  container.innerHTML = snapshots
    .map(
      (p) => `
    <div class="settings-provider-row" data-id="${escape(p.id)}">
      <button class="settings-provider-edit" type="button" data-action="edit">
        <span class="sp-name">${escape(p.display_name)}</span>
        <span class="sp-kind">${escape(p.kind)}</span>
      </button>
      <button class="icon-btn-danger" type="button" data-action="delete" title="删除" aria-label="删除 ${escape(p.display_name)}">×</button>
    </div>
  `
    )
    .join("");
  container.querySelectorAll('[data-action="edit"]').forEach((button) => {
    button.addEventListener("click", (event) => {
      const row = event.target.closest(".settings-provider-row");
      beginProviderEdit(row.dataset.id);
    });
  });
  container.querySelectorAll('[data-action="delete"]').forEach((btn) => {
    btn.addEventListener("click", async (e) => {
      const row = e.target.closest(".settings-provider-row");
      const id = row.dataset.id;
      const name = row.querySelector(".sp-name").textContent;
      openConfirm(`删除「${name}」？`, async () => {
        try {
          await invoke("remove_provider", { id });
          if (editingProviderId === id) resetProviderForm();
          showToast("已删除");
          await refreshAll();
          await loadSettings();
        } catch (err) {
          showToast(`删除失败: ${err}`, true);
        }
      });
    });
  });
}

function configuredProvider(id) {
  const cfg = window.__usagebarConfig;
  return cfg?.providers?.find((provider) => provider.id === id) || null;
}

function resetCpaAccountDiscovery() {
  cpaDiscoveryGeneration += 1;
  discoveredCpaAccounts = [];
  const results = $("cpa-account-results");
  results.innerHTML = "";
  results.classList.add("hidden");
  const button = $("cpa-discover-accounts");
  if (button) {
    button.disabled = $("add-type").value !== "cpa_direct";
    button.textContent = "读取 Codex 账号";
  }
}

function selectDiscoveredCpaAccount(index) {
  const account = discoveredCpaAccounts[index];
  if (!account || account.disabled) return;
  const fields = $("cpa-direct-fields");
  fields.querySelector('[name="auth_index"]').value = account.auth_index || "";
  fields.querySelector('[name="account_id"]').value = account.account_id || "";
  document.querySelectorAll(".cpa-account-option").forEach((button, buttonIndex) => {
    button.classList.toggle("selected", buttonIndex === index);
  });
  showToast(`已选择 ${account.display_name}`);
}

function renderDiscoveredCpaAccounts(accounts) {
  discoveredCpaAccounts = accounts;
  const results = $("cpa-account-results");
  if (accounts.length === 0) {
    results.innerHTML = '<div class="reset-credit-empty">没有发现可用的 Codex OAuth 账号</div>';
    results.classList.remove("hidden");
    resizeWindowToContent();
    return;
  }
  results.innerHTML = accounts.map((account, index) => {
    const plan = formatCodexPlan(account.plan_type);
    const authIndex = (account.auth_index || "").toString();
    const shortIndex = authIndex.length > 12
      ? `${authIndex.slice(0, 6)}…${authIndex.slice(-4)}`
      : authIndex;
    return `
      <button class="cpa-account-option" type="button" data-account-index="${index}"${account.disabled ? " disabled" : ""}>
        <span class="cpa-account-option-copy">
          <strong>${escape(account.display_name || "Codex 账号")}${account.disabled ? "（已停用）" : ""}</strong>
          <small>${escape(shortIndex)}</small>
        </span>
        ${plan ? `<span class="cpa-account-option-plan">${escape(plan)}</span>` : ""}
      </button>
    `;
  }).join("");
  results.classList.remove("hidden");
  results.querySelectorAll(".cpa-account-option").forEach((button) => {
    button.addEventListener("click", () => selectDiscoveredCpaAccount(Number(button.dataset.accountIndex)));
  });
  const available = accounts
    .map((account, index) => ({ account, index }))
    .filter(({ account }) => !account.disabled);
  if (available.length === 1) selectDiscoveredCpaAccount(available[0].index);
  resizeWindowToContent();
}

function beginProviderEdit(id) {
  const provider = configuredProvider(id);
  if (!provider) {
    showToast("无法读取该 Provider 配置", true);
    return;
  }
  editingProviderId = id;
  resetCpaAccountDiscovery();
  openProviderEditor();
  $("provider-form-title").textContent = `编辑 ${provider.display_name}`;
  $("provider-submit").textContent = "保存";
  const form = $("add-form");
  form.elements.display_name.value = provider.display_name;
  setCustomSelectValue($("add-type"), provider.type, provider.type);
  $("add-type").closest("[data-custom-select]").querySelector(".custom-select-trigger").disabled = true;
  toggleTypeFields();
  fillProviderFields(form, provider);
  $("provider-editor").scrollIntoView({ behavior: "smooth", block: "start" });
}

function openProviderEditor() {
  $("provider-editor").classList.remove("hidden");
  $("provider-add-open").classList.add("hidden");
  resizeWindowToContent();
}

function closeProviderEditor() {
  $("provider-editor").classList.add("hidden");
  $("provider-add-open").classList.remove("hidden");
  views.settings.querySelector(".settings-body").scrollTo({ top: 0, behavior: "smooth" });
  resizeWindowToContent();
}

function fillProviderFields(form, provider) {
  const fields = {
    total: provider.total,
    api_key: provider.api_key,
    endpoint: provider.endpoint,
    path: provider.path === "/quota/cache" ? "/api/v1/quota/cache" : provider.path,
    auth_index: provider.auth_index,
    row_key: provider.row_key,
    account_id: provider.account_id,
    quota_window: provider.quota_window,
    currency: provider.currency,
    json_used: provider.json_used,
    json_total: provider.json_total,
  };
  Object.entries(fields).forEach(([name, value]) => {
    if (value == null) return;
    const controls = Array.from(form.elements).filter((control) => control.name === name && !control.disabled);
    controls.forEach((control) => {
      if (control.type === "hidden" && control.closest("[data-custom-select]")) {
        setCustomSelectValue(control, value, String(value));
      } else {
        control.value = value;
      }
    });
  });
}

function resetProviderForm() {
  editingProviderId = null;
  const form = $("add-form");
  form.reset();
  resetCpaAccountDiscovery();
  setCustomSelectValue($("add-type"), "minimax", "MiniMax（订阅 Key）");
  const rowKey = form.querySelector('[name="row_key"]');
  if (rowKey) setCustomSelectValue(rowKey, "rate_limit.primary_window", "Codex Primary Window");
  const quotaWindow = form.querySelector('[name="quota_window"]');
  if (quotaWindow) setCustomSelectValue(quotaWindow, "auto", "自动（显示最紧张窗口）");
  const currency = form.querySelector('[name="currency"]');
  if (currency) setCustomSelectValue(currency, "CNY", "CNY（人民币）");
  $("provider-form-title").textContent = "添加 Provider";
  $("provider-submit").textContent = "添加";
  $("add-type").closest("[data-custom-select]").querySelector(".custom-select-trigger").disabled = false;
  toggleTypeFields();
  closeProviderEditor();
}

function openConfirm(message, action, options = {}) {
  const { title = "确认删除", confirmText = "删除" } = options;
  $("confirm-title").textContent = title;
  $("confirm-message").textContent = message;
  $("confirm-ok").textContent = confirmText;
  confirmAction = action;
  confirmModal.classList.remove("hidden");
  setTimeout(() => $("confirm-cancel").focus(), 20);
}

function closeConfirm() {
  confirmModal.classList.add("hidden");
  confirmAction = null;
}

function fmtNum(n) {
  if (typeof n !== "number") return String(n);
  if (Number.isInteger(n)) return n.toString();
  return n.toFixed(2);
}

function escape(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// ==================== data flow ====================

async function refresh() {
  const button = $("refresh-btn");
  if (button.disabled) return;
  button.disabled = true;
  document.querySelectorAll(".refresh-control").forEach((control) => {
    control.classList.add("is-refreshing");
    control.disabled = true;
  });
  try {
    // 后端触发 fetch（写入缓存），前端从缓存读
    await invoke("refresh_now");
    const snapshots = await invoke("list_providers");
    renderProviders(snapshots);
  } catch (err) {
    setStatus("err", `刷新失败: ${err}`);
  } finally {
    document.querySelectorAll(".refresh-control").forEach((control) => {
      control.classList.remove("is-refreshing");
      control.disabled = false;
    });
  }
}

async function loadSettings() {
  try {
    const cfg = await invoke("get_config");
    window.__usagebarConfig = cfg;
    const sel = $("refresh-interval");
    setCustomSelectValue(sel, cfg.refresh_interval_secs, `${cfg.refresh_interval_secs} 秒`);
    renderSettingsProviders(lastSnapshot);
    resizeWindowToContent();
  } catch (err) {
    showToast(`加载设置失败: ${err}`, true);
  }
}

async function refreshAll() {
  await refresh();
  if (currentView === "settings") await loadSettings();
}

// ==================== modal ====================

function openEditModal(id) {
  const snap = lastSnapshot.find((s) => s.id === id);
  if (!snap) return;
  currentEditId = id;
  $("edit-title").textContent = `更新「${snap.display_name}」`;
  $("edit-used").value = snap.usage ? snap.usage.used : 0;
  modal.classList.remove("hidden");
  setTimeout(() => $("edit-used").focus(), 50);
}

function closeEditModal() {
  modal.classList.add("hidden");
  currentEditId = null;
}

// ==================== navigation ====================

function switchView(name) {
  currentView = name;
  views.main.classList.toggle("hidden", name !== "main");
  views.settings.classList.toggle("hidden", name !== "settings");
  if (name === "settings") loadSettings();
  resizeWindowToContent();
}

// ==================== event wiring ====================

window.addEventListener("DOMContentLoaded", () => {
  initializeCustomSelects();
  setInterval(updateRelativeTimes, 30_000);
  // main view
  $("refresh-btn").addEventListener("click", refresh);
  $("quit-btn").addEventListener("click", () => invoke("quit_app"));
  $("settings-btn").addEventListener("click", () => {
    resetProviderForm();
    switchView("settings");
  });

  // settings view
  $("back-btn").addEventListener("click", () => switchView("main"));
  $("provider-add-open").addEventListener("click", () => {
    resetProviderForm();
    openProviderEditor();
    $("provider-editor").scrollIntoView({ behavior: "smooth", block: "start" });
  });
  $("provider-edit-cancel").addEventListener("click", resetProviderForm);
  $("cpa-direct-fields").querySelectorAll('[name="endpoint"], [name="api_key"]').forEach((input) => {
    input.addEventListener("input", () => {
      resetCpaAccountDiscovery();
    });
  });
  $("cpa-discover-accounts").addEventListener("click", async () => {
    const fields = $("cpa-direct-fields");
    const endpoint = (fields.querySelector('[name="endpoint"]').value || "").trim();
    const apiKey = (fields.querySelector('[name="api_key"]').value || "").trim();
    if (!endpoint || !apiKey) {
      showToast("请先填写 CLIProxyAPI 地址和管理密钥", true);
      return;
    }
    const button = $("cpa-discover-accounts");
    resetCpaAccountDiscovery();
    const requestGeneration = cpaDiscoveryGeneration;
    button.disabled = true;
    button.textContent = "读取中…";
    try {
      const accounts = await invoke("discover_cpa_codex_accounts", { endpoint, apiKey });
      const currentEndpoint = (fields.querySelector('[name="endpoint"]').value || "").trim();
      const currentApiKey = (fields.querySelector('[name="api_key"]').value || "").trim();
      if (
        requestGeneration !== cpaDiscoveryGeneration ||
        endpoint !== currentEndpoint ||
        apiKey !== currentApiKey
      ) return;
      renderDiscoveredCpaAccounts(accounts);
      if (accounts.length > 0) showToast(`发现 ${accounts.length} 个 Codex 账号`);
    } catch (err) {
      if (requestGeneration !== cpaDiscoveryGeneration) return;
      resetCpaAccountDiscovery();
      showToast(`读取账号失败: ${err}`, true);
    } finally {
      if (requestGeneration === cpaDiscoveryGeneration) {
        button.disabled = $("add-type").value !== "cpa_direct";
        button.textContent = "读取 Codex 账号";
      }
    }
  });
  $("add-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const fd = new FormData(e.target);
    const type = fd.get("type");
    const name = (fd.get("display_name") || "").toString().trim();
    if (!name) {
      showToast("请填写名称", true);
      return;
    }
    const id = editingProviderId ||
      name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") ||
      `provider-${Date.now()}`;
    // 按类型自动用默认 icon/color/unit
    const defaults = {
      manual:    { icon: "○", color: "#8e8e93", unit: "credits" },
      minimax:   { icon: "M", color: "#5e5ce6", unit: "%" },
      cpa_direct:{ icon: "C", color: "#30d158", unit: "%" },
      deepseek:  { icon: "D", color: "#4389e8", unit: "CNY" },
      cpa_keeper:{ icon: "C", color: "#30d158", unit: "%" },
      http:      { icon: "H", color: "#0a84ff", unit: "" },
    };
    const d = defaults[type] || defaults.manual;
    const existing = editingProviderId ? configuredProvider(editingProviderId) : null;
    const base = {
      ...(existing || {}),
      id,
      type,
      display_name: name,
      icon: d.icon,
      color: d.color,
      unit: d.unit,
      enabled: true,
    };

    let provider;
    if (type === "manual") {
      const total = Number(fd.get("total"));
      if (!Number.isFinite(total) || total <= 0) {
        showToast("请填写有效的总额度", true);
        return;
      }
      provider = { ...base, total, used: existing?.used ?? 0 };
    } else if (type === "http") {
      const endpoint = (fd.get("endpoint") || "").toString().trim();
      if (!endpoint) {
        showToast("请填写 API 地址", true);
        return;
      }
      provider = {
        ...base,
        endpoint,
        api_key: (fd.get("api_key") || "").toString() || null,
        json_used: (fd.get("json_used") || "used").toString(),
        json_total: (fd.get("json_total") || "total").toString(),
        timeout_secs: 15,
      };
    } else if (type === "minimax") {
      const key = (fd.get("api_key") || "").toString().trim();
      if (!key) {
        showToast("请填写订阅 Key", true);
        return;
      }
      provider = {
        ...base,
        api_key: key,
        endpoint: (fd.get("endpoint") || "").toString().trim(),
      };
    } else if (type === "cpa_direct") {
      const endpoint = (fd.get("endpoint") || "").toString().trim();
      const managementKey = (fd.get("api_key") || "").toString().trim();
      const authIndex = (fd.get("auth_index") || "").toString().trim();
      if (!endpoint) {
        showToast("请填写 CLIProxyAPI 地址", true);
        return;
      }
      if (!managementKey) {
        showToast("请填写 CLIProxyAPI 管理密钥", true);
        return;
      }
      if (!authIndex) {
        showToast("请填写 Auth Index", true);
        return;
      }
      provider = {
        ...base,
        endpoint,
        api_key: managementKey,
        auth_index: authIndex,
        account_id: (fd.get("account_id") || "").toString().trim() || null,
        quota_window: (fd.get("quota_window") || "auto").toString(),
      };
    } else if (type === "deepseek") {
      const key = (fd.get("api_key") || "").toString().trim();
      const endpoint = (fd.get("endpoint") || "").toString().trim();
      if (!key) {
        showToast("请填写 DeepSeek API Key", true);
        return;
      }
      if (!endpoint) {
        showToast("请填写 DeepSeek API 地址", true);
        return;
      }
      const currency = (fd.get("currency") || "CNY").toString().toUpperCase();
      provider = {
        ...base,
        endpoint,
        api_key: key,
        currency,
        unit: currency,
      };
    } else if (type === "cpa_keeper") {
      const endpoint = (fd.get("endpoint") || "").toString().trim();
      const authIndex = (fd.get("auth_index") || "").toString().trim();
      if (!endpoint) {
        showToast("请填写 Keeper 地址", true);
        return;
      }
      if (!authIndex) {
        showToast("请填写 Auth Index", true);
        return;
      }
      provider = {
        ...base,
        endpoint,
        path: (fd.get("path") || "/api/v1/quota/cache").toString().trim(),
        api_key: (fd.get("api_key") || "").toString() || null,
        auth_index: authIndex,
        row_key: (fd.get("row_key") || "rate_limit.primary_window").toString(),
      };
    } else {
      showToast(`未知类型: ${type}`, true);
      return;
    }

    try {
      const command = editingProviderId ? "update_provider" : "add_provider";
      await invoke(command, { provider });
      showToast(editingProviderId ? "已保存" : "已添加");
      resetProviderForm();
      await refreshAll();
    } catch (err) {
      showToast(`${editingProviderId ? "保存" : "添加"}失败: ${err}`, true);
    }
  });
  $("refresh-interval").addEventListener("change", async (e) => {
    const secs = Number(e.target.value);
    if (!Number.isFinite(secs) || secs < 30) {
      showToast("刷新间隔需 ≥ 30 秒", true);
      return;
    }
    try {
      await invoke("update_config", { refreshIntervalSecs: secs });
      showToast(`已设为 ${secs < 60 ? secs + " 秒" : secs / 60 + " 分钟"}`);
    } catch (err) {
      showToast(`保存失败: ${err}`, true);
    }
  });

  // edit modal
  $("edit-cancel").addEventListener("click", closeEditModal);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) closeEditModal();
  });
  $("confirm-cancel").addEventListener("click", closeConfirm);
  $("confirm-ok").addEventListener("click", async () => {
    const action = confirmAction;
    closeConfirm();
    if (action) await action();
  });
  confirmModal.addEventListener("click", (event) => {
    if (event.target === confirmModal) closeConfirm();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !modal.classList.contains("hidden")) closeEditModal();
    if (e.key === "Escape" && !confirmModal.classList.contains("hidden")) closeConfirm();
    if (e.key === "Escape") {
      document.querySelectorAll("[data-custom-select].open").forEach(closeCustomSelect);
    }
  });
  $("edit-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!currentEditId) return;
    const used = Number($("edit-used").value);
    try {
      await invoke("update_manual_used", { id: currentEditId, used });
      showToast("已更新");
      closeEditModal();
      await refresh();
    } catch (err) {
      showToast(`更新失败: ${err}`, true);
    }
  });

  // tray menu refresh event
  window.addEventListener("usagebar-refresh", refresh);

  // 切换 Provider 表单字段
  $("add-type").addEventListener("change", toggleTypeFields);
  toggleTypeFields();

  // 后端 scheduler 推送的更新事件
  listen("usagebar-updated", async () => {
    try {
      const snapshots = await invoke("list_providers");
      renderProviders(snapshots);
    } catch (err) {
      console.error("auto refresh failed:", err);
    }
  });

  // initial load
  refresh();
});
