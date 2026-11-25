// 父页面持有 Cookie 和 CSRF 信息，只转发该 iframe 所属插件的 API 请求。
const CHANNEL = "nuono-plugin-view";
const METHODS = new Set(["GET", "POST", "PUT", "PATCH", "DELETE"]);
const THEME_TOKENS = ["--font", "--mono", "--bg-desktop", "--surface-solid", "--text-1", "--text-2", "--text-3", "--hairline", "--hairline-strong", "--accent", "--accent-ink", "--accent-control", "--accent-hover", "--accent-soft", "--accent-border", "--accent-focus"];
const randomNonce = () => Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, "0")).join("");

export function pluginEndpoint(plugin, endpoint) {
  if (typeof plugin !== "string" || !/^[A-Za-z0-9_-]+$/.test(plugin)) throw new Error("Invalid plugin identifier");
  if (typeof endpoint !== "string" || !endpoint || endpoint.length > 2048 || /[\\%?:#\x00-\x20]/.test(endpoint)) throw new Error("Invalid plugin endpoint");
  const parts = endpoint.split("/");
  if (parts.some(part => !part || part === "." || part === "..")) throw new Error("Invalid plugin endpoint");
  return `/api/plugin/${encodeURIComponent(plugin)}/${parts.map(encodeURIComponent).join("/")}`;
}

function csrfCookie() {
  return document.cookie.split(";").map(value => value.trim()).find(value => value.startsWith("csrf_token="))?.slice(11) || "";
}

export function attachPluginView(frame, plugin, initialContext) {
  let nonce = randomNonce();
  let instance = null;
  const source = new URL(frame.src, location.origin);
  const proof = source.origin === location.origin && source.pathname.match(/^\/api\/plugin-pages\/assets\/([a-f0-9]{64})\//)?.[1];
  if (!proof) throw new Error("Invalid plugin page asset URL");
  const baseContext = JSON.parse(initialContext);
  const requests = new Map();
  let closed = false;
  let ready = false;
  let port = null;
  const send = payload => {
    if (!closed && port) port.postMessage({ channel: CHANNEL, version: 1, nonce, instance, ...payload });
  };
  const context = () => {
    const root = document.documentElement;
    const styles = getComputedStyle(root);
    return { ...baseContext, plugin, locale: root.lang || baseContext.locale || "zh-CN", dark: root.dataset.theme === "dark", tokens: Object.fromEntries(THEME_TOKENS.map(key => [key, styles.getPropertyValue(key).trim()])) };
  };
  const sendContext = () => { if (ready) send({ type: "context", context: context() }); };
  const onRequest = async message => {
    if (closed) return;
    if (!message || message.channel !== CHANNEL || message.version !== 1) return;
    if (!ready || message.instance !== instance || message.nonce !== nonce) return;
    if (message.type === "navigate") {
      const path = message.path;
      if (typeof path === "string" && path.startsWith("/") && !path.startsWith("//") && !/[\\%\x00-\x20]/.test(path) && !path.startsWith("/api/")) {
        history.pushState(null, "", path);
        window.dispatchEvent(new PopStateEvent("popstate"));
      }
      return;
    }
    if (message.type === "open-external") {
      try {
        const url = new URL(message.url);
        if (["http:", "https:", "mailto:"].includes(url.protocol)) window.open(url.href, "_blank", "noopener,noreferrer");
      } catch {}
      return;
    }
    if (message.type !== "request" || typeof message.id !== "string" || message.id.length > 64) return;
    const id = message.id;
    const requestInstance = instance;
    const requestNonce = nonce;
    const reply = payload => { if (instance === requestInstance && nonce === requestNonce) send(payload); };
    if (requests.has(id)) return;
    let controller;
    let timeout;
    try {
      if (requests.size >= 32) throw new Error("Too many pending plugin requests");
      if (!METHODS.has(message.method)) throw new Error("Unsupported plugin request method");
      const url = new URL(pluginEndpoint(plugin, message.endpoint), location.origin);
      for (const [key, value] of Object.entries(message.query || {})) {
        if (!["string", "number", "boolean"].includes(typeof value)) throw new Error("Invalid plugin query");
        url.searchParams.set(key, String(value));
      }
      if (url.href.length > 8192) throw new Error("Plugin query is too large");
      const body = message.method === "GET" ? undefined : JSON.stringify(message.body ?? {});
      if (body && new TextEncoder().encode(body).length > 1024 * 1024) throw new Error("Plugin JSON request exceeds 1 MiB");
      controller = new AbortController();
      requests.set(id, controller);
      timeout = setTimeout(() => controller.abort(), 30000);
      const headers = { "Accept-Language": context().locale };
      if (body !== undefined) { headers["Content-Type"] = "application/json"; headers["X-CSRF-Token"] = csrfCookie(); }
      const response = await fetch(url, { method: message.method, body, headers, credentials: "same-origin", signal: controller.signal });
      const reader = response.body?.getReader();
      if (!reader) throw new Error("Empty plugin response");
      const decoder = new TextDecoder();
      let bytes = 0;
      let text = "";
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          bytes += value.byteLength;
          if (bytes > 4 * 1024 * 1024) { await reader.cancel(); throw new Error("Plugin response exceeds 4 MiB"); }
          text += decoder.decode(value, { stream: true });
        }
        text += decoder.decode();
      } finally { reader.releaseLock(); }
      const payload = JSON.parse(text);
      if (!response.ok || payload?.success === false) {
        const error = new Error(payload?.error || `HTTP ${response.status}`);
        if (response.status >= 400 && response.status <= 599) error.status = response.status;
        throw error;
      }
      reply({ type: "response", id, ok: true, data: payload?.success === true ? payload.data : payload });
    } catch (error) {
      reply({ type: "response", id, ok: false, status: error?.status, error: error?.name === "AbortError" ? "Plugin request cancelled or timed out" : String(error?.message || error) });
    } finally {
      clearTimeout(timeout);
      if (controller && requests.get(id) === controller) requests.delete(id);
    }
  };
  const announce = () => {
    if (!closed && frame.contentWindow) frame.contentWindow.postMessage({ channel: CHANNEL, version: 1, type: "host-ready" }, "*");
  };
  const onMessage = event => {
    if (closed || event.source !== frame.contentWindow || event.origin !== "null") return;
    const message = event.data;
    if (!message || message.channel !== CHANNEL || message.version !== 1 || message.proof !== proof || typeof message.instance !== "string" || !message.instance || message.instance.length > 64) return;
    if (message.type === "hello") { announce(); return; }
    if (message.type !== "ready" || !event.ports?.[0]) return;
    if (port) { port.onmessage = null; port.close(); }
    for (const controller of requests.values()) controller.abort();
    requests.clear();
    instance = message.instance;
    nonce = randomNonce();
    port = event.ports[0];
    port.onmessage = event => onRequest(event.data);
    port.start();
    ready = true;
    sendContext();
  };
  window.addEventListener("message", onMessage);
  frame.addEventListener("load", announce);
  announce();
  const observer = new MutationObserver(sendContext);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["lang", "data-theme", "style"] });
  return {
    destroy() {
      if (closed) return;
      closed = true;
      window.removeEventListener("message", onMessage);
      frame.removeEventListener("load", announce);
      observer.disconnect();
      if (port) { port.onmessage = null; port.close(); port = null; }
      for (const controller of requests.values()) controller.abort();
      requests.clear();
      frame.src = "about:blank";
    },
  };
}

export function destroyPluginView(handle) { handle?.destroy(); }
