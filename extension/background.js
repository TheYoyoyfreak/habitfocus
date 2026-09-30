// habitfocus browser extension.
//
// Talks to habitd through the native messaging host (`hf native-host`):
//   host -> extension: {type: "snapshot", snapshot} | {type: "disconnected"} | {type: "response", id, response}
//   extension -> host: {type: "tab", window, title, url} | {type: "window_closed", window} | {type: "request", id, request}
//
// Blocking fails closed: the last known blocked domains stay blocked while the
// daemon or host is unreachable.
//
// Snapshots arrive every second while a session or unlock is active. Only the
// cheap work (updating open blocked pages) happens per snapshot; tab scans and
// storage writes happen only when the set of blocked domains changes.

const HOST = "dev.habitfocus.host";
const BLOCKED_PAGE = chrome.runtime.getURL("blocked.html");
const RECONNECT_MS = 5000;
// A tab we just redirected is left alone this long, so overlapping events
// (onBeforeNavigate, onUpdated, a tab scan) can't pile up redirects.
const REDIRECT_COOLDOWN_MS = 2000;

let port = null;
let daemonConnected = false;
let snapshot = null;
let blocked = [];
let blockedKey = null;
const recentRedirects = new Map();
let nextRequestId = 1;
const pendingRequests = new Map();
const pages = new Set();

const ready = chrome.storage.local.get(["snapshot"]).then((stored) => {
  if (!snapshot && stored.snapshot) {
    setSnapshot(stored.snapshot, false);
  }
});

function blockedDomainFor(url) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
  const host = parsed.hostname.toLowerCase().replace(/\.$/, "");
  return blocked.find((d) => host === d || host.endsWith("." + d)) ?? null;
}

function blockTab(tabId, url) {
  const last = recentRedirects.get(tabId);
  if (last !== undefined && Date.now() - last < REDIRECT_COOLDOWN_MS) return;
  recentRedirects.set(tabId, Date.now());
  const target = `${BLOCKED_PAGE}?url=${encodeURIComponent(url)}`;
  chrome.tabs.update(tabId, { url: target }).catch(() => {});
}

async function enforceOpenTabs() {
  // Unloaded (discarded) tabs are skipped: updating them would load every one
  // of them at once. They are caught by onBeforeNavigate when they load.
  const tabs = await chrome.tabs.query({ discarded: false }).catch(() => []);
  for (const tab of tabs) {
    if (tab.url && blockedDomainFor(tab.url)) blockTab(tab.id, tab.url);
  }
}

function setSnapshot(next, persist) {
  snapshot = next;
  const nextBlocked = next.blocked_domains ?? [];
  const key = nextBlocked.join("\n");
  if (key !== blockedKey) {
    blockedKey = key;
    blocked = nextBlocked;
    if (persist) chrome.storage.local.set({ snapshot: next });
    enforceOpenTabs();
  }
  broadcast();
}

function broadcast() {
  if (pages.size === 0) return;
  const message = { type: "state", snapshot, connected: daemonConnected && port !== null };
  for (const page of pages) {
    try {
      page.postMessage(message);
    } catch {
      pages.delete(page);
    }
  }
}

function connect() {
  if (port) return;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    port = null;
    setTimeout(connect, RECONNECT_MS);
    return;
  }
  port.onMessage.addListener((message) => {
    switch (message.type) {
      case "snapshot":
        daemonConnected = true;
        setSnapshot(message.snapshot, true);
        break;
      case "disconnected":
        daemonConnected = false;
        broadcast();
        break;
      case "response": {
        const resolve = pendingRequests.get(message.id);
        pendingRequests.delete(message.id);
        resolve?.(message.response);
        break;
      }
    }
  });
  port.onDisconnect.addListener(() => {
    port = null;
    daemonConnected = false;
    for (const resolve of pendingRequests.values()) {
      resolve({ ok: false, error: "habitfocus native host is not running" });
    }
    pendingRequests.clear();
    broadcast();
    setTimeout(connect, RECONNECT_MS);
  });
  reportAllWindows();
}

function sendToHost(message) {
  if (!port) return false;
  try {
    port.postMessage(message);
    return true;
  } catch {
    return false;
  }
}

function daemonRequest(request) {
  return new Promise((resolve) => {
    const id = nextRequestId++;
    pendingRequests.set(id, resolve);
    if (!sendToHost({ type: "request", id, request })) {
      pendingRequests.delete(id);
      resolve({ ok: false, error: "habitfocus native host is not connected" });
    }
  });
}

// ---- tab reporting (for habits with `url` allow rules) --------------------

async function reportWindow(windowId) {
  const [tab] = await chrome.tabs.query({ active: true, windowId }).catch(() => []);
  if (!tab) return;
  sendToHost({ type: "tab", window: windowId, title: tab.title ?? "", url: tab.url ?? "" });
}

async function reportAllWindows() {
  const windows = await chrome.windows.getAll().catch(() => []);
  for (const w of windows) reportWindow(w.id);
}

chrome.tabs.onActivated.addListener(({ windowId }) => reportWindow(windowId));
chrome.windows.onFocusChanged.addListener((windowId) => {
  if (windowId !== chrome.windows.WINDOW_ID_NONE) reportWindow(windowId);
});
chrome.windows.onRemoved.addListener((windowId) => sendToHost({ type: "window_closed", window: windowId }));
chrome.tabs.onRemoved.addListener((tabId) => recentRedirects.delete(tabId));

// ---- blocking ---------------------------------------------------------------

chrome.webNavigation.onBeforeNavigate.addListener(async (details) => {
  if (details.frameId !== 0) return;
  await ready;
  if (blockedDomainFor(details.url)) blockTab(details.tabId, details.url);
});

chrome.tabs.onUpdated.addListener(async (tabId, change, tab) => {
  await ready;
  if (change.url && blockedDomainFor(change.url)) {
    blockTab(tabId, change.url);
    return;
  }
  if (tab.active && (change.url || change.title)) reportWindow(tab.windowId);
});

// ---- blocked page -----------------------------------------------------------

chrome.runtime.onConnect.addListener((page) => {
  if (page.name !== "blocked-page") return;
  pages.add(page);
  page.onDisconnect.addListener(() => pages.delete(page));
  page.onMessage.addListener(async (message) => {
    if (message.type === "request") {
      const response = await daemonRequest(message.request);
      // Apply the new state right away (e.g. after an unlock) instead of
      // waiting for the next streamed snapshot, so the page and the navigation
      // listener agree before the page navigates back to the site.
      if (response.ok && response.snapshot) {
        daemonConnected = true;
        setSnapshot(response.snapshot, true);
      }
      page.postMessage({ type: "response", id: message.id, response });
    }
  });
  ready.then(() =>
    page.postMessage({ type: "state", snapshot, connected: daemonConnected && port !== null }),
  );
});

connect();
