// Sign-in: the admin secret, and a clear account of why it did not work.
//
// What the gateway can answer to POST /login (DESIGN.md section 11):
//   200  secret accepted
//   401  wrong secret
//   403  this address is not allowed (admin.allow_remote is off)
//   404  the admin API is off (no admin secret configured)
//   429  five wrong secrets in a row: locked out, Retry-After says for how long
//
// Two failures never reach the gateway's verdict: it cannot be reached
// (status 0), or the secret contains a control character and cannot be sent
// at all (code "invalid", shown on the field).
//
// The lockout lives in the gateway's memory, so only the gateway knows
// whether it still holds: a restart clears it at once. What this tab
// remembers of it (sessionStorage `sy.login.lockedUntil`, so a reload keeps
// the countdown) is a hint, never a reason to refuse input: the form stays
// usable, the countdown says when the gateway should accept a secret again,
// and the next answer decides. A 429 renews the hint from Retry-After; any
// other answer (accepted, wrong, refused) clears it. No answer at all (the
// gateway is unreachable, or nothing was sent) leaves it as it was.

import { html, useEffect, useState } from '../../vendor/preact-htm.js';
import { Button } from '../components/button.js';
import { Checkbox, Form, SecretInput } from '../components/form.js';
import { LogoMark } from '../components/icons.js';
import { Notice } from '../components/surface.js';
import { api, auth, isRemembered } from '../lib/api.js';
import { loadStyles } from '../lib/dom.js';
import { formatCountdown } from '../lib/format.js';
import { useNow } from '../lib/hooks.js';
import { useStore } from '../lib/store.js';
import { ThemeMenu } from '../shell/shell.js';

await loadStyles('pages/login.css');

const LOCK_KEY = 'sy.login.lockedUntil';
const DEFAULT_LOCK_SECONDS = 30 * 60;

function readLock() {
  try {
    const until = Number(sessionStorage.getItem(LOCK_KEY));
    return Number.isFinite(until) && until > Date.now() ? until : null;
  } catch {
    return null;
  }
}

function writeLock(until) {
  try {
    if (until) sessionStorage.setItem(LOCK_KEY, String(until));
    else sessionStorage.removeItem(LOCK_KEY);
  } catch {
    /* storage unavailable: the countdown lasts for this page load */
  }
}

/** Seconds left, in words that change once a minute (for screen readers). */
function lockWords(seconds) {
  if (seconds < 60) return 'less than a minute';
  const minutes = Math.round(seconds / 60);
  return minutes === 1 ? 'about 1 minute' : `about ${minutes} minutes`;
}

// htm drops the space at a line break inside running text, so prose with
// inline code is written on one line with this helper.
const code = (text) => html`<span class="mono">${text}</span>`;

// Characters a chat window, an e-mail client or a word processor likes to
// swap in for the plain ones. The names are for the message below.
const LOOKALIKES = {
  0x00a0: 'a non-breaking space',
  0x00ad: 'a soft hyphen',
  0x2010: 'a typographic hyphen',
  0x2011: 'a non-breaking hyphen',
  0x2012: 'a figure dash',
  0x2013: 'an en dash',
  0x2014: 'an em dash',
  0x2018: 'a curly quote',
  0x2019: 'a curly quote',
  0x201c: 'a curly quote',
  0x201d: 'a curly quote',
  0x200b: 'an invisible zero-width space',
  0x200c: 'an invisible zero-width character',
  0x200d: 'an invisible zero-width character',
  0x2060: 'an invisible zero-width character',
  0xfeff: 'an invisible zero-width character',
};

/**
 * The first character of `secret` outside printable ASCII, described, or
 * null. Such a secret is legal (it is sent as UTF-8), but when the gateway
 * turns it down the odd character is the first thing to check: secrets are
 * usually pasted, and pasting from a chat or a document changes characters.
 * Exported for tests.
 */
export function oddCharacter(secret) {
  for (const ch of String(secret)) {
    const point = ch.codePointAt(0);
    if (point >= 0x20 && point <= 0x7e) continue;
    const hex = `U+${point.toString(16).toUpperCase().padStart(4, '0')}`;
    return { hex, name: LOOKALIKES[point] ?? null };
  }
  return null;
}

/** Turn a failed sign-in into a title, an explanation and a tone. */
function explain(error, odd) {
  switch (error.status) {
    case 401:
      return {
        tone: 'stop',
        title: 'That secret was not accepted',
        body: odd
          ? html`What you entered contains ${odd.name ?? 'a character outside plain ASCII'} (${code(odd.hex)}). Copying from a chat or a document often replaces hyphens, quotes and spaces: if the real secret has none of these, type it by hand. Five wrong attempts in a row lock this address out for 30 minutes.`
          : html`Check it against ${code('SWITCHYARD_ADMIN_SECRET')} or ${code('admin.secret')} in ${code('switchyard.toml')}. Five wrong attempts in a row lock this address out for 30 minutes.`,
      };
    case 403:
      return {
        tone: 'caution',
        title: 'Remote access to the dashboard is off',
        body: html`This gateway only accepts admin connections from the machine it runs on. Open the dashboard there, or set ${code('admin.allow_remote = true')} (or ${code('SWITCHYARD_ADMIN_ALLOW_REMOTE=1')}) and restart.`,
      };
    case 404:
      return {
        tone: 'caution',
        title: 'The admin API is switched off',
        body: html`No admin secret is configured, so the gateway serves no admin routes. Set ${code('SWITCHYARD_ADMIN_SECRET')} or ${code('admin.secret')} and restart.`,
      };
    case 0:
      return {
        tone: 'stop',
        title: error.code === 'timeout' ? 'The gateway did not answer' : 'Cannot reach the gateway',
        body: error.code === 'timeout' ? 'It may be starting up or overloaded. Wait a moment and try again.' : 'Check that Switchyard is running and that this device can connect to it, then try again.',
      };
    default:
      return {
        tone: 'stop',
        title: error.status >= 500 ? 'The gateway hit an error' : 'Sign-in failed',
        body: `${error.message} (HTTP ${error.status})`,
      };
  }
}

/** The yard behind the form: parked tracks, and one route set to the platform. */
function Yard() {
  const tracks = [150, 270, 390, 510, 630];
  const switches = ['M120 390C250 390 250 270 380 270', 'M410 270C540 270 540 150 670 150', 'M300 390C430 390 430 510 560 510', 'M520 510C650 510 650 630 780 630'];
  const route = 'M-40 390H120C250 390 250 270 380 270H410C540 270 540 150 670 150H1240';
  return html`
    <svg class="yard" viewBox="0 0 1200 780" preserveAspectRatio="xMidYMid slice" aria-hidden="true" focusable="false">
      <g class="yard-tracks">
        ${tracks.map((y) => html`<path key=${y} d=${`M-40 ${y}H1240`} />`)}
        ${switches.map((d) => html`<path key=${d} d=${d} />`)}
      </g>
      <g class="yard-beds">
        ${tracks.map((y) => html`<path key=${y} d=${`M-40 ${y}H1240`} />`)}
        ${switches.map((d) => html`<path key=${d} d=${d} />`)}
      </g>
      <path class="yard-route" d=${route} pathLength="100" />
      <path class="yard-route-bed" d=${route} />
      <g class="yard-lamps">
        <circle class="yard-lamp" data-tone="clear" cx="104" cy="366" r="7" />
        <circle class="yard-lamp" data-tone="clear" cx="394" cy="246" r="7" />
        <circle class="yard-lamp" data-tone="stop" cx="286" cy="414" r="7" />
        <circle class="yard-lamp" data-tone="caution" cx="506" cy="534" r="7" />
      </g>
    </svg>
  `;
}

export default function Login() {
  const { reason, elsewhere } = useStore(auth);
  const [secret, setSecret] = useState('');
  const [remember, setRemember] = useState(isRemembered());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  // The odd character (if any) in the secret that `error` is about.
  const [odd, setOdd] = useState(null);
  const [fieldError, setFieldError] = useState(null);
  const [lockedUntil, setLockedUntil] = useState(readLock);
  const now = useNow();

  useEffect(() => {
    document.title = 'Sign in · Switchyard';
    // Not the autofocus attribute: browsers skip it when the URL has a fragment.
    document.getElementById('login-secret')?.focus();
  }, []);

  // A hint only (see the top of this file): it never disables the form.
  const lockSeconds = lockedUntil ? Math.ceil((lockedUntil - now) / 1000) : 0;
  const locked = lockSeconds > 0;

  // The countdown has run out: forget it.
  useEffect(() => {
    if (lockedUntil && !locked) {
      setLockedUntil(null);
      writeLock(null);
    }
  }, [lockedUntil, locked]);

  const submit = async () => {
    if (busy) return;
    const value = secret.trim();
    if (!value) {
      setFieldError('Enter the admin secret.');
      return;
    }
    setBusy(true);
    setError(null);
    setFieldError(null);
    try {
      await api.login(value, { remember });
      // Accepted: whatever this tab remembered of a lockout is over. The
      // auth store is now "authenticated"; app.js swaps in the shell.
      writeLock(null);
    } catch (cause) {
      if (cause.status === 429) {
        // Still locked out: the gateway says for how long.
        const until = Date.now() + (cause.retryAfter ?? DEFAULT_LOCK_SECONDS) * 1000;
        setLockedUntil(until);
        writeLock(until);
      } else {
        // Any other answer means the gateway is no longer locking this
        // address out (a restart, or the time is up). No answer (status 0:
        // unreachable, or nothing sent) tells nothing either way.
        if (cause.status > 0) {
          setLockedUntil(null);
          writeLock(null);
        }
        if (cause.code === 'invalid') {
          // Nothing was sent: the problem is in the field, so say it there.
          setFieldError(cause.message);
        } else {
          setOdd(oddCharacter(value));
          setError(cause);
        }
      }
      setBusy(false);
      // Ready for the next try: cursor in the field, old value selected.
      requestAnimationFrame(() => {
        const field = document.getElementById('login-secret');
        field?.focus();
        field?.select();
      });
    }
  };

  const problem = error ? explain(error, odd) : null;

  return html`
    <div class="login">
      <${Yard} />
      <div class="login-tools"><${ThemeMenu} /></div>
      <main class="login-main">
        <div class="login-card">
          <div class="login-brand">
            <${LogoMark} size=${28} draw />
            <span>Switchyard</span>
          </div>
          <div class="login-head">
            <h1>Sign in</h1>
            <p class="muted">Enter the admin secret of this gateway to open the dashboard.</p>
          </div>

          ${reason === 'expired' &&
          !error &&
          !locked &&
          html`<${Notice} tone="caution" title="Your session ended">The gateway no longer accepts the stored secret. It may have been changed. Sign in again.<//>`}
          ${reason === 'signed-out' &&
          !error &&
          !locked &&
          html`<${Notice} tone="info" title=${elsewhere ? 'Signed out in another tab' : 'Signed out'}>The secret was removed from this browser.<//>`}

          ${locked &&
          html`
            <${Notice} tone="caution" title="Too many wrong secrets" icon="lock">
              The gateway locked this address out after five wrong secrets in a row. It accepts a secret again in <strong class="num login-countdown" aria-hidden="true">${formatCountdown(lockSeconds)}</strong><span class="sr-only">${lockWords(lockSeconds)}</span>, or as soon as it restarts: a restart clears the lockout, so you can try again at any time.
            <//>
          `}
          ${problem && html`<${Notice} tone=${problem.tone} title=${problem.title}>${problem.body}<//>`}

          <${Form} onSubmit=${submit}>
            <input type="text" name="username" autocomplete="username" value="switchyard-admin" readonly hidden />
            <${SecretInput}
              label="Admin secret"
              name="password"
              size="lg"
              value=${secret}
              onChange=${(value) => {
                setSecret(value);
                if (fieldError) setFieldError(null);
              }}
              error=${fieldError}
              autocomplete="current-password"
              id="login-secret"
              readOnly=${busy}
            />
            <${Checkbox}
              label="Remember on this device"
              hint="Stores the secret in this browser so the next visit skips this page. Leave it off on a shared computer."
              checked=${remember}
              onChange=${setRemember}
              disabled=${busy}
            />
            <${Button} type="submit" variant="primary" size="lg" block loading=${busy}>Sign in<//>
          <//>

          <p class="login-help">
            Read the secret from ${code('admin.secret')} in ${code('switchyard.toml')}. When the gateway first creates that file in an interactive terminal, it also prints the secret there once.
          </p>
        </div>
      </main>
    </div>
  `;
}
