const params = new URLSearchParams(location.search);
const originalUrl = params.get("url") ?? "";
const host = (() => {
  try {
    return new URL(originalUrl).hostname.toLowerCase().replace(/\.$/, "");
  } catch {
    return "";
  }
})();

const $ = (id) => document.getElementById(id);
const port = chrome.runtime.connect({ name: "blocked-page" });
let nextId = 1;
const pending = new Map();
// Set once we navigate back to the site. State keeps arriving every second;
// calling location.replace() again would cancel the navigation in flight, so
// a slow site would never load.
let leaving = false;
// Group/habit cards are rebuilt only when their content changes, so buttons
// aren't replaced under the cursor every second.
let cardsKey = null;

function fmt(ms) {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor(s / 60) % 60;
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

function matches(domain) {
  return host === domain || host.endsWith("." + domain);
}

function request(req) {
  return new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    port.postMessage({ type: "request", id, request: req });
  });
}

async function run(req, button) {
  button.disabled = true;
  const response = await request(req);
  const notice = $("notice");
  notice.hidden = false;
  notice.textContent = response.ok ? (response.message ?? "Done") : response.error;
  button.disabled = false;
}

function button(label, onClick, primary = false) {
  const b = document.createElement("button");
  b.textContent = label;
  if (primary) b.className = "primary";
  b.addEventListener("click", () => onClick(b));
  return b;
}

function render(snapshot, connected) {
  if (leaving) return;
  $("offline").hidden = connected;
  if (!snapshot) return;

  const blocked = (snapshot.blocked_domains ?? []).some(matches);
  if (!blocked && originalUrl) {
    leaving = true;
    // The site can take a while to load; dots blinking in turn show the page
    // hasn't frozen.
    const dots = document.createElement("span");
    dots.className = "dots";
    dots.setAttribute("aria-hidden", "true");
    dots.append(...[0, 1, 2].map(() => Object.assign(document.createElement("span"), { textContent: "." })));
    $("headline").replaceChildren("Unlocking", dots);
    $("lead").textContent = host ? `Taking you back to ${host}.` : "Taking you back.";
    location.replace(originalUrl);
    return;
  }

  const groups = snapshot.groups.filter((g) => g.domains.some(matches));
  const groupNames = groups.map((g) => g.name).join(", ");
  $("headline").textContent = host ? `${host} is locked` : "Locked";
  $("lead").textContent = groupNames
    ? `Part of ${groupNames}. Finish a habit to earn time for it.`
    : "Finish a habit to earn time for it.";

  const s = snapshot.session;
  $("session").hidden = !s;
  if (s) {
    $("session-name").textContent = s.name;
    $("session-time").textContent = `${fmt(s.elapsed_ms)} / ${fmt(s.target_ms)}`;
    $("session-progress").style.width = `${Math.round(s.progress * 100)}%`;
    $("session-status").textContent = s.running
      ? "Running"
      : s.pause_reason === "idle"
        ? "Paused: you're idle"
        : "Paused: switch back to the habit window";
  }

  const penalty = snapshot.penalty_remaining_ms > 0;
  const groupIds = new Set(groups.map((g) => g.id));
  // Habits that earn time for these groups, or that they wait for.
  const required = new Set(groups.flatMap((g) => (g.requires ?? []).map((r) => r.habit)));
  const habits = snapshot.habits.filter(
    (h) => h.reward_groups.some((id) => groupIds.has(id)) || required.has(h.id),
  );
  const key = JSON.stringify([
    groups.map((g) => [g.id, g.credit_ms, g.unlock_mode, g.requires]),
    habits.map((h) => [h.id, h.count_today, h.done_today, Math.floor((h.today_focused_ms ?? 0) / 60000)]),
    !!s,
    penalty,
  ]);
  if (key === cardsKey && !penalty) return;
  cardsKey = key;

  const groupsEl = $("groups");
  groupsEl.replaceChildren();
  for (const g of groups) {
    const card = document.createElement("div");
    card.className = "card";
    const row = document.createElement("div");
    row.className = "row";
    const label = document.createElement("span");
    label.textContent = g.credit_ms > 0 ? `${g.name}: ${fmt(g.credit_ms)} earned` : `${g.name}: no credit yet`;
    row.append(label);
    // Older daemons don't send requirements; a missing field counts as met.
    const waiting = g.requirements_met === false;
    const usage = g.unlock_mode === "usage";
    const dayPass = g.unlock_mode === "rest_of_day";
    if (g.credit_ms > 0) {
      const actions = document.createElement("div");
      actions.className = "actions";
      const unlock = (text, durationMs, primary) => {
        const b = button(text, (el) => run({ cmd: "unlock", group: g.id, duration_ms: durationMs }, el), primary);
        b.disabled = penalty || waiting;
        actions.append(b);
      };
      const of = usage ? " of use" : "";
      if (dayPass) {
        unlock(`Day pass (${fmt(g.rest_of_day_price_ms ?? 0)})`, null, true);
        actions.lastChild.disabled ||= g.credit_ms < (g.rest_of_day_price_ms ?? 0);
      } else {
        if (g.credit_ms > 15 * 60000) unlock(`Unlock 15m${of}`, 15 * 60000, false);
        unlock(`Unlock ${fmt(g.credit_ms)}${of}`, null, true);
      }
      row.append(actions);
    }
    card.append(row);
    if (g.requires?.length) {
      const list = document.createElement("div");
      list.className = "requires";
      const title = document.createElement("p");
      title.className = "muted";
      title.textContent = g.require_all ? "Do all of these today first:" : "Do one of these today first:";
      list.append(title);
      for (const r of g.requires) {
        const item = document.createElement("div");
        item.className = "row requirement";
        const name = document.createElement("span");
        name.textContent = `${r.done ? "✓ " : ""}${r.name}`;
        const bar = document.createElement("div");
        bar.className = "bar";
        const fill = document.createElement("div");
        fill.style.width = `${Math.round((r.progress ?? 0) * 100)}%`;
        bar.append(fill);
        item.append(name, bar);
        list.append(item);
      }
      card.append(list);
    }
    if (penalty) {
      const p = document.createElement("p");
      p.className = "muted";
      p.textContent = `Unlocks refused for ${fmt(snapshot.penalty_remaining_ms)} after an emergency abort.`;
      card.append(p);
    }
    groupsEl.append(card);
  }

  const habitsEl = $("habits");
  habitsEl.replaceChildren();
  if (!s && habits.length) {
    const title = document.createElement("h2");
    title.textContent = "Earn time";
    habitsEl.append(title);
    for (const h of habits) {
      const card = document.createElement("div");
      card.className = "card row";
      const label = document.createElement("span");
      if (h.kind === "passive") {
        // Counts by itself while its windows are focused: nothing to start.
        const today = `${fmt(h.today_focused_ms ?? 0)} / ${fmt(h.target_ms)} today`;
        const reward = h.reward_groups.length ? ` for +${fmt(h.reward_ms)}` : "";
        label.textContent = `${h.name}: ${h.done_today ? "done" : today}${reward}`;
        const hint = document.createElement("span");
        hint.textContent = "counts by itself";
        card.append(label, hint);
      } else if (h.kind === "manual" || h.kind === "counter") {
        // Logging is self-reported, so a web page can't do it: the native
        // host doesn't forward `done`.
        const count = h.kind === "manual" ? (h.done_today ? "done" : "to do") : `${h.count_today} / ${h.goal * ((h.rounds_today ?? 0) + 1)} ${h.unit}`;
        label.textContent = `${h.name}: ${count}`;
        const hint = document.createElement("code");
        hint.textContent = `hf done ${h.id}`;
        card.append(label, hint);
      } else {
        label.textContent = `${h.name}: ${fmt(h.target_ms)} for +${fmt(h.reward_ms)}${h.strictness === "strict" ? " (strict)" : ""}`;
        card.append(label, button("Start", (el) => run({ cmd: "start", habit: h.id }, el)));
      }
      habitsEl.append(card);
    }
  }
}

port.onMessage.addListener((message) => {
  if (message.type === "state") {
    render(message.snapshot, message.connected);
  } else if (message.type === "response") {
    pending.get(message.id)?.(message.response);
    pending.delete(message.id);
  }
});
