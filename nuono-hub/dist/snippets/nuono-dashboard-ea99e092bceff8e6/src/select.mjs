// 只管理选择器的 DOM 交互；选项、值和业务写入继续由 Leptos 持有。
export const SEARCH_THRESHOLD = 8;
const instances = new WeakMap();
let nextId = 0;

export function filterOptions(options, query) {
  const words = query.trim().toLocaleLowerCase().split(/\s+/u).filter(Boolean);
  return options.filter(option => !option.hidden && words.every(word =>
    `${option.label} ${option.value} ${option.group} ${option.searchText || ''}`.toLocaleLowerCase().includes(word)));
}

export function moveActive(options, current, key) {
  const enabled = options.filter(option => !option.disabled).map(option => option.index);
  if (!enabled.length) return null;
  if (key === 'Home') return enabled[0];
  if (key === 'End') return enabled.at(-1);
  const position = enabled.indexOf(current);
  if (position < 0) return key === 'ArrowUp' ? enabled.at(-1) : enabled[0];
  return enabled[Math.max(0, Math.min(enabled.length - 1, position + (key === 'ArrowUp' ? -1 : 1)))];
}

export function popupPlacement(rect, viewport) {
  const margin = 8, gap = 6;
  const viewportLeft = viewport.offsetLeft || 0, viewportTop = viewport.offsetTop || 0;
  const width = Math.min(Math.max(rect.width, 240), Math.max(0, viewport.width - margin * 2));
  const left = Math.max(viewportLeft + margin, Math.min(rect.left, viewportLeft + viewport.width - width - margin));
  const topEdge = viewportTop + margin, bottomEdge = viewportTop + viewport.height - margin;
  const belowStart = Math.max(topEdge, Math.min(rect.bottom + gap, bottomEdge));
  const aboveEnd = Math.max(topEdge, Math.min(rect.top - gap, bottomEdge));
  const below = Math.max(0, bottomEdge - belowStart);
  const above = Math.max(0, aboveEnd - topEdge);
  const up = below < 260 && above > below;
  return { left, width, maxHeight: Math.min(360, up ? above : below),
    ...(up ? { bottom: (viewport.layoutHeight ?? viewport.height) - aboveEnd } : { top: belowStart }) };
}

export function attachSelect(root) {
  if (instances.has(root)) return instances.get(root);
  const doc = root.ownerDocument, win = doc.defaultView;
  const source = root.querySelector('.select-source');
  const editable = root.dataset.selectEditable === 'true';
  const choices = editable ? root.querySelector('datalist') : source;
  const id = `nuono-select-${++nextId}`;
  const listeners = [];
  let disposed = false, scheduled = false, opened = false, searchable = false;
  let options = [], visible = [], active = null, query = '', typeahead = '', typedAt = 0;
  let composing = false, snapshot = '', labels = [];
  let selectedAvatar = null, avatarSnapshot = '';
  let menuWidth = 240;
  let placedAnchor = null;
  const measure = doc.createElement('canvas').getContext('2d');
  const trigger = doc.createElement(editable ? 'input' : 'button');
  if (editable) { trigger.type = 'text'; trigger.autocomplete = 'off'; }
  else trigger.type = 'button';
  trigger.id = `${id}-trigger`;
  trigger.setAttribute('role', 'combobox');
  trigger.setAttribute('aria-haspopup', 'listbox');
  trigger.setAttribute('aria-controls', `${id}-list`);
  trigger.setAttribute('aria-expanded', 'false');
  if (editable) trigger.setAttribute('aria-autocomplete', 'list');
  const caption = doc.createElement('span');
  caption.className = 'select-caption';
  const arrow = doc.createElement('span');
  arrow.className = 'select-chevron';
  arrow.setAttribute('aria-hidden', 'true');
  if (!editable) trigger.append(caption, arrow);

  const popup = doc.createElement('div');
  popup.className = 'select-popup';
  popup.id = `${id}-popup`;
  popup.setAttribute('popover', 'manual');
  popup.hidden = true;
  const search = doc.createElement('input');
  search.type = 'search';
  search.className = 'input select-search';
  search.autocomplete = 'off';
  search.spellcheck = false;
  search.setAttribute('role', 'searchbox');
  search.setAttribute('aria-controls', `${id}-list`);
  const list = doc.createElement('div');
  list.id = `${id}-list`;
  list.className = 'select-options';
  list.setAttribute('role', 'listbox');
  const empty = doc.createElement('div');
  empty.className = 'select-empty';
  empty.setAttribute('role', 'status');
  popup.append(search, list, empty);
  root.append(trigger, popup);
  source.setAttribute('tabindex', '-1');
  source.setAttribute('aria-hidden', 'true');
  root.dataset.selectEnhanced = 'true';

  function listen(node, type, callback, settings) {
    node.addEventListener(type, callback, settings);
    listeners.push(() => node.removeEventListener(type, callback, settings));
  }
  function attr(node, name, value) {
    if (value === null || value === undefined || value === '') {
      if (node.hasAttribute(name)) node.removeAttribute(name);
    } else if (node.getAttribute(name) !== String(value)) node.setAttribute(name, String(value));
  }
  function accessibleName() {
    labels = [...(source.labels || [])];
    if (!labels.length) {
      const field = root.closest('.field, .database-editor-field');
      const label = field?.querySelector('.field-label');
      if (label) labels = [label];
    }
    const explicit = source.getAttribute('aria-label');
    const text = explicit || labels.map(label => {
      const copy = label.cloneNode(true);
      copy.querySelectorAll('.select-control, .help-tip').forEach(node => node.remove());
      return copy.textContent.trim();
    }).filter(Boolean).join(' ') || root.dataset.selectLabel;
    attr(trigger, 'aria-label', text);
    attr(list, 'aria-label', text);
  }
  function readOptions() {
    return [...choices.querySelectorAll('option')].map((option, index) => {
      const group = option.closest('optgroup');
      return { index, value: option.value, label: option.label || option.textContent.trim() || option.value,
        searchText: option.dataset.search || '',
        avatar: option.hasAttribute('data-avatar') ? option.dataset.avatar : null,
        group: group?.label || '', disabled: option.disabled || Boolean(group?.disabled),
        hidden: option.hidden || Boolean(group?.hidden) };
    });
  }
  function currentValue() {
    return root.hasAttribute('data-select-value') ? root.dataset.selectValue : source.value;
  }
  function focusedControl() { return searchable && !editable ? search : trigger; }
  function createAvatar(option) {
    if (option?.avatar == null) return null;
    const avatar = doc.createElement('span');
    avatar.className = 'select-avatar';
    avatar.setAttribute('aria-hidden', 'true');
    const fallback = doc.createElement('span');
    fallback.className = 'select-avatar-fallback';
    fallback.textContent = [...option.label.trim()][0]?.toLocaleUpperCase() || '';
    avatar.append(fallback);
    if (option.avatar) {
      const img = doc.createElement('img');
      img.alt = ''; img.width = 28; img.height = 28; img.decoding = 'async';
      img.addEventListener('load', () => { fallback.hidden = true; }, { once: true });
      img.addEventListener('error', () => img.remove(), { once: true });
      img.src = option.avatar;
      avatar.append(img);
    }
    return avatar;
  }
  function markActive(reveal = false) {
    const focus = focusedControl();
    for (const control of [trigger, search]) control.removeAttribute('aria-activedescendant');
    for (const node of list.querySelectorAll('[role="option"]')) {
      const isActive = Number(node.dataset.index) === active;
      node.toggleAttribute('data-active', isActive);
      if (isActive && opened) {
        focus.setAttribute('aria-activedescendant', node.id);
        if (reveal) {
          const offset = node.offsetTop - list.offsetTop;
          if (offset < list.scrollTop) list.scrollTop = offset;
          else if (offset + node.offsetHeight > list.scrollTop + list.clientHeight)
            list.scrollTop = offset + node.offsetHeight - list.clientHeight;
        }
      }
    }
  }
  function renderOptions() {
    visible = filterOptions(options, query);
    if (!visible.some(option => option.index === active && !option.disabled))
      active = visible.find(option => option.value === currentValue() && !option.disabled)?.index
        ?? moveActive(visible, null, 'Home');
    const fragment = doc.createDocumentFragment();
    let groupName = null, group = fragment;
    for (const option of visible) {
      if (option.group !== groupName) {
        groupName = option.group;
        group = fragment;
        if (groupName) {
          group = doc.createElement('div');
          group.setAttribute('role', 'group');
          group.setAttribute('aria-label', groupName);
          const heading = doc.createElement('div');
          heading.className = 'select-group-label';
          heading.setAttribute('aria-hidden', 'true');
          heading.textContent = groupName;
          group.append(heading);
          fragment.append(group);
        }
      }
      const row = doc.createElement('div');
      row.id = `${id}-option-${option.index}`;
      row.className = 'select-option';
      row.dataset.index = String(option.index);
      row.setAttribute('role', 'option');
      row.setAttribute('aria-selected', String(option.value === currentValue()));
      if (option.disabled) row.setAttribute('aria-disabled', 'true');
      const text = doc.createElement('span');
      text.className = 'select-option-label';
      text.textContent = option.label;
      const check = doc.createElement('span');
      check.className = 'select-check';
      check.setAttribute('aria-hidden', 'true');
      check.textContent = '✓';
      const avatar = createAvatar(option);
      if (avatar) row.append(avatar);
      row.append(text, check);
      group.append(row);
    }
    list.replaceChildren(fragment);
    empty.hidden = visible.length > 0;
    markActive(true);
  }
  function position() {
    const rect = trigger.getBoundingClientRect();
    placedAnchor = { left: rect.left, top: rect.top };
    const viewport = win.visualViewport;
    const placement = popupPlacement({ left: rect.left, top: rect.top, bottom: rect.bottom,
      width: Math.max(rect.width, menuWidth) }, { width: viewport?.width ?? win.innerWidth,
      height: viewport?.height ?? win.innerHeight, offsetLeft: viewport?.offsetLeft,
      offsetTop: viewport?.offsetTop, layoutHeight: win.innerHeight });
    popup.style.left = `${placement.left}px`;
    popup.style.width = `${placement.width}px`;
    popup.style.maxHeight = `${placement.maxHeight}px`;
    popup.style.top = placement.top === undefined ? 'auto' : `${placement.top}px`;
    popup.style.bottom = placement.bottom === undefined ? 'auto' : `${placement.bottom}px`;
  }
  function close(restoreFocus = false) {
    if (!opened) return;
    opened = false;
    trigger.setAttribute('aria-expanded', 'false');
    trigger.removeAttribute('aria-activedescendant');
    search.removeAttribute('aria-activedescendant');
    if (popup.matches(':popover-open')) popup.hidePopover();
    popup.hidden = true;
    query = ''; search.value = ''; typeahead = '';
    if (restoreFocus && trigger.isConnected && !trigger.disabled) trigger.focus({ preventScroll: true });
  }
  function sync() {
    scheduled = false;
    if (disposed) return;
    options = readOptions();
    const desired = currentValue();
    if (root.hasAttribute('data-select-value') && source.value !== desired) source.value = desired;
    const selected = options.find(option => option.value === desired);
    trigger.className = `${source.className.replace(/\bselect-source\b/g, '').trim()} select select-trigger${editable ? ' select-editable' : ''}`;
    trigger.style.cssText = source.style.cssText;
    trigger.disabled = source.matches(':disabled');
    if (editable) {
      if (trigger.value !== desired && !composing) trigger.value = desired;
      trigger.placeholder = root.dataset.selectPlaceholder;
    } else {
      caption.textContent = selected?.label || desired || root.dataset.selectPlaceholder;
      trigger.toggleAttribute('data-placeholder', !selected && !desired);
      const nextAvatar = JSON.stringify([selected?.avatar, selected?.label]);
      if (nextAvatar !== avatarSnapshot) {
        avatarSnapshot = nextAvatar;
        selectedAvatar?.remove();
        selectedAvatar = createAvatar(selected);
        if (selectedAvatar) trigger.prepend(selectedAvatar);
      }
    }
    attr(trigger, 'aria-invalid', source.getAttribute('aria-invalid'));
    attr(trigger, 'aria-busy', source.getAttribute('aria-busy'));
    attr(trigger, 'aria-describedby', source.getAttribute('aria-describedby'));
    accessibleName();
    search.placeholder = root.dataset.selectSearch;
    search.setAttribute('aria-label', root.dataset.selectSearch);
    empty.textContent = root.dataset.selectEmpty;
    // 搜索阈值按全部可见选项判断，过滤到少量结果后仍保留输入框。
    searchable = !editable && (root.dataset.selectSearchMode === 'always'
      || options.filter(option => !option.hidden).length > SEARCH_THRESHOLD);
    const wasSearchVisible = !search.hidden;
    search.hidden = !searchable;
    if (opened && (trigger.disabled || !trigger.getClientRects().length)) close(false);
    const nextSnapshot = JSON.stringify([options, desired]);
    if (nextSnapshot !== snapshot) {
      snapshot = nextSnapshot;
      if (measure) {
        const style = win.getComputedStyle(popup);
        measure.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
        menuWidth = options.filter(option => !option.hidden).reduce((width, option) =>
          Math.min(480, Math.max(width, Math.ceil(measure.measureText(option.label).width) + (option.avatar === null ? 64 : 104),
            Math.ceil(measure.measureText(option.group).width) + 32)), 240);
      }
      if (opened) renderOptions();
    }
    if (opened && wasSearchVisible !== searchable) focusedControl().focus({ preventScroll: true });
    if (opened) position();
  }
  function schedule() {
    if (disposed || scheduled) return;
    scheduled = true;
    win.queueMicrotask(sync);
  }
  function open(key = 'ArrowDown') {
    sync();
    if (trigger.disabled || opened) return;
    opened = true;
    query = ''; search.value = '';
    active = options.find(option => option.value === currentValue() && !option.disabled && !option.hidden)?.index
      ?? moveActive(filterOptions(options, ''), null, key);
    popup.hidden = false;
    if (typeof popup.showPopover === 'function') popup.showPopover();
    trigger.setAttribute('aria-expanded', 'true');
    position();
    renderOptions();
    focusedControl().focus({ preventScroll: true });
    markActive(true);
  }
  function dispatch(type) { source.dispatchEvent(new win.Event(type, { bubbles: true })); }
  function choose(index) {
    const option = options.find(option => option.index === index);
    if (!option || option.disabled || trigger.disabled) return;
    const previous = currentValue();
    source.value = option.value;
    if (editable) trigger.value = option.value;
    // 先关闭并恢复焦点，业务事件可能立即销毁当前表单或打开确认框。
    close(true);
    if (previous !== option.value) { dispatch('input'); dispatch('change'); }
    schedule();
  }
  function onKey(event) {
    if (event.isComposing || composing || trigger.disabled) return;
    const key = event.key;
    if (key === 'Escape' && opened) {
      event.preventDefault(); event.stopPropagation(); close(true); return;
    }
    if (key === 'Tab') { if (opened) close(search === event.target); return; }
    if (['ArrowDown', 'ArrowUp'].includes(key) || (!editable && event.target !== search && ['Home', 'End'].includes(key))) {
      event.preventDefault(); event.stopPropagation();
      if (!opened) open(key);
      else { active = moveActive(visible, active, key); markActive(true); }
      if (['Home', 'End'].includes(key)) { active = moveActive(visible, active, key); markActive(true); }
      return;
    }
    if (key === 'Enter' || (key === ' ' && !editable && event.target !== search)) {
      event.preventDefault(); event.stopPropagation();
      if (opened && editable && active === null) close(true);
      else if (opened) choose(active); else open();
      return;
    }
    if (event.target === trigger && !editable && key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
      event.preventDefault();
      if (!opened) open();
      if (searchable) { query += key; search.value = query; renderOptions(); }
      else {
        const now = Date.now();
        typeahead = now - typedAt > 650 ? key : typeahead + key; typedAt = now;
        const needle = [...typeahead].every(char => char === key) ? key : typeahead;
        const enabled = visible.filter(option => !option.disabled);
        const start = enabled.findIndex(option => option.index === active);
        const ordered = [...enabled.slice(start + 1), ...enabled.slice(0, start + 1)];
        active = ordered.find(option => option.label.toLocaleLowerCase().startsWith(needle.toLocaleLowerCase()))?.index ?? active;
        markActive(true);
      }
    }
  }
  listen(trigger, 'click', event => {
    event.preventDefault();
    if (editable) { if (!opened) open(); }
    else if (opened) close(true); else open();
  });
  listen(trigger, 'keydown', onKey);
  listen(search, 'keydown', onKey);
  listen(search, 'input', event => { event.stopPropagation(); query = search.value; active = null; renderOptions(); });
  for (const input of [search, trigger]) {
    listen(input, 'compositionstart', () => { composing = true; });
    listen(input, 'compositionend', () => { composing = false; });
  }
  if (editable) {
    listen(trigger, 'input', event => {
      event.stopPropagation();
      const text = trigger.value;
      source.value = text;
      dispatch('input');
      if (!opened) open();
      trigger.value = text;
      query = text; active = null; renderOptions();
    });
    listen(trigger, 'change', event => { event.stopPropagation(); dispatch('change'); });
  }
  listen(list, 'pointerdown', event => { if (event.target.closest('[role="option"]')) event.preventDefault(); });
  listen(list, 'pointermove', event => {
    const row = event.target.closest('[role="option"]');
    if (event.pointerType === 'mouse' && row && row.getAttribute('aria-disabled') !== 'true') {
      active = Number(row.dataset.index); markActive();
    }
  });
  listen(list, 'click', event => {
    const row = event.target.closest('[role="option"]');
    if (row) { event.preventDefault(); event.stopPropagation(); choose(Number(row.dataset.index)); }
  });
  listen(doc, 'pointerdown', event => { if (opened && !root.contains(event.target)) close(false); }, true);
  listen(doc, 'focusin', event => { if (opened && !root.contains(event.target)) close(false); });
  // label[for] 以及包裹式 label 均聚焦可见控件，不激活隐藏的原生选择器。
  listen(doc, 'click', event => {
    if (root.contains(event.target) || event.target.closest('button, input, a')) return;
    if (labels.some(label => label.contains(event.target))) {
      event.preventDefault(); trigger.focus(); if (!trigger.disabled) open();
    }
  });
  listen(win, 'resize', () => close(false));
  listen(doc, 'scroll', event => {
    if (!opened || popup.contains(event.target) || !placedAnchor) return;
    // 点击打开下拉菜单后，浏览器可能异步触发一次 focus/scrollIntoView 滚动事件；
    // 若触发按钮的实际屏幕坐标并未改变，则忽略该滚动事件，避免刚打开的菜单被立刻关闭。
    const rect = trigger.getBoundingClientRect();
    if (Math.abs(rect.left - placedAnchor.left) > 1 || Math.abs(rect.top - placedAnchor.top) > 1) close(false);
  }, true);
  // 移动端软键盘改变可视区域时重排菜单，不能因此关闭刚获得焦点的搜索框。
  if (win.visualViewport) {
    listen(win.visualViewport, 'resize', () => { if (opened) position(); });
    listen(win.visualViewport, 'scroll', () => { if (opened) position(); });
  }
  listen(source, 'change', schedule);
  listen(source, 'input', schedule);
  if (source.form) listen(source.form, 'reset', () => win.queueMicrotask(schedule));
  const observer = new win.MutationObserver(schedule);
  observer.observe(source, { attributes: true, childList: true, characterData: true, subtree: true });
  if (choices !== source) observer.observe(choices, { attributes: true, childList: true, characterData: true, subtree: true });
  observer.observe(root, { attributes: true, attributeFilter: ['data-select-value', 'data-select-placeholder', 'data-select-search', 'data-select-empty', 'data-select-label', 'data-select-search-mode'] });
  for (let ancestor = root.parentElement; ancestor; ancestor = ancestor.parentElement)
    observer.observe(ancestor, { attributes: true, attributeFilter: ['disabled', 'hidden', 'aria-hidden'] });
  const resize = new win.ResizeObserver(() => { if (opened) { if (!trigger.getClientRects().length) close(false); else position(); } });
  resize.observe(trigger);
  sync();
  const handle = { destroy() {
    if (disposed) return;
    close(false); disposed = true;
    observer.disconnect(); resize.disconnect();
    listeners.forEach(remove => remove());
    trigger.remove(); popup.remove();
    source.removeAttribute('tabindex'); source.removeAttribute('aria-hidden');
    delete root.dataset.selectEnhanced;
    instances.delete(root);
  } };
  instances.set(root, handle);
  return handle;
}

export function destroySelect(handle) { handle?.destroy(); }
