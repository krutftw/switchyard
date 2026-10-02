// Tiny observable store and the hook that reads it.
//
//   const counter = createStore({ n: 0 });
//   counter.set({ n: 1 });                 // shallow-merges into object state
//   counter.set((s) => ({ n: s.n + 1 }));  // updater form
//   counter.replace({ n: 0 });             // swap the whole value
//   const n = useStore(counter, (s) => s.n);
//
// Stores live at module level, so they work across separate render roots
// (overlays are rendered through a portal) and outside components.

import { useEffect, useRef, useState } from '../../vendor/preact-htm.js';

const isPlainObject = (v) => v !== null && typeof v === 'object' && Object.getPrototypeOf(v) === Object.prototype;

/** Shallow equality for plain objects and arrays; Object.is otherwise. */
export function shallowEqual(a, b) {
  if (Object.is(a, b)) return true;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((v, i) => Object.is(v, b[i]));
  }
  if (!isPlainObject(a) || !isPlainObject(b)) return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  return ka.length === kb.length && ka.every((k) => Object.is(a[k], b[k]));
}

/**
 * @template T
 * @param {T} initial
 * @returns {{ get(): T, set(next: Partial<T> | ((s: T) => Partial<T> | T)): void, replace(next: T): void, subscribe(fn: (s: T) => void): () => void }}
 */
export function createStore(initial) {
  let state = initial;
  const listeners = new Set();

  const commit = (next) => {
    if (Object.is(next, state)) return;
    state = next;
    // Copy: a listener may unsubscribe while we iterate.
    for (const fn of [...listeners]) fn(state);
  };

  return {
    get: () => state,
    set(next) {
      const value = typeof next === 'function' ? next(state) : next;
      if (isPlainObject(state) && isPlainObject(value)) {
        const merged = { ...state, ...value };
        if (!shallowEqual(merged, state)) commit(merged);
      } else {
        commit(value);
      }
    },
    replace: commit,
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
  };
}

const identity = (s) => s;

/**
 * Subscribe a component to a store. The component re-renders only when the
 * selected value changes (shallow comparison).
 */
export function useStore(store, selector = identity) {
  const selectorRef = useRef(selector);
  selectorRef.current = selector;

  const [selected, setSelected] = useState(() => selector(store.get()));
  const selectedRef = useRef(selected);

  useEffect(() => {
    const check = (state) => {
      const next = selectorRef.current(state);
      if (!shallowEqual(next, selectedRef.current)) {
        selectedRef.current = next;
        setSelected(() => next);
      }
    };
    // The store may have changed between render and effect.
    check(store.get());
    return store.subscribe(check);
  }, [store]);

  return selected;
}
