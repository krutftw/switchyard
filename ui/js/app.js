// Entry point: boot, auth gate, and the hand-off to the shell.
//
//   unknown        a stored secret is being checked  -> splash
//   anonymous      no (valid) secret                 -> sign-in page
//   authenticated                                    -> shell + routed pages
//
// The auth store lives in lib/api.js; any request that gets a 401 flips it
// back to "anonymous", which brings the sign-in page back with a reason.

import { html, render, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { Button, Spinner } from './components/button.js';
import { LogoMark } from './components/icons.js';
import { Notice } from './components/surface.js';
import { api, auth } from './lib/api.js';
import './lib/live.js'; // starts and stops the live connection with the session
import { useStore } from './lib/store.js';
import './lib/theme.js'; // applies the theme and follows the OS
import { Shell } from './shell/shell.js';

function Splash({ children }) {
  return html`
    <div class="boot">
      <div class="boot-inner">
        <div class="boot-brand">
          <${LogoMark} size=${32} />
          <span>Switchyard</span>
        </div>
        ${children}
      </div>
    </div>
  `;
}

/** Loads the sign-in page on demand: signed-in sessions never fetch it. */
function SignIn() {
  const [state, setState] = useState({ Component: null, error: null });
  useEffect(() => {
    let alive = true;
    import('./pages/login.js')
      .then((module) => alive && setState({ Component: module.default, error: null }))
      .catch((error) => {
        console.error('Could not load the sign-in page', error);
        if (alive) setState({ Component: null, error });
      });
    return () => {
      alive = false;
    };
  }, []);
  if (state.error) {
    return html`
      <${Splash}>
        <${Notice} tone="stop" title="Could not load the sign-in page" action=${html`<${Button} size="sm" onClick=${() => location.reload()}>Reload<//>`}>
          A file did not arrive from the gateway. Check the connection and reload.
        <//>
      <//>
    `;
  }
  if (!state.Component) return html`<${Splash} />`;
  const Login = state.Component;
  return html`<${Login} />`;
}

function App() {
  const { status } = useStore(auth);
  const [bootError, setBootError] = useState(null);
  const [attempt, setAttempt] = useState(0);
  // Something the user could work in (the sign-in form, the boot error with
  // its buttons) has been on screen: when the shell replaces it, the focused
  // control goes with it, and the shell takes the focus.
  const handOff = useRef(false);
  if (status === 'anonymous' || bootError) handOff.current = true;

  // A stored secret is checked once at start. A gateway that cannot be
  // reached is not a reason to throw the secret away: offer a retry.
  useEffect(() => {
    if (auth.get().status !== 'unknown') return;
    setBootError(null);
    api.resume().catch((error) => setBootError(error));
  }, [attempt]);

  if (status === 'authenticated') return html`<${Shell} takeFocus=${handOff.current} />`;
  if (status === 'anonymous') return html`<${SignIn} />`;

  if (bootError) {
    return html`
      <${Splash}>
        <${Notice} tone="stop" title="Cannot reach the gateway">
          ${bootError.message}
        <//>
        <div class="btn-group">
          <${Button} variant="primary" icon="refresh" onClick=${() => setAttempt((n) => n + 1)}>Try again<//>
          <${Button} onClick=${() => api.logout({ allTabs: false })}>Use a different secret<//>
        </div>
      <//>
    `;
  }
  return html`
    <${Splash}>
      <div class="boot-status" role="status"><${Spinner} /> Checking the stored session</div>
    <//>
  `;
}

const root = document.getElementById('app');
// Drop the static splash from index.html; Preact renders into an empty node.
root.textContent = '';
render(html`<${App} />`, root);
