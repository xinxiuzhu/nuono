// Nuono Plugin View protocol v1. This client contains no Dashboard credentials.
(function () {
  "use strict";
  const CHANNEL = "nuono-plugin-view";
  const scriptOrigin = new URL(document.currentScript.src).origin;
  const proof = location.pathname.match(/^\/api\/plugin-pages\/assets\/([a-f0-9]{64})\//)?.[1];
  const instance = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, "0")).join("");
  const pending = new Map();
  const listeners = new Set();
  let context = null;
  let nonce = null;
  let sequence = 0;
  let closed = false;
  let port = null;
  let connecting = false;
  let resolveReady;
  const initialized = new Promise(resolve => { resolveReady = resolve; });
  const send = payload => {
    if (!port) throw new Error("Plugin view is not connected");
    port.postMessage({ channel: CHANNEL, version: 1, nonce, instance, ...payload });
  };

  const receive = event => {
    if (closed) return;
    const message = event.data;
    if (!message || message.channel !== CHANNEL || message.version !== 1 || message.instance !== instance) return;
    if (message.type === "context") {
      if (typeof message.nonce !== "string" || (nonce && nonce !== message.nonce)) return;
      nonce = message.nonce;
      context = message.context;
      if (!context || typeof context !== "object") return;
      document.documentElement.dataset.theme = context.dark ? "dark" : "light";
      if (typeof context.locale === "string") document.documentElement.lang = context.locale;
      for (const [key, value] of Object.entries(context.tokens || {})) {
        if (/^--[a-z0-9-]+$/.test(key) && typeof value === "string" && value.length <= 256) document.documentElement.style.setProperty(key, value);
      }
      resolveReady(context);
      for (const listener of listeners) { try { listener(context); } catch (error) { console.warn("Plugin view context listener failed", error); } }
    } else if (message.type === "response" && message.nonce === nonce) {
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id);
      clearTimeout(request.timeout);
      if (message.ok) request.resolve(message.data);
      else {
        const error = new Error(message.error || "Plugin request failed");
        if (Number.isInteger(message.status) && message.status >= 400 && message.status <= 599) error.status = message.status;
        request.reject(error);
      }
    }
  };
  window.addEventListener("message", event => {
    if (closed || connecting || context || event.source !== window.parent || event.origin !== scriptOrigin) return;
    const message = event.data;
    if (!message || message.channel !== CHANNEL || message.version !== 1 || message.type !== "host-ready") return;
    connecting = true;
    const pair = new MessageChannel();
    port = pair.port1;
    port.onmessage = receive;
    port.start();
    window.parent.postMessage({ channel: CHANNEL, version: 1, type: "ready", instance, proof }, scriptOrigin, [pair.port2]);
  });

  async function ready() {
    if (closed) throw new Error("Plugin view is closed");
    if (context) return context;
    let timeout;
    try {
      return await Promise.race([initialized, new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error("Open this page inside the Nuono WebUI")), 15000); })]);
    } finally { clearTimeout(timeout); }
  }

  async function request(method, endpoint, body, query) {
    await ready();
    if (closed) throw new Error("Plugin view is closed");
    if (pending.size >= 32) throw new Error("Too many pending plugin requests");
    const id = String(++sequence);
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => { pending.delete(id); reject(new Error("Plugin request timed out")); }, 30000);
      pending.set(id, { resolve, reject, timeout });
      try { send({ type: "request", id, method, endpoint, body, query }); }
      catch (error) { pending.delete(id); clearTimeout(timeout); reject(error); }
    });
  }

  window.NuonoPluginView = Object.freeze({
    version: 1, ready,
    getContext: () => context,
    onContext(listener) { listeners.add(listener); if (context) listener(context); return () => listeners.delete(listener); },
    request,
    apiGet: (endpoint, query) => request("GET", endpoint, undefined, query),
    apiPost: (endpoint, body) => request("POST", endpoint, body),
    async navigate(path) { await ready(); send({ type: "navigate", path }); },
    async openExternal(url) { await ready(); send({ type: "open-external", url }); },
  });
  window.addEventListener("pagehide", () => {
    closed = true;
    for (const request of pending.values()) { clearTimeout(request.timeout); request.reject(new Error("Plugin view is closed")); }
    pending.clear();
    listeners.clear();
    port?.close();
  }, { once: true });
  window.parent.postMessage({ channel: CHANNEL, version: 1, type: "hello", instance, proof }, scriptOrigin);
})();
