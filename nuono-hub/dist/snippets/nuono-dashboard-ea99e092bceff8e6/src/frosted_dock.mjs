// 原生按钮负责导航；磨砂材质与选中态过渡由 CSS 绘制，无逐帧光学计算。
export function hitDockItem(rects, x, y) {
  if (!rects.length || !Number.isFinite(x) || !Number.isFinite(y)) return -1;
  const bounds = {
    left: Math.min(...rects.map(r => r.left)), right: Math.max(...rects.map(r => r.right)),
    top: Math.min(...rects.map(r => r.top)), bottom: Math.max(...rects.map(r => r.bottom)),
  };
  if (x < bounds.left || x > bounds.right || y < bounds.top || y > bounds.bottom) return -1;
  let best = -1, distance = Infinity;
  rects.forEach((rect, index) => {
    const next = Math.hypot(x - (rect.left + rect.right) / 2, y - (rect.top + rect.bottom) / 2);
    if (next < distance) { best = index; distance = next; }
  });
  return best;
}

export function createDockGesture() {
  let pointer = null;
  return {
    begin(id, x, y, primary = true, button = 0) {
      if (pointer || !primary || button !== 0) return false;
      pointer = {id, x, y, dragged: false}; return true;
    },
    move(id, x, y) {
      if (!pointer || pointer.id !== id) return false;
      if (Math.hypot(x - pointer.x, y - pointer.y) > 5) pointer.dragged = true;
      return pointer.dragged;
    },
    end(id, x, y, rects) {
      if (!pointer || pointer.id !== id) return null;
      const dragged = this.move(id, x, y);
      pointer = null;
      return {index: hitDockItem(rects, x, y), dragged};
    },
    cancel() { pointer = null; },
    get id() { return pointer?.id; },
    get dragged() { return pointer?.dragged || false; },
  };
}

export function attachDock(dock) {
  const doc = dock.ownerDocument, win = doc.defaultView;
  const area = dock.closest('.dock-island-area'), shell = dock.closest('.app-shell');
  const pod = dock.querySelector('.dock-island-selection');
  const reduced = win.matchMedia('(prefers-reduced-motion: reduce)');
  const gesture = createDockGesture(), listeners = [];
  let disposed = false, queued = false, committing = false;
  let blockedUntil = 0, pointerRects = null, dockRect = null, lastTarget = null, previewTarget = null;
  const buttons = () => [...dock.querySelectorAll('[data-dock-select]')].filter(button => !button.disabled && button.getClientRects().length);
  const active = () => buttons().find(button => button.classList.contains('active'));
  const listen = (node, type, fn, options) => { node.addEventListener(type, fn, options); listeners.push(() => node.removeEventListener(type, fn, options)); };
  function aim(button, smooth = true) {
    Object.assign(pod.style, {
      transition: smooth && !reduced.matches && !doc.hidden ? '' : 'none',
      width: button.offsetWidth + 'px', height: button.offsetHeight + 'px',
      transform: `translate(${button.offsetLeft}px, ${button.offsetTop}px)`,
    });
    pod.hidden = false;
  }
  function reveal(button) {
    const left = dock.scrollLeft || 0;
    if (button.offsetLeft < left) dock.scrollLeft = button.offsetLeft;
    else if (button.offsetLeft + button.offsetWidth > left + dock.clientWidth) dock.scrollLeft = button.offsetLeft + button.offsetWidth - dock.clientWidth;
  }
  function selection(smooth = true) {
    if (gesture.id !== undefined) return;
    const button = active();
    if (!button) { pod.hidden = true; lastTarget = null; return; }
    const key = `${button.dataset.dockIndex}/${button.offsetLeft}/${button.offsetTop}/${button.offsetWidth}/${button.offsetHeight}`;
    if (smooth && key === lastTarget) return;
    lastTarget = key; aim(button, smooth); reveal(button);
  }
  function clearPointer() {
    const id = gesture.id; gesture.cancel(); pointerRects = dockRect = previewTarget = null;
    dock.querySelectorAll('.dock-preview').forEach(button => button.classList.remove('dock-preview'));
    if (id !== undefined && dock.hasPointerCapture(id)) dock.releasePointerCapture(id);
  }
  function cancel() { clearPointer(); lastTarget = null; if (!disposed) selection(); }
  function down(event) {
    const button = event.target.closest('[data-dock-select]');
    if (!button || button.disabled) return;
    // Dock 超出视口宽度时，触屏继续使用原生横向滚动。
    if (event.pointerType === 'touch' && dock.scrollWidth > dock.clientWidth) return;
    if (!gesture.begin(event.pointerId, event.clientX, event.clientY, event.isPrimary, event.button)) return;
    blockedUntil = 0;
    pointerRects = [...dock.querySelectorAll('[data-dock-select]')].filter(button => button.getClientRects().length).map(button => ({ el: button, ...button.getBoundingClientRect().toJSON() }));
    dockRect = dock.getBoundingClientRect(); dock.setPointerCapture(event.pointerId);
    previewTarget = button; aim(button);
  }
  function hit(event) {
    if (!dockRect || event.clientX < dockRect.left || event.clientX > dockRect.right || event.clientY < dockRect.top || event.clientY > dockRect.bottom) return -1;
    const index = hitDockItem(pointerRects, event.clientX, event.clientY);
    return index >= 0 && !pointerRects[index].el.disabled ? index : -1;
  }
  function move(event) {
    if (event.pointerId !== gesture.id || !gesture.move(event.pointerId, event.clientX, event.clientY)) return;
    const index = hit(event);
    const target = index >= 0 ? pointerRects[index].el : null;
    if (target === previewTarget) return;
    previewTarget = target;
    dock.querySelectorAll('[data-dock-select]').forEach(button => button.classList.toggle('dock-preview', target === button));
    if (target) aim(target);
  }
  function up(event) {
    if (event.pointerId !== gesture.id) return;
    const index = hit(event), button = index >= 0 ? pointerRects[index].el : null;
    const dragged = gesture.dragged;
    blockedUntil = win.performance.now() + 180;
    clearPointer(); lastTarget = null;
    if (!button?.isConnected || button.disabled) { selection(); return; }
    committing = true; button.click(); committing = false; aim(button);
    if (dragged) button.focus({ preventScroll: true });
  }
  function key(event) {
    const button = event.target.closest('[data-dock-select]');
    if (!button) return;
    if (event.key === 'Escape') { cancel(); return; }
    if (event.key === 'ArrowUp') {
      const selected = shell?.querySelector('.module-navigation [aria-current], .module-navigation [aria-pressed="true"]');
      if (selected) { event.preventDefault(); selected.focus({ preventScroll: true }); }
      return;
    }
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    const items = buttons(), index = items.indexOf(button);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : -1) + items.length) % items.length;
    event.preventDefault(); items[next]?.focus({ preventScroll: true }); if (items[next]) reveal(items[next]);
  }
  function resize() {
    if (disposed) return;
    if (gesture.id !== undefined) clearPointer();
    buttons().forEach((button, index) => { button.dataset.dockIndex = String(index); });
    lastTarget = null; selection(false);
    if (area && shell) {
      const height = Math.ceil(area.getBoundingClientRect().height);
      if (height > 0) shell.style.setProperty('--dock-island-h', height + 'px');
    }
  }
  function schedule() {
    if (queued || disposed) return;
    queued = true; queueMicrotask(() => { queued = false; if (!disposed) selection(); });
  }
  listen(dock, 'pointerdown', down); listen(dock, 'pointermove', move); listen(dock, 'pointerup', up);
  listen(dock, 'pointercancel', cancel); listen(dock, 'lostpointercapture', cancel);
  listen(dock, 'click', event => { if (!committing && event.detail > 0 && win.performance.now() < blockedUntil) { event.preventDefault(); event.stopImmediatePropagation(); } }, true);
  listen(dock, 'keydown', key); listen(win, 'resize', resize); listen(reduced, 'change', () => { clearPointer(); selection(false); });
  listen(win, 'blur', cancel); listen(doc, 'visibilitychange', () => { clearPointer(); resize(); });
  const observer = new win.ResizeObserver(resize); observer.observe(dock);
  if (area) observer.observe(area);
  const mutations = new win.MutationObserver(schedule); mutations.observe(dock, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ['class', 'aria-pressed'] });
  resize();
  return {
    update() { resize(); schedule(); },
    destroy() {
      if (disposed) return;
      disposed = true; clearPointer();
      observer.disconnect(); mutations.disconnect(); listeners.forEach(remove => remove()); shell?.style.removeProperty('--dock-island-h');
    },
  };
}
export function updateDock(handle) { handle.update(); }
export function destroyDock(handle) { handle.destroy(); }
