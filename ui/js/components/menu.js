// Menu: a dropdown of actions.
//
//   html`<${Menu} label="Provider actions" items=${[
//     { label: 'Test connection', icon: 'zap', onSelect: test },
//     { label: 'Discover models', icon: 'search', onSelect: discover },
//     { separator: true },
//     { label: 'Delete provider', icon: 'trash', danger: true, onSelect: remove },
//   ]} />`
//
// With no `trigger` it renders a "more" icon button. For a custom trigger
// pass a function; spread the props it receives onto your button:
//
//   html`<${Menu} items=${items}
//         trigger=${(props) => html`<${Button} iconRight="chevron-down" ...${props}>Range<//>`} />`

import { html, useEffect, useLayoutEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { cx, placeFloating } from '../lib/dom.js';
import { useModalLayer, useOutsidePointer, usePresence, useUid } from '../lib/hooks.js';
import { IconButton } from './button.js';
import { Icon } from './icons.js';
import { Portal } from './portal.js';

/**
 * items   [{ label, icon?, hint?, onSelect?, href?, danger?, disabled?, checked? }
 *          | { separator: true } | { heading: 'Text' }]
 *         `checked` (true/false) draws a tick column, for pick-one menus.
 * label   accessible name of the default trigger and of the menu
 * icon    icon of the default trigger (default "more")
 * side    "bottom" (default) | "top" | "left" | "right"
 * align   "end" (default) | "start" | "center"
 * size    size of the default trigger
 *
 * Keyboard: Enter, Space or ArrowDown opens; arrows move; Home/End jump;
 * typing a letter jumps to the next item starting with it; Enter selects;
 * Escape or Tab closes and returns focus to the trigger.
 */
export function Menu({ items, trigger, label = 'More actions', icon = 'more', side = 'bottom', align = 'end', size = 'md', class: className }) {
  const anchor = useRef(null);
  const menu = useRef(null);
  const id = useUid('menu');
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [pos, setPos] = useState(null);
  // Opening from the keyboard skips the animation (see UI_GUIDE, Motion).
  const [instant, setInstant] = useState(false);
  const { mounted, state } = usePresence(open, 140);

  const selectable = items.map((item, index) => (item.separator || item.heading || item.disabled ? -1 : index)).filter((i) => i !== -1);
  const hasChecks = items.some((item) => typeof item.checked === 'boolean');

  const close = () => setOpen(false);
  useModalLayer(menu, open, { onClose: close, lock: false });
  useOutsidePointer([anchor, menu], close, open);

  // Measure and place on every opening. `open` is a dependency, not only
  // `mounted`: a menu reopened while its exit animation is still running was
  // never unmounted, and still has to be placed.
  useLayoutEffect(() => {
    if (!open || !anchor.current || !menu.current) return;
    const target = anchor.current.firstElementChild ?? anchor.current;
    const next = placeFloating(target.getBoundingClientRect(), { width: menu.current.offsetWidth, height: menu.current.offsetHeight }, { side, align, gap: 4 });
    setPos(next);
  }, [open, mounted, items.length, side, align]);

  // The position is kept while the menu fades out where it stood, and
  // dropped once it has left the document.
  useEffect(() => {
    if (!mounted) setPos(null);
  }, [mounted]);

  useEffect(() => {
    if (!open) return undefined;
    // A menu is anchored to something that scrolls away: close rather than chase it.
    const onScroll = (event) => {
      if (menu.current && menu.current.contains(event.target)) return;
      close();
    };
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', close);
    return () => {
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', close);
    };
  }, [open]);

  const openMenu = (fromKeyboard, startAt = 'none') => {
    setInstant(fromKeyboard);
    setActive(startAt === 'first' ? (selectable[0] ?? -1) : startAt === 'last' ? (selectable[selectable.length - 1] ?? -1) : -1);
    setOpen(true);
  };

  const select = (item, event) => {
    // `!open`: the second click of a double-click lands on a menu that is
    // already fading out, and must not run the action again.
    if (item.disabled || !open) {
      event?.preventDefault();
      return;
    }
    close();
    item.onSelect?.(event);
  };

  const move = (delta) => {
    if (selectable.length === 0) return;
    const at = selectable.indexOf(active);
    const next = at === -1 ? (delta > 0 ? 0 : selectable.length - 1) : (at + delta + selectable.length) % selectable.length;
    setActive(selectable[next]);
  };

  const onMenuKey = (event) => {
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        move(1);
        break;
      case 'ArrowUp':
        event.preventDefault();
        move(-1);
        break;
      case 'Home':
        event.preventDefault();
        setActive(selectable[0] ?? -1);
        break;
      case 'End':
        event.preventDefault();
        setActive(selectable[selectable.length - 1] ?? -1);
        break;
      case 'Enter':
      case ' ': {
        event.preventDefault();
        const item = items[active];
        if (!item) break;
        if (item.href) menu.current.querySelector(`[data-index="${active}"]`)?.click();
        else select(item, event);
        break;
      }
      case 'Tab':
        event.preventDefault();
        close();
        break;
      default:
        if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          const letter = event.key.toLowerCase();
          const from = selectable.indexOf(active);
          const order = [...selectable.slice(from + 1), ...selectable.slice(0, from + 1)];
          const hit = order.find((i) => String(items[i].label).toLowerCase().startsWith(letter));
          if (hit != null) setActive(hit);
        }
    }
  };

  // Keep the active item in view in long menus.
  useEffect(() => {
    if (!open || active < 0) return;
    menu.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [open, active]);

  const triggerProps = {
    'aria-haspopup': 'menu',
    'aria-expanded': open ? 'true' : 'false',
    'aria-controls': open ? id : undefined,
    onClick: (event) => {
      if (open) close();
      // detail === 0 means the click came from the keyboard.
      else openMenu(event.detail === 0, event.detail === 0 ? 'first' : 'none');
    },
    onKeyDown: (event) => {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        openMenu(true, event.key === 'ArrowDown' ? 'first' : 'last');
      }
    },
  };

  return html`
    <span ref=${anchor} class=${cx('tip-anchor', className)}>
      ${trigger ? trigger(triggerProps) : html`<${IconButton} icon=${icon} label=${label} size=${size} tooltip=${false} ...${triggerProps} />`}
    </span>
    ${mounted &&
    html`
      <${Portal}>
        <div
          ref=${menu}
          id=${id}
          class="menu"
          role="menu"
          aria-label=${label}
          aria-activedescendant=${active >= 0 ? `${id}-${active}` : undefined}
          tabindex="-1"
          data-state=${pos ? (instant && open ? 'open' : state) : 'closed'}
          data-autofocus=""
          style=${pos
            ? `top:${pos.top}px;left:${pos.left}px;transform-origin:${pos.origin}${instant ? ';transition-duration:0ms' : ''}`
            : 'top:0;left:0;visibility:hidden'}
          onKeyDown=${onMenuKey}
        >
          ${items.map((item, index) => {
            if (item.separator) return html`<div class="menu-sep" role="separator" key=${`sep-${index}`}></div>`;
            if (item.heading) return html`<div class="menu-heading" key=${`h-${index}`}>${item.heading}</div>`;
            const shared = {
              id: `${id}-${index}`,
              class: 'menu-item',
              role: typeof item.checked === 'boolean' ? 'menuitemradio' : 'menuitem',
              tabindex: -1,
              'data-index': index,
              'data-active': index === active ? '' : undefined,
              'data-danger': item.danger ? '' : undefined,
              'aria-disabled': item.disabled ? 'true' : undefined,
              'aria-checked': typeof item.checked === 'boolean' ? (item.checked ? 'true' : 'false') : undefined,
              onPointerMove: () => !item.disabled && active !== index && setActive(index),
              onClick: (event) => select(item, event),
            };
            const body = html`
              ${hasChecks && html`<${Icon} name="check" size=${14} class=${item.checked ? undefined : 'invisible'} />`}
              ${item.icon && html`<${Icon} name=${item.icon} />`}
              <span class="menu-label">${item.label}</span>
              ${item.hint && html`<span class="menu-hint">${item.hint}</span>`}
            `;
            return item.href && !item.disabled
              ? html`<a key=${index} href=${item.href} target=${item.external ? '_blank' : undefined} rel=${item.external ? 'noreferrer' : undefined} ...${shared}>${body}</a>`
              : html`<div key=${index} ...${shared}>${body}</div>`;
          })}
        </div>
      <//>
    `}
  `;
}
