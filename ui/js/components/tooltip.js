// Tooltip: a short hint shown on hover and on keyboard focus.
//
//   html`<${Tooltip} content="Clears the cooldown on this credential">
//     <${Button} icon="refresh">Reset<//>
//   <//>`
//
// A tooltip adds to what is on screen; it never carries the only copy of
// something the user needs (touch screens do not show tooltips at all).

import { html, useEffect, useLayoutEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { cx, focusableWithin, placeFloating, scrollMoves } from '../lib/dom.js';
import { usePresence, useUid } from '../lib/hooks.js';
import { Portal } from './portal.js';

// Once one tooltip has been shown, its neighbours open without the delay and
// without the animation, so scanning a toolbar feels immediate.
let lastHiddenAt = 0;
const WARM_MS = 400;

/**
 * content   text or markup; nothing is shown when it is empty
 * side      "top" (default) | "bottom" | "left" | "right"; flips when there is no room
 * align     "center" (default) | "start" | "end"
 * delay     ms before the first tooltip appears (default 450)
 * describe  set false when the child already has the same text as its
 *           accessible name (IconButton does this)
 */
export function Tooltip({ content, side = 'top', align = 'center', delay = 450, describe = true, disabled = false, class: className, children }) {
  const anchor = useRef(null);
  const tip = useRef(null);
  const timer = useRef(null);
  const id = useUid('tip');
  const [open, setOpen] = useState(false);
  const [instant, setInstant] = useState(false);
  const [pos, setPos] = useState(null);
  const active = open && !disabled && content != null && content !== '' && content !== false;
  const { mounted, state } = usePresence(active, 140);

  const show = (immediate) => {
    clearTimeout(timer.current);
    const warm = Date.now() - lastHiddenAt < WARM_MS;
    if (immediate || warm) {
      setInstant(warm);
      setOpen(true);
    } else {
      timer.current = setTimeout(() => {
        setInstant(false);
        setOpen(true);
      }, delay);
    }
  };

  const hide = () => {
    clearTimeout(timer.current);
    setOpen((was) => {
      if (was) lastHiddenAt = Date.now();
      return false;
    });
  };

  useEffect(() => () => clearTimeout(timer.current), []);

  // Escape dismisses a tooltip without closing whatever is behind it.
  useEffect(() => {
    if (!active) return undefined;
    const onKey = (event) => {
      if (event.key === 'Escape') hide();
    };
    // Only scrolling that moves the anchor: a list elsewhere that scrolls by
    // itself (a log tail) must not take every tooltip down.
    const onScroll = (event) => {
      if (scrollMoves(event.target, anchor.current)) hide();
    };
    document.addEventListener('keydown', onKey);
    window.addEventListener('scroll', onScroll, true);
    return () => {
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [active]);

  // Measure and place each time it is shown. `active` is a dependency, not
  // only `mounted`: a tooltip shown again while it is still fading out was
  // never unmounted, and its anchor may have moved since.
  useLayoutEffect(() => {
    if (!active || !anchor.current || !tip.current) return;
    const target = anchor.current.firstElementChild ?? anchor.current;
    const rect = target.getBoundingClientRect();
    const next = placeFloating(rect, { width: tip.current.offsetWidth, height: tip.current.offsetHeight }, { side, align, gap: 8 });
    setPos((prev) => (prev && prev.top === next.top && prev.left === next.left && prev.origin === next.origin ? prev : next));
  }, [active, mounted, content, side, align]);

  // Tie the hint to the focusable child for screen readers.
  useEffect(() => {
    if (!describe || !mounted || !anchor.current) return undefined;
    const target = focusableWithin(anchor.current)[0];
    if (!target || target.hasAttribute('aria-describedby')) return undefined;
    target.setAttribute('aria-describedby', id);
    return () => target.removeAttribute('aria-describedby');
  }, [describe, mounted, id]);

  return html`
    <span
      ref=${anchor}
      class=${cx('tip-anchor', className)}
      onPointerEnter=${(event) => event.pointerType !== 'touch' && show(false)}
      onPointerLeave=${hide}
      onPointerDown=${hide}
      onFocusCapture=${(event) => {
        // Only keyboard focus: a click already shows what the control does.
        if (event.target.matches?.(':focus-visible')) show(true);
      }}
      onBlurCapture=${hide}
    >
      ${children}
    </span>
    ${mounted &&
    html`
      <${Portal}>
        <div
          ref=${tip}
          id=${id}
          role="tooltip"
          class="tip"
          data-state=${pos ? state : 'closed'}
          data-instant=${instant ? '' : undefined}
          style=${pos ? `top:${pos.top}px;left:${pos.left}px;transform-origin:${pos.origin}` : 'top:0;left:0;visibility:hidden'}
        >
          ${content}
        </div>
      <//>
    `}
  `;
}
