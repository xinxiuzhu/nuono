// 保留原生 <details> 的内容结构与 toggle 事件，仅对展开/收起时的可视高度做过渡动画。
const duration = 220;
const easing = 'cubic-bezier(.2,.8,.2,1)';

export function focusDisclosureTrigger(panel) {
  if (!panel.contains(panel.ownerDocument.activeElement) || !panel.id) return;
  const trigger = [...panel.ownerDocument.querySelectorAll('[aria-controls]')]
    .find(node => node.getAttribute('aria-controls')?.split(/\s+/).includes(panel.id));
  trigger?.focus({preventScroll: true});
}

export function attachDisclosures() {
  const doc = document, win = doc.defaultView;
  const reduced = win.matchMedia('(prefers-reduced-motion: reduce)');
  const active = new Map();
  const responsive = new Map();
  function syncResponsive() {
    for (const [panel, entry] of responsive) if (!panel.isConnected) {
      entry.media.removeEventListener('change', entry.sync); responsive.delete(panel);
    }
    for (const panel of doc.querySelectorAll('[data-disclosure-breakpoint]')) {
      let entry = responsive.get(panel);
      if (!entry) {
        const media = win.matchMedia(panel.dataset.disclosureBreakpoint);
        const sync = () => {
          const closed = media.matches && panel.dataset.disclosureOpen !== 'true';
          if (closed) focusDisclosureTrigger(panel);
          if (panel.inert !== closed) panel.inert = closed;
        };
        entry = {media, sync, inert:panel.inert}; responsive.set(panel, entry);
        media.addEventListener('change', sync);
      }
      entry.sync();
    }
  }

  function restoreInert(state) {
    for (const [node, inert] of state.inert) node.inert = inert;
    state.inert.clear();
  }
  function release(details, state, commit) {
    state.animation?.cancel();
    state.resize?.disconnect();
    state.mutations?.disconnect();
    win.cancelAnimationFrame(state.frame);
    active.delete(details);
    if (commit) details.open = state.target;
    Object.assign(details.style, state.style);
    restoreInert(state);
    if (state.aria === null) state.summary.removeAttribute('aria-expanded');
    else state.summary.setAttribute('aria-expanded', state.aria);
    delete details.dataset.disclosureState;
  }
  function expandedHeight(details) {
    const rect = details.getBoundingClientRect(), style = win.getComputedStyle(details);
    let bottom = rect.top + parseFloat(style.paddingTop) + parseFloat(style.borderTopWidth);
    for (const child of details.children) {
      const css = win.getComputedStyle(child);
      if (css.display === 'none' || ['absolute', 'fixed'].includes(css.position)) continue;
      bottom = Math.max(bottom, child.getBoundingClientRect().bottom + parseFloat(css.marginBottom));
    }
    return bottom - rect.top + parseFloat(style.paddingBottom) + parseFloat(style.borderBottomWidth);
  }
  function animate(details, target) {
    const start = details.getBoundingClientRect().height;
    let state = active.get(details);
    if (!state) {
      const summary = details.querySelector(':scope > summary');
      state = {summary, aria: summary.getAttribute('aria-expanded'), inert: new Map(), frame: 0,
        style: {height: details.style.height, overflow: details.style.overflow, boxSizing: details.style.boxSizing}};
      active.set(details, state);
    }
    state.animation?.cancel();
    state.resize?.disconnect();
    state.mutations?.disconnect();
    win.cancelAnimationFrame(state.frame);
    state.target = target;
    state.summary.setAttribute('aria-expanded', String(target));
    details.dataset.disclosureState = target ? 'open' : 'closed';
    restoreInert(state);
    if (!target) {
      if (details.contains(doc.activeElement) && !state.summary.contains(doc.activeElement)) state.summary.focus({preventScroll:true});
      for (const child of details.children) if (child !== state.summary) {
        state.inert.set(child, child.inert); child.inert = true;
      }
    }
    // 在同一帧内测量起止高度；收起时先保持 open=true 播放高度动画，动画结束后再真正将原生 open 设为 false。
    details.style.height = state.style.height;
    details.style.boxSizing = 'border-box';
    details.style.overflow = 'hidden';
    details.open = target;
    const end = details.getBoundingClientRect().height;
    details.open = true;
    state.end = end;
    if (reduced.matches || Math.abs(end - start) < 1 || !details.animate) {
      release(details, state, true); return;
    }
    details.style.height = `${end}px`;
    const animation = details.animate([{height:`${start}px`}, {height:`${end}px`}], {duration, easing});
    state.animation = animation;
    animation.onfinish = () => {
      if (active.get(details) === state && state.animation === animation) release(details, state, true);
    };
    const retarget = () => {
      win.cancelAnimationFrame(state.frame);
      state.frame = win.requestAnimationFrame(() => {
        if (active.get(details) !== state || !state.target) return;
        if (Math.abs(expandedHeight(details) - state.end) > 1) animate(details, true);
      });
    };
    state.resize = new ResizeObserver(retarget);
    for (const child of details.children) state.resize.observe(child);
    state.mutations = new MutationObserver(retarget);
    state.mutations.observe(details, {childList:true, subtree:true, characterData:true});
  }
  function click(event) {
    if (event.defaultPrevented || event.button !== 0 || !(event.target instanceof Element)) return;
    const summary = event.target.closest('summary'), details = summary?.parentElement;
    if (details?.tagName !== 'DETAILS' || !details.closest('.app-shell,.rui-dialog')) return;
    // 表格列选择器是悬浮弹层而非文档流内的折叠面板，不做高度动画；summary 内部的按钮/链接也保留各自的点击行为。
    if (details.matches('.database-column-picker,[data-disclosure-animation="off"]')) return;
    if (event.target.closest('a,button,input,select,textarea,[contenteditable="true"]')) return;
    if (reduced.matches || !details.animate) return;
    event.preventDefault();
    animate(details, !(active.get(details)?.target ?? details.open));
  }
  function toggle(event) {
    const state = active.get(event.target);
    // 以外部对 open 属性的直接修改为准（例如动画播放途中通过代码强制收起）。
    if (state && !event.target.open) { state.target = false; release(event.target, state, true); }
  }
  function reduceMotion() {
    if (reduced.matches) for (const [details, state] of active) release(details, state, true);
  }
  const removals = new MutationObserver(() => {
    for (const [details, state] of active) if (!details.isConnected) release(details, state, false);
    syncResponsive();
  });
  removals.observe(doc.body, {childList:true, subtree:true, attributes:true, attributeFilter:['data-disclosure-open']});
  syncResponsive();
  doc.addEventListener('click', click);
  doc.addEventListener('toggle', toggle, true);
  reduced.addEventListener('change', reduceMotion);
  return {destroy() {
    doc.removeEventListener('click', click);
    doc.removeEventListener('toggle', toggle, true);
    reduced.removeEventListener('change', reduceMotion);
    removals.disconnect();
    for (const [details, state] of active) release(details, state, false);
    for (const [panel, entry] of responsive) { entry.media.removeEventListener('change', entry.sync); panel.inert = entry.inert; }
    responsive.clear();
  }};
}

export function destroyDisclosures(handle) { handle?.destroy(); }
