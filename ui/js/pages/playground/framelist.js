// FrameList: what went over the wire, one row per frame, with the selected
// frame's data underneath. Used for the server-sent events of an HTTP stream
// and for the frames of a WebSocket (both directions).
//
// The caller keeps at most `cap` frames; this component draws only the rows
// in view (rows have a fixed height), so a stream of thousands of events
// costs the same as one of twenty. While the list is scrolled to its end it
// follows new frames; scroll up and it stays where you are reading.
//
// Keyboard: the list is one tab stop. Arrow keys, Page Up/Down, Home and End
// move the selection.

import { html, useCallback, useEffect, useLayoutEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { CodeBlock, CopyButton, Icon } from '../../components/index.js';
import { cx } from '../../lib/dom.js';
import { formatDuration, formatNumber, plural } from '../../lib/format.js';
import { useMediaQuery, useSize, useUid } from '../../lib/hooks.js';

const OVERSCAN = 6;
const PREVIEW_CHARS = 240;

/** "+0ms", "+312ms", "+1.24s": time since the request (or the connection) began. */
export function formatOffset(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms)) return '';
  return `+${ms < 1 ? '0ms' : formatDuration(ms)}`;
}

/**
 * frames   [{ seq, at (ms offset), name, note? (what it carries, in a few words), data (string), dir? 'in' | 'out', tone? 'stop' | 'info' }]
 * total    frames seen in all (more than frames.length once the cap is reached)
 * cap      how many the caller keeps
 * noun     what a frame is called: "event" or "frame"
 * label    accessible name of the list
 * toText   (frame) => string, for "copy all"
 * empty    what to show when there are no frames
 */
export default function FrameList({ frames, total, cap, noun = 'event', label, toText, empty, class: className }) {
  const id = useUid('frames');
  const scroller = useRef(null);
  const follow = useRef(true);
  const [sizeRef, size] = useSize();
  const attach = useCallback(
    (el) => {
      scroller.current = el;
      sizeRef(el);
    },
    [sizeRef],
  );
  const coarse = useMediaQuery('(pointer: coarse)');
  const rowHeight = coarse ? 40 : 28;
  const [scrollTop, setScrollTop] = useState(0);
  const [selected, setSelected] = useState(null);

  const count = frames.length;
  const lastSeq = count > 0 ? frames[count - 1].seq : -1;

  // Follow the tail while the reader is at the end.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && follow.current) el.scrollTop = el.scrollHeight;
  }, [lastSeq, rowHeight, size.height]);

  // A new request starts a new list: forget the selection of the old one.
  const firstSeq = count > 0 ? frames[0].seq : -1;
  useEffect(() => {
    if (selected != null && (count === 0 || selected < firstSeq || selected > lastSeq)) setSelected(null);
  }, [selected, firstSeq, lastSeq, count]);

  if (count === 0) return empty ?? null;

  const viewport = size.height || 280;
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - OVERSCAN);
  const end = Math.min(count, Math.ceil((scrollTop + viewport) / rowHeight) + OVERSCAN);
  const selectedIndex = selected == null ? -1 : frames.findIndex((f) => f.seq === selected);
  const current = selectedIndex === -1 ? null : frames[selectedIndex];

  const select = (index) => {
    const frame = frames[Math.max(0, Math.min(count - 1, index))];
    if (!frame) return;
    setSelected(frame.seq);
    const el = scroller.current;
    if (!el) return;
    const top = frames.indexOf(frame) * rowHeight;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + rowHeight > el.scrollTop + el.clientHeight) el.scrollTop = top + rowHeight - el.clientHeight;
  };

  const onKeyDown = (event) => {
    const page = Math.max(1, Math.floor(viewport / rowHeight) - 1);
    const at = selectedIndex === -1 ? (event.key === 'ArrowUp' ? count : -1) : selectedIndex;
    let next;
    if (event.key === 'ArrowDown') next = at + 1;
    else if (event.key === 'ArrowUp') next = at - 1;
    else if (event.key === 'PageDown') next = at + page;
    else if (event.key === 'PageUp') next = at - page;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = count - 1;
    else return;
    event.preventDefault();
    select(next);
  };

  return html`
    <div class=${cx('play-frames', className)}>
      <div class="play-frames-head">
        <span class="num">${plural(count, noun)}</span>
        ${total > count && html`<span class="faint">The last ${formatNumber(cap)} of ${formatNumber(total)} are kept.</span>`}
        <span class="grow"></span>
        ${toText && html`<${CopyButton} value=${() => frames.map(toText).join('\n')} label=${`Copy all ${noun}s`} />`}
      </div>
      <div
        ref=${attach}
        id=${id}
        class="play-frames-list"
        role="listbox"
        tabindex="0"
        aria-label=${label}
        aria-activedescendant=${current ? `${id}-${current.seq}` : undefined}
        onKeyDown=${onKeyDown}
        onScroll=${(event) => {
          const el = event.currentTarget;
          follow.current = el.scrollTop + el.clientHeight >= el.scrollHeight - rowHeight;
          setScrollTop(el.scrollTop);
        }}
      >
        <div class="play-frames-space" style=${`height:${count * rowHeight}px`}>
          ${frames.slice(start, end).map(
            (frame, i) => html`
              <div
                key=${frame.seq}
                id=${`${id}-${frame.seq}`}
                class="play-frame"
                role="option"
                aria-selected=${frame.seq === selected ? 'true' : 'false'}
                data-tone=${frame.tone}
                style=${`top:${(start + i) * rowHeight}px;height:${rowHeight}px`}
                onClick=${() => setSelected(frame.seq)}
              >
                <span class="play-frame-at num">${formatOffset(frame.at)}</span>
                ${frame.dir && html`<${Icon} name=${frame.dir === 'out' ? 'arrow-up' : 'arrow-down'} size=${12} label=${frame.dir === 'out' ? 'Sent' : 'Received'} />`}
                <span class="play-frame-name mono">${frame.name}</span>
                ${frame.note && html`<span class="play-frame-note">${frame.note}</span>`}
                <span class="play-frame-data mono">${frame.data.length > PREVIEW_CHARS ? frame.data.slice(0, PREVIEW_CHARS) : frame.data}</span>
              </div>
            `,
          )}
        </div>
      </div>
      ${current
        ? html`<${CodeBlock}
            class="play-frames-detail"
            title=${`${current.dir === 'out' ? 'Sent ' : current.dir === 'in' ? 'Received ' : ''}${current.name} at ${formatOffset(current.at)}`}
            value=${current.data || '(no data)'}
            maxHeight="280px"
          />`
        : html`<p class="play-frames-hint faint">Select ${noun === 'event' ? 'an event' : 'a frame'} to read its data.</p>`}
    </div>
  `;
}
