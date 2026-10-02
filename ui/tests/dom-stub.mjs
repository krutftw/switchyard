// A document just big enough to render the dashboard's components under
// Node, so their behaviour (not only their pure helpers) can be asserted
// without a browser. Used by dom.mjs.
//
// What it has: a node tree, attributes, events with capture and bubbling,
// focus, a small selector matcher, and fixed layout numbers. What it does not
// have: CSS, real layout, painting. Anything about how a component looks
// still has to be checked in a browser.
//
// installDom() puts document, window and friends on globalThis and returns
// helpers. Import the modules under test BEFORE calling it: they are written
// to load without a document, and this keeps their import-time side effects
// (theme, live connection) out of the tests.

class Node {
  constructor(nodeType) {
    this.nodeType = nodeType;
    this.parentNode = null;
    this.childNodes = [];
    this.listeners = new Map();
  }

  get firstChild() {
    return this.childNodes[0] ?? null;
  }

  get nextSibling() {
    const siblings = this.parentNode?.childNodes;
    return siblings ? (siblings[siblings.indexOf(this) + 1] ?? null) : null;
  }

  appendChild(node) {
    return this.insertBefore(node, null);
  }

  insertBefore(node, before) {
    node.parentNode?.removeChild(node);
    const at = before ? this.childNodes.indexOf(before) : -1;
    if (before && at === -1) throw new Error('insertBefore: the reference node is not a child');
    if (at === -1) this.childNodes.push(node);
    else this.childNodes.splice(at, 0, node);
    node.parentNode = this;
    return node;
  }

  removeChild(node) {
    const at = this.childNodes.indexOf(node);
    if (at === -1) throw new Error('removeChild: not a child');
    this.childNodes.splice(at, 1);
    node.parentNode = null;
    return node;
  }

  remove() {
    this.parentNode?.removeChild(this);
  }

  contains(other) {
    for (let node = other; node; node = node.parentNode) if (node === this) return true;
    return false;
  }

  addEventListener(type, fn, capture = false) {
    const key = `${type}|${capture === true || capture?.capture === true}`;
    if (!this.listeners.has(key)) this.listeners.set(key, new Set());
    this.listeners.get(key).add(fn);
  }

  removeEventListener(type, fn, capture = false) {
    this.listeners.get(`${type}|${capture === true || capture?.capture === true}`)?.delete(fn);
  }

  get textContent() {
    return this.nodeType === 3 ? this.data : this.childNodes.map((child) => child.textContent).join('');
  }
}

class Text extends Node {
  constructor(data) {
    super(3);
    this.data = String(data);
  }
}

// ---- selectors: tag, .class, #id, [attr], [attr="v"], :not(...), :disabled,
// descendant combinator, comma lists --------------------------------------

function matchesCompound(el, compound) {
  let rest = compound;
  const tag = /^(\*|[a-zA-Z][\w-]*)/.exec(rest);
  if (tag) {
    if (tag[1] !== '*' && el.localName !== tag[1].toLowerCase()) return false;
    rest = rest.slice(tag[0].length);
  }
  while (rest) {
    let m;
    if ((m = /^\.([\w-]+)/.exec(rest))) {
      if (!(el.getAttribute('class') ?? '').split(/\s+/).includes(m[1])) return false;
    } else if ((m = /^#([\w-]+)/.exec(rest))) {
      if (el.getAttribute('id') !== m[1]) return false;
    } else if ((m = /^\[([\w-]+)(?:=(?:"([^"]*)"|'([^']*)'|([^\]]*)))?\]/.exec(rest))) {
      const have = el.getAttribute(m[1]);
      if (have === null) return false;
      const want = m[2] ?? m[3] ?? m[4];
      if (want !== undefined && have !== want) return false;
    } else if ((m = /^:not\(([^()]*)\)/.exec(rest))) {
      if (matchesCompound(el, m[1])) return false;
    } else if ((m = /^:disabled/.exec(rest))) {
      if (!el.hasAttribute('disabled')) return false;
    } else {
      throw new Error(`dom-stub: unsupported selector "${compound}"`);
    }
    rest = rest.slice(m[0].length);
  }
  return true;
}

function matchesSelector(el, selector) {
  return selector.split(',').some((alternative) => {
    const parts = alternative.trim().split(/\s+/);
    if (!matchesCompound(el, parts[parts.length - 1])) return false;
    let node = el.parentNode;
    for (let i = parts.length - 2; i >= 0; i -= 1) {
      while (node && !(node.nodeType === 1 && matchesCompound(node, parts[i]))) node = node.parentNode;
      if (!node) return false;
      node = node.parentNode;
    }
    return true;
  });
}

const VALUE_TAGS = new Set(['input', 'textarea', 'select']);

class Element extends Node {
  constructor(localName, document) {
    super(1);
    this.localName = localName;
    this.tagName = localName.toUpperCase();
    this.ownerDocument = document;
    this.attrs = new Map();
    this.style = { cssText: '', setProperty(name, value) { this[name] = value; } };
    // Layout does not exist here; every element is "visible" and 10x10.
    this.offsetWidth = 10;
    this.offsetHeight = 10;
    this.clientWidth = 10;
    this.clientHeight = 10;
    this.scrollHeight = 10;
    // Form controls carry their value as a property, as in a browser.
    if (VALUE_TAGS.has(localName)) this.value = '';
    if (localName === 'input') this.checked = false;
  }

  get id() {
    return this.getAttribute('id') ?? '';
  }

  set id(value) {
    this.setAttribute('id', value);
  }

  get className() {
    return this.getAttribute('class') ?? '';
  }

  set className(value) {
    this.setAttribute('class', value);
  }

  get disabled() {
    return this.hasAttribute('disabled');
  }

  set disabled(value) {
    if (value) this.setAttribute('disabled', '');
    else this.removeAttribute('disabled');
  }

  getAttribute(name) {
    return this.attrs.has(name) ? this.attrs.get(name) : null;
  }

  setAttribute(name, value) {
    this.attrs.set(name, String(value));
  }

  removeAttribute(name) {
    this.attrs.delete(name);
  }

  hasAttribute(name) {
    return this.attrs.has(name);
  }

  get children() {
    return this.childNodes.filter((node) => node.nodeType === 1);
  }

  get firstElementChild() {
    return this.children[0] ?? null;
  }

  get isContentEditable() {
    return this.getAttribute('contenteditable') === 'true';
  }

  matches(selector) {
    // No layout, no input modality: keyboard focus is assumed.
    if (selector === ':focus-visible') return this.ownerDocument.activeElement === this;
    return matchesSelector(this, selector);
  }

  querySelectorAll(selector) {
    const out = [];
    const walk = (node) => {
      for (const child of node.childNodes) {
        if (child.nodeType !== 1) continue;
        if (matchesSelector(child, selector)) out.push(child);
        walk(child);
      }
    };
    walk(this);
    return out;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }

  getBoundingClientRect() {
    return { top: 100, left: 100, right: 110, bottom: 110, width: 10, height: 10, x: 100, y: 100 };
  }

  focus() {
    if (this.hasAttribute('disabled')) return;
    const document = this.ownerDocument;
    const previous = document.activeElement;
    if (previous === this) return;
    if (previous && previous !== document.body) dispatch(previous, 'blur', { bubbles: false });
    document.activeElement = this;
    dispatch(this, 'focus', { bubbles: false });
  }

  blur() {
    const document = this.ownerDocument;
    if (document.activeElement !== this) return;
    dispatch(this, 'blur', { bubbles: false });
    document.activeElement = document.body;
  }

  scrollIntoView() {}

  select() {}

  click() {
    if (this.hasAttribute('disabled')) return;
    dispatch(this, 'click', { detail: 1 });
  }
}

// Preact decides between "click" and "Click" by asking whether "onclick" is
// a property of the element, so the handler properties have to exist.
for (const type of [
  'click', 'dblclick', 'keydown', 'keyup', 'input', 'change', 'submit', 'focus', 'blur', 'paste', 'scroll', 'wheel',
  'pointerdown', 'pointerup', 'pointermove', 'pointerenter', 'pointerleave', 'mousedown', 'mouseup', 'mouseenter', 'mouseleave', 'touchstart',
]) {
  Element.prototype[`on${type}`] = null;
}

class Document extends Node {
  constructor() {
    super(9);
    this.documentElement = new Element('html', this);
    this.documentElement.clientWidth = 1280;
    this.documentElement.dataset = {};
    this.head = new Element('head', this);
    this.body = new Element('body', this);
    this.appendChild(this.documentElement);
    this.documentElement.appendChild(this.head);
    this.documentElement.appendChild(this.body);
    this.activeElement = this.body;
    this.visibilityState = 'visible';
    this.baseURI = 'http://localhost/admin/';
    this.title = '';
  }

  createElement(name) {
    return new Element(String(name).toLowerCase(), this);
  }

  createElementNS(_namespace, name) {
    return new Element(String(name), this);
  }

  createTextNode(data) {
    return new Text(data);
  }

  getElementById(id) {
    return this.documentElement.querySelector(`#${id}`);
  }

  querySelector(selector) {
    return this.documentElement.querySelector(selector);
  }

  querySelectorAll(selector) {
    return this.documentElement.querySelectorAll(selector);
  }
}

/**
 * Dispatch an event the way a browser does: capture listeners from the top
 * down, then (when it bubbles) the others from the target up, ending at the
 * document and the window. Returns the event, so tests can read
 * defaultPrevented.
 */
export function dispatch(target, type, init = {}) {
  const event = {
    type,
    target,
    currentTarget: null,
    bubbles: true,
    defaultPrevented: false,
    isComposing: false,
    detail: 0,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    shiftKey: false,
    stopped: false,
    preventDefault() {
      this.defaultPrevented = true;
    },
    stopPropagation() {
      this.stopped = true;
    },
    ...init,
  };
  const path = [];
  for (let node = target; node; node = node.parentNode) path.push(node);
  if (globalThis.window && path[path.length - 1]?.nodeType === 9) path.push(globalThis.window);

  const run = (node, capture) => {
    const fns = node.listeners.get(`${type}|${capture}`);
    if (!fns) return;
    event.currentTarget = node;
    for (const fn of [...fns]) fn.call(node, event);
  };
  for (let i = path.length - 1; i >= 0 && !event.stopped; i -= 1) run(path[i], true);
  for (let i = 0; i < path.length && !event.stopped; i += 1) {
    run(path[i], false);
    if (!event.bubbles) break;
  }
  return event;
}

function memoryStorage() {
  const map = new Map();
  return {
    getItem: (key) => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => void map.set(key, String(value)),
    removeItem: (key) => void map.delete(key),
    clear: () => map.clear(),
  };
}

/**
 * Preact reads `typeof requestAnimationFrame` once, when its module loads,
 * to decide how effects are scheduled; without it every effect waits 100 ms.
 * Call this before the first import of vendor/preact-htm.js. It defines
 * nothing that the dashboard's modules look at while loading.
 */
export function installFrameClock() {
  globalThis.requestAnimationFrame ??= (fn) => setTimeout(() => fn(Date.now()), 1);
  globalThis.cancelAnimationFrame ??= (id) => clearTimeout(id);
}

/** Install the globals. Returns { document, window, dispatch, text, sleep, until }. */
export function installDom() {
  installFrameClock();
  const document = new Document();
  const window = new Node(0);
  Object.assign(window, {
    document,
    innerWidth: 1280,
    innerHeight: 800,
    isSecureContext: false,
    scrollTo() {},
  });
  const define = (name, value) => Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
  define('document', document);
  define('window', window);
  define('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
  define('getComputedStyle', () => ({ lineHeight: '20px' }));
  define('localStorage', memoryStorage());
  define('sessionStorage', memoryStorage());
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  return {
    document,
    window,
    dispatch,
    /** Visible text of a node, white space collapsed. */
    text: (node) => node.textContent.replace(/\s+/g, ' ').trim(),
    sleep,
    /** Wait until `check()` is truthy; fails with `what` after `timeoutMs`. */
    async until(check, what = 'condition', timeoutMs = 2000) {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const value = check();
        if (value) return value;
        if (Date.now() > deadline) throw new Error(`timed out waiting for: ${what}`);
        await sleep(5);
      }
    },
  };
}
