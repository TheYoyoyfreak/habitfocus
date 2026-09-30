// End-to-end checks for the browser extension, driven over WebDriver BiDi.
// Usage: node harness.mjs <bidi-port> <extension-dir> <hf-binary>

import { execFileSync } from "node:child_process";

const [port, extensionPath, hf] = process.argv.slice(2);
const SITE = "http://blocked.test:8765/page.html";

const ws = new WebSocket(`ws://127.0.0.1:${port}/session`);
let nextId = 1;
const pending = new Map();
const navigations = [];
ws.onmessage = (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.type === "error" ? reject(new Error(`${msg.error}: ${msg.message}`)) : resolve(msg.result);
  } else if (msg.method === "browsingContext.navigationStarted") {
    navigations.push(msg.params.url);
  }
};
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const hfRun = (...args) => execFileSync(hf, args, { encoding: "utf8" });

let failures = 0;
function check(name, ok, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures++;
}

async function waitForUrl(context, predicate, timeoutMs) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const url = await currentUrl(context).catch(() => "");
    if (predicate(url)) return Date.now() - start;
    await sleep(200);
  }
  return null;
}

async function currentUrl(context) {
  const result = await send("script.evaluate", {
    expression: "location.href",
    target: { context },
    awaitPromise: false,
  });
  return result.result.value;
}

const isBlockedPage = (url) => url.startsWith("moz-extension://") && url.includes("blocked.html");

await new Promise((r) => (ws.onopen = r));
await send("session.new", { capabilities: {} });
await send("session.subscribe", { events: ["browsingContext.navigationStarted"] });
await send("webExtension.install", { extensionData: { type: "path", path: extensionPath } });
const { context } = await send("browsingContext.create", { type: "tab" });
await sleep(2500); // extension connects to the native host

// 1. Visiting a blocked site shows the blocked page.
send("browsingContext.navigate", { context, url: SITE }).catch(() => {});
check("blocked site redirects to blocked page", (await waitForUrl(context, isBlockedPage, 5000)) !== null);

// 2. No navigation storm while a session streams snapshots every second.
hfRun("start", "long");
const before = navigations.length;
await sleep(6000);
check("no navigations while blocked page idles", navigations.length === before, `${navigations.length - before} navigations`);
hfRun("abort");

// 3. Unlocking waits for the required habit, which the page lists.
const gate = async () =>
  (
    await send("script.evaluate", {
      expression: `(async () => {
        for (let i = 0; i < 20; i++) {
          const b = [...document.querySelectorAll("button")].find((b) => b.textContent.startsWith("Unlock"));
          const listed = document.querySelector(".requirement")?.textContent ?? "";
          if (b) return JSON.stringify({ disabled: b.disabled, listed });
          await new Promise((r) => setTimeout(r, 250));
        }
        return "{}";
      })()`,
      target: { context },
      awaitPromise: true,
    })
  ).result.value;
const waiting = JSON.parse(await gate());
check("unlock waits for required habits", waiting.disabled === true && waiting.listed.includes("Walk"), JSON.stringify(waiting));
hfRun("done", "walk");
await sleep(1500);
const met = JSON.parse(await gate());
check("hf done enables unlock", met.disabled === false && met.listed.includes("✓ Walk"), JSON.stringify(met));

// 4. Unlocking from the page returns to a slow site.
const clicked = await send("script.evaluate", {
  expression: `(async () => {
    for (let i = 0; i < 20; i++) {
      const b = [...document.querySelectorAll("button")].find((b) => b.textContent.startsWith("Unlock"));
      if (b) { b.click(); return true; }
      await new Promise((r) => setTimeout(r, 250));
    }
    return false;
  })()`,
  target: { context },
  awaitPromise: true,
});
check("unlock button present", clicked.result.value === true);
const returned = await waitForUrl(context, (url) => url === SITE, 8000);
check("unlock returns to the slow site", returned !== null, returned === null ? "stuck" : `${returned} ms`);
await sleep(1000);
check("site stays loaded after unlock", (await currentUrl(context)) === SITE);

// 5. Relocking sends the open tab back to the blocked page.
hfRun("relock", "social");
const relocked = await waitForUrl(context, isBlockedPage, 5000);
check("relock blocks the open tab", relocked !== null, relocked === null ? "" : `${relocked} ms`);

ws.close();
process.exit(failures === 0 ? 0 : 1);
