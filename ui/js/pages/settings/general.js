// Settings, General tab: the listener, client authentication and the admin
// interface itself. Everything here is saved with PATCH /settings.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { Button, Notice, Panel, SecretInput } from '../../components/index.js';
import { NumberRow, Rows, SettingRow, SettingsForm, SwitchRow, TextRow, expectSecret, fieldId, forgetSecret, isSecretReference, useEdits, useSettingsSave } from './common.js';

/** 32 URL-safe characters from the browser's random source. */
function generateSecret() {
  const bytes = crypto.getRandomValues(new Uint8Array(24));
  let text = '';
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function GeneralTab({ config, status, onDiskInvalid }) {
  const live = config.data.config;
  // The edited subset. An unset [server.tls] reads as two empty fields, and
  // the secret field starts empty: the gateway only ever sends its mask.
  const base = useMemo(
    () => ({
      server: { ...live.server, tls: { cert: live.server.tls?.cert ?? '', key: live.server.tls?.key ?? '' } },
      auth: { required: live.auth?.required ?? true },
      admin: { ...live.admin, secret: '' },
    }),
    [live],
  );
  const form = useEdits(base);

  const listen = status?.listen ?? null;
  const viaRemote = status?.admin?.remote === true;
  const cert = String(form.value('server.tls.cert') ?? '').trim();
  const key = String(form.value('server.tls.key') ?? '').trim();
  const newSecret = form.value('admin.secret') ?? '';
  // "env:NAME" or "${NAME}": the gateway reads the secret from its own environment.
  const secretIsReference = isSecretReference(newSecret);

  const saver = useSettingsSave({
    form,
    config,
    name: 'General',
    onDiskInvalid,
    check: () => {
      const problems = [];
      if (cert && !key) problems.push({ path: 'server.tls.key', message: 'Enter the key file too, or clear the certificate to serve plain HTTP.' });
      if (key && !cert) problems.push({ path: 'server.tls.cert', message: 'Enter the certificate file too, or clear the key to serve plain HTTP.' });
      // The gateway would take such a secret, and no browser could send it.
      if (/[\u0000-\u0008\u000a-\u001f\u007f]/.test(newSecret)) {
        problems.push({ path: 'admin.secret', message: 'The secret cannot contain line breaks or other control characters. Enter it as a single line.' });
      }
      return problems;
    },
    toPatch: (patch) => {
      // [server.tls] is all or nothing: both paths, or the table removed.
      if (patch.server?.tls) patch.server.tls = cert || key ? { cert, key } : null;
      return patch;
    },
    risks: (patch) => {
      const lines = [];
      if (patch.auth?.required === false) {
        lines.push(`Client authentication is switched off: anyone who can reach ${listen ?? 'the gateway'} can use your providers without a key.`);
      }
      if (patch.admin?.allow_remote === true) lines.push('The dashboard and the admin API will accept sign-ins from other machines.');
      if (patch.admin?.allow_remote === false && viaRemote) lines.push('You are connected from another machine. This session is refused as soon as the change is saved.');
      if (patch.admin?.ui === false) lines.push('The dashboard stops being served. This page works until you reload it; after that, set admin.ui = true in switchyard.toml to get it back.');
      if (patch.admin?.secret && secretIsReference) {
        lines.push("The admin secret becomes the value of that variable in the gateway's environment. This page cannot read it: this browser, like every other browser and script, has to sign in again with that value.");
      } else if (patch.admin?.secret) {
        lines.push('The admin secret changes at once. Other browsers and scripts must sign in again with the new secret.');
      }
      return lines;
    },
    prepare: (patch) => {
      // A reference cannot be followed from here: the session ends, and the
      // sign-in page asks for the variable's value.
      if (!patch.admin?.secret) return null;
      if (secretIsReference) return { done: forgetSecret };
      const change = expectSecret(patch.admin.secret);
      return {
        failed: change.cancel,
        done: async () =>
          (await change.adopt())
            ? 'This browser stays signed in with the new secret.'
            : 'The gateway did not accept the new secret: SWITCHYARD_ADMIN_SECRET is set in its environment and overrides the file. The current secret stays in effect.',
      };
    },
  });
  const { issues } = saver;

  const authOff = form.value('auth.required') === false;
  const remoteOn = form.value('admin.allow_remote') === true;
  const uiOff = form.value('admin.ui') === false;
  const secretId = fieldId('admin.secret');

  return html`
    <${SettingsForm} form=${form} saver=${saver} what="general settings">
      <${Panel} title="Server" description="Where the gateway listens and what it accepts.">
        <${Rows}>
          <${TextRow}
            form=${form}
            issues=${issues}
            path="server.host"
            label="Host"
            restart
            mono
            placeholder="127.0.0.1"
            description=${html`The address to bind. <span class="mono">127.0.0.1</span> accepts connections from this machine only; <span class="mono">0.0.0.0</span> accepts them from other machines too.${listen ? html` Listening on <span class="mono">${listen}</span> now.` : null}`}
          />
          <${NumberRow} form=${form} issues=${issues} path="server.port" label="Port" restart min=${1} max=${65535} description="The TCP port for the client API and this dashboard. A port given on the command line wins over this value." />
          <${TextRow}
            form=${form}
            issues=${issues}
            path="server.data_dir"
            label="Data directory"
            restart
            mono
            placeholder="data"
            description=${html`Usage history, request logs and log files. A relative path starts from the folder of switchyard.toml.${status?.data_dir ? html` In use now: <span class="mono settings-break">${status.data_dir}</span>.` : null}`}
          />
          <${TextRow}
            form=${form}
            issues=${issues}
            path="server.tls.cert"
            label="TLS certificate"
            restart
            mono
            placeholder="cert.pem"
            error=${issues.at('server.tls.cert') ?? issues.at('server.tls')}
            description=${`Serve HTTPS directly from this PEM certificate. Leave certificate and key empty to serve plain HTTP, for example behind a reverse proxy.${typeof status?.tls === 'boolean' ? ` The listener serves ${status.tls ? 'HTTPS' : 'plain HTTP'} now.` : ''}`}
          />
          <${TextRow} form=${form} issues=${issues} path="server.tls.key" label="TLS private key" restart mono placeholder="key.pem" description="The PEM private key that belongs to the certificate." />
          <${NumberRow} form=${form} issues=${issues} path="server.body_limit_mb" label="Request body limit" min=${1} unit="MB" description="The largest request body the client API accepts. Larger requests are answered with 413." />
          <${SwitchRow} form=${form} issues=${issues} path="server.cors" label="Allow browser apps (CORS)" description="Lets web pages on other origins call the client API directly. Turn it off when only servers and command-line tools use the gateway." />
        <//>
      <//>

      <${Panel} title="Client authentication" description="Who may send requests to the client API.">
        <${Rows}>
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="auth.required"
            label="Require a client key"
            description=${html`Requests must carry one of the keys on the <a href="#/keys">API keys</a> page. Turn this off only on a machine nobody else can reach.`}
          />
        <//>
        ${authOff &&
        html`<${Notice} class="settings-inset" tone="caution" title="Without this, the gateway is open">
          Anyone who can reach ${listen ? html`<span class="mono">${listen}</span>` : 'the listener'} can spend your provider credentials, and requests are not attributed to a key. Rate limits and model allow-lists of client keys no longer apply to requests sent without one.
        <//>`}
      <//>

      <${Panel} title="Admin interface" description="This dashboard and the API behind it.">
        <${Rows}>
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="admin.allow_remote"
            label="Allow remote access"
            description=${html`Off: only connections from this machine (<span class="mono">127.0.0.1</span>, <span class="mono">::1</span>) may use the dashboard and the admin API. A request relayed by a reverse proxy on this machine counts as remote. On: any address that can reach the listener may sign in with the admin secret.`}
          />
          <${SwitchRow} form=${form} issues=${issues} path="admin.ui" label="Serve the dashboard" description=${html`Off: <span class="mono">/admin/</span> answers 404. The admin API keeps working for scripts.`} />
          <${SettingRow}
            id=${secretId}
            label="Admin secret"
            changed=${form.changed('admin.secret')}
            description=${html`Signs in to this dashboard and the admin API. ${live.admin?.secret ? html`The secret in switchyard.toml is <span class="mono">${live.admin.secret}</span>.` : 'switchyard.toml sets no secret.'} If <span class="mono settings-break">SWITCHYARD_ADMIN_SECRET</span> is set in the gateway's environment, that value is the one in effect and this one is ignored.`}
          >
            <div class="stack" style="--gap:var(--space-2)">
              <${SecretInput}
                id=${secretId}
                value=${newSecret}
                onChange=${form.set('admin.secret')}
                copy
                placeholder="New secret"
                error=${issues.at('admin.secret')}
                warning=${newSecret && !secretIsReference && newSecret.length < 16 ? 'Short secrets are easy to guess. Use 16 characters or more.' : undefined}
                hint=${!newSecret ? 'Leave empty to keep the current secret.' : secretIsReference ? 'A reference: the gateway reads the secret from that variable in its own environment.' : 'Copy it somewhere safe before saving. It is shown masked afterwards.'}
              />
              <div><${Button} size="sm" icon="refresh" onClick=${() => form.set('admin.secret')(generateSecret())}>Generate a secret<//></div>
            </div>
          <//>
        <//>
        ${remoteOn &&
        form.changed('admin.allow_remote') &&
        html`<${Notice} class="settings-inset" tone="caution" title="The admin secret becomes the only lock">
          Use a long secret and serve HTTPS before exposing the admin interface to a network. Five wrong secrets in a row lock an address out for 30 minutes.
        <//>`}
        ${!remoteOn &&
        form.changed('admin.allow_remote') &&
        viaRemote &&
        html`<${Notice} class="settings-inset" tone="stop" title="This would lock you out">You are connected from another machine. After saving, the gateway refuses this session; you can only sign in again from the machine it runs on.<//>`}
        ${uiOff &&
        html`<${Notice} class="settings-inset" tone="caution" title="The dashboard will not load again">
          This page keeps working until you reload it. To get the dashboard back, set <span class="mono">admin.ui = true</span> in switchyard.toml.
        <//>`}
        ${form.changed('admin.secret') &&
        html`<${Notice} class="settings-inset" tone="info" title="Changing the secret">
          ${secretIsReference
            ? "The variable's value becomes the secret the moment this is saved. Every session, this one included, has to sign in again with it."
            : 'The new secret takes effect the moment it is saved. This browser stays signed in; every other session and script has to use the new secret.'}
        <//>`}
      <//>
    <//>
  `;
}
