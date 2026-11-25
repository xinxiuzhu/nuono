// 仅对遮罩透明度与面板轻微位移做过渡动画；尺寸布局与主题样式仍由 CSS 控制。
const motions = new WeakMap();
const reduced = () => matchMedia('(prefers-reduced-motion: reduce)');

export function animateDialog(mask, open) {
  const previous = motions.get(mask);
  if (previous?.open === open) return previous.finished;
  const panel = mask.querySelector('[data-slot="dialog-content"]');
  const isDrawer = mask.dataset.variant === 'drawer';
  const first = !previous && open && !mask.dataset.motionReady;
  const panelStyle = getComputedStyle(panel);
  const start = {opacity: first ? '0' : getComputedStyle(mask).opacity,
    transform: first ? (isDrawer ? 'translateX(32px)' : 'translateY(6px)') : panelStyle.transform};
  destroyDialogMotion(mask);
  mask.dataset.motionReady = 'true';
  const opacity = open ? '1' : '0';
  const transform = open ? 'none' : (isDrawer ? 'translateX(24px)' : 'translateY(4px)');
  mask.style.opacity = opacity;
  panel.style.transform = transform;
  const media = reduced();
  if (media.matches || !mask.animate) return Promise.resolve();
  const options = isDrawer
    ? {duration: open ? 220 : 160, easing: 'cubic-bezier(0.22, 1, 0.36, 1)'}
    : {duration: open ? 180 : 140, easing: 'cubic-bezier(.2,.8,.2,1)'};
  const animations = [mask.animate([{opacity:start.opacity}, {opacity}], options),
    panel.animate([{transform:start.transform}, {transform}], options)];
  const finish = () => { if (media.matches) animations.forEach(animation => animation.finish()); };
  media.addEventListener('change', finish);
  const state = {open, animations, media, finish};
  state.finished = Promise.all(animations.map(animation => animation.finished.catch(() => {}))).then(() => {
    media.removeEventListener('change', finish);
    if (motions.get(mask) === state) {
      motions.delete(mask);
      animations.forEach(animation => animation.cancel());
      // 打开动画结束后移除内联 transform，避免面板变成新的包含块（影响内部 fixed 定位元素）。
      if (open) { mask.style.removeProperty('opacity'); panel.style.removeProperty('transform'); }
    }
  });
  motions.set(mask, state);
  return state.finished;
}

export function destroyDialogMotion(mask) {
  const state = motions.get(mask);
  if (!state) return;
  motions.delete(mask);
  state.media.removeEventListener('change', state.finish);
  state.animations.forEach(animation => animation.cancel());
}
