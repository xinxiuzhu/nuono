// 仅负责主工作区的 DOM 交互（自定义滚动条、滚动位置恢复、二级导航键盘焦点）；路由、权限与业务状态由 Rust 管理。
export function attachWorkspace(main, rail) {
  const doc = main.ownerDocument, win = doc.defaultView;
  const root = doc.documentElement, thumb = rail.querySelector('.workspace-scrollbar-thumb');
  const navigations = [doc.querySelector('.module-navigation-strip'), doc.querySelector('.module-navigation-host'), doc.querySelector('.section-navigation-host')].filter(Boolean);
  const forced = win.matchMedia('(forced-colors: active)');
  const listeners = [];
  let frame = 0, disposed = false, drag = null, restoration = null, restoreTimer = 0;
  let range = 0, length = 0, travel = 0, selectedPluginKey = '';
  const listen = (node, type, fn, options) => { node.addEventListener(type, fn, options); listeners.push(() => node.removeEventListener(type, fn, options)); };
  function schedule() { if (!disposed && !frame) frame = win.requestAnimationFrame(update); }
  function cancelRestore() { restoration = null; win.clearTimeout(restoreTimer); restoreTimer = 0; }
  function release() {
    const pointer = drag?.id; drag = null;
    rail.removeAttribute('data-dragging');
    if (pointer !== undefined && rail.hasPointerCapture(pointer)) rail.releasePointerCapture(pointer);
  }
  function scroll(top) { cancelRestore(); main.scrollTo({ top: Math.max(0, Math.min(range, top)), behavior: 'instant' }); schedule(); }
  function revealSelectedPlugin() {
    const list = main.querySelector('.plugin-list');
    const selected = list?.querySelector('[aria-pressed="true"]');
    const key = selected ? `${selected.getAttribute('title')}/${selected.offsetLeft}/${list.clientWidth}` : '';
    if (selected && key !== selectedPluginKey) {
      const x = selected.offsetLeft - list.offsetLeft;
      if (x < list.scrollLeft) list.scrollLeft = x;
      else if (x + selected.offsetWidth > list.scrollLeft + list.clientWidth) list.scrollLeft = x + selected.offsetWidth - list.clientWidth;
    }
    selectedPluginKey = key;
  }
  function update() {
    frame = 0;
    if (disposed) return;
    revealSelectedPlugin();
    if (restoration) {
      let anchor = null;
      try { anchor = restoration.hash && doc.getElementById(decodeURIComponent(restoration.hash.replace(/^#/, ''))); } catch { /* URL 片段无效时保留正常的阅读位置。 */ }
      if (anchor && main.contains(anchor)) { anchor.scrollIntoView({ block: 'start', behavior: 'instant' }); cancelRestore(); }
      else if (!restoration.hash) {
        const target = restoration.top;
        main.scrollTo({ top: Math.min(target, Math.max(0, main.scrollHeight - main.clientHeight)), behavior: 'instant' });
        if (main.scrollHeight - main.clientHeight >= target - 1) cancelRestore();
      }
    }
    const enabled = !forced.matches;
    if (enabled) main.dataset.scrollbar = 'custom'; else delete main.dataset.scrollbar;
    range = Math.max(0, main.scrollHeight - main.clientHeight);
    rail.hidden = !enabled || range <= 1 || !main.getClientRects().length;
    if (rail.hidden) { release(); return; }
    length = Math.min(rail.clientHeight, Math.max(52, rail.clientHeight * main.clientHeight / main.scrollHeight));
    travel = Math.max(0, rail.clientHeight - length);
    const position = Math.max(0, Math.min(range, main.scrollTop));
    rail.style.setProperty('--layout-scroll-length', length.toFixed(2) + 'px');
    rail.style.setProperty('--layout-scroll-offset', (position / range * travel).toFixed(2) + 'px');
    rail.setAttribute('aria-valuemax', String(Math.round(range)));
    rail.setAttribute('aria-valuenow', String(Math.round(position)));
    rail.setAttribute('aria-valuetext', new Intl.NumberFormat(root.lang || 'en', { style: 'percent', maximumFractionDigits: 0 }).format(position / range));
  }
  listen(main, 'scroll', schedule, { passive: true });
  for (const type of ['wheel', 'pointerdown', 'keydown']) listen(main, type, cancelRestore, { passive: true });
  listen(rail, 'pointerdown', event => {
    if (event.button !== 0 || travel <= 0) return;
    event.preventDefault(); cancelRestore();
    const y = event.clientY - rail.getBoundingClientRect().top;
    const top = main.scrollTop / range * travel;
    const inside = y >= top && y <= top + length;
    drag = { id: event.pointerId, grab: inside ? y - top : length / 2 };
    rail.setAttribute('data-dragging', ''); rail.setPointerCapture(event.pointerId); rail.focus({ preventScroll: true });
    if (!inside) scroll((y - drag.grab) / travel * range);
  });
  listen(rail, 'pointermove', event => { if (drag?.id === event.pointerId && travel > 0) scroll((event.clientY - rail.getBoundingClientRect().top - drag.grab) / travel * range); });
  for (const type of ['pointerup', 'pointercancel', 'lostpointercapture']) listen(rail, type, release);
  listen(rail, 'keydown', event => {
    const page = Math.max(40, main.clientHeight * .9);
    const next = { ArrowDown: main.scrollTop + 40, ArrowUp: main.scrollTop - 40, PageDown: main.scrollTop + page, PageUp: main.scrollTop - page, Home: 0, End: range }[event.key];
    if (next !== undefined) { event.preventDefault(); scroll(next); }
  });
  listen(rail, 'wheel', event => { event.preventDefault(); scroll(main.scrollTop + event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? main.clientHeight : 1)); }, { passive: false });
  listen(doc, 'keydown', event => {
    const item = event.target.closest?.('.module-navigation a');
    if (!item || !['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
    const items = [...item.closest('.module-navigation').querySelectorAll('a')]
      .filter(link => link.getClientRects().length > 0 && !link.closest('[inert]'));
    const index = items.indexOf(item);
    if (index < 0) return;
    const forward = event.key === 'ArrowRight' || event.key === 'ArrowDown';
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : (index + (forward ? 1 : -1) + items.length) % items.length;
    event.preventDefault(); items[next]?.focus({ preventScroll: true }); items[next]?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  });
  listen(win, 'resize', schedule); listen(forced, 'change', schedule);
  const resize = new win.ResizeObserver(schedule); resize.observe(main);
  let observed = new Set();
  function observeContent() {
    const children = new Set(main.children);
    for (const child of observed) if (!children.has(child)) resize.unobserve(child);
    for (const child of children) if (!observed.has(child)) resize.observe(child);
    observed = children;
  }
  observeContent();
  const content = new win.MutationObserver(() => { observeContent(); schedule(); });
  content.observe(main, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ['hidden', 'class', 'aria-pressed', 'aria-current'] });
  for (const navigation of navigations) content.observe(navigation, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ['class', 'aria-pressed', 'aria-current'] });
  const theme = new win.MutationObserver(schedule); theme.observe(root, { attributes: true, attributeFilter: ['data-theme', 'lang'] });
  schedule();
  return {
    restore(top, hash) {
      cancelRestore(); release(); selectedPluginKey = '';
      restoration = { top, hash };
      restoreTimer = win.setTimeout(() => { cancelRestore(); schedule(); }, 8000);
      schedule();
    },
    get restoring() { return !!restoration; },
    destroy() {
      disposed = true; cancelRestore(); release(); if (frame) win.cancelAnimationFrame(frame);
      resize.disconnect(); content.disconnect(); theme.disconnect(); listeners.forEach(remove => remove());
      delete main.dataset.scrollbar; rail.hidden = true;
    },
  };
}
export function restoreWorkspace(handle, top, hash) { handle.restore(top, hash); }
export function workspaceRestoring(handle) { return handle.restoring; }
export function destroyWorkspace(handle) { handle.destroy(); }
