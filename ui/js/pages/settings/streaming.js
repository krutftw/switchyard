// Settings, Streaming and upstream tab: how streams are kept alive and how
// the gateway reaches providers.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { Panel } from '../../components/index.js';
import { NumberRow, Rows, SecondsRow, SettingsForm, SwitchRow, TextRow, useEdits, useSettingsSave } from './common.js';

const PROXY_FORMS = [
  ['http://host:port', 'An HTTP proxy. https:// works the same way.'],
  ['socks5://host:port', 'A SOCKS5 proxy; names are resolved by the gateway.'],
  ['socks5h://host:port', 'A SOCKS5 proxy that also resolves the names.'],
  ['direct', 'Never use a proxy, whatever the environment says.'],
  ['empty', 'Follow HTTP_PROXY, HTTPS_PROXY and NO_PROXY from the environment.'],
];

function ProxyForms() {
  return html`
    <dl class="settings-forms">
      ${PROXY_FORMS.map(
        ([form, meaning]) => html`
          <div class="settings-forms-row" key=${form}>
            <dt class=${form === 'empty' ? undefined : 'mono'}>${form}</dt>
            <dd>${meaning}</dd>
          </div>
        `,
      )}
    </dl>
  `;
}

export function StreamingTab({ config, onDiskInvalid }) {
  const live = config.data.config;
  const base = useMemo(() => ({ streaming: live.streaming, upstream: live.upstream }), [live]);
  const form = useEdits(base);
  const saver = useSettingsSave({ form, config, name: 'Streaming and upstream', onDiskInvalid });
  const { issues } = saver;

  const proxy = String(form.value('upstream.proxy') ?? '');
  // The gateway masks the password of a stored proxy URL: a long one as
  // "sec…23", a short one as bullets. Sent back like that, it is kept.
  const masked = /^[a-z][a-z0-9+.-]*:\/\/[^/?#]*[…•][^/?#]*@/i.test(proxy);

  return html`
    <${SettingsForm} form=${form} saver=${saver} what="streaming and upstream settings">
      <${Panel} title="Streaming" description="Server-sent events and WebSocket connections to clients.">
        <${Rows}>
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="streaming.keepalive_secs"
            label="Keep-alive interval"
            zero="Off: nothing is sent while the model is silent."
            description="During silence the gateway sends an SSE comment or a WebSocket ping this often, so proxies and clients do not drop the connection."
          />
          <${NumberRow}
            form=${form}
            issues=${issues}
            path="streaming.bootstrap_retries"
            label="Retries before the first byte"
            min=${0}
            hint=${form.value('streaming.bootstrap_retries') === 0 ? 'Off: a stream that fails before it starts is returned as an error.' : undefined}
            description="A stream that fails before anything reached the client is started again on another credential, up to this many extra times. After the first event there is no retry."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="streaming.idle_timeout_secs"
            label="Idle timeout"
            zero="Off: a silent stream is never cut."
            description="A stream is ended with an error when the upstream sends nothing for this long."
          />
        <//>
      <//>

      <${Panel} title="Upstream connections" description="How the gateway reaches providers.">
        <${Rows}>
          <${TextRow}
            form=${form}
            issues=${issues}
            path="upstream.proxy"
            label="Proxy"
            mono
            placeholder="Empty: use the environment"
            hint=${masked ? 'The password is stored and shown masked. Leave the masked part as it is to keep the password; type over it to set a new one.' : undefined}
            description=${html`Used for every provider that does not set its own proxy. Accepted forms:<${ProxyForms} />`}
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="upstream.connect_timeout_secs"
            label="Connect timeout"
            description="How long to wait for the TCP and TLS handshake with a provider before the attempt counts as failed."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="upstream.request_timeout_secs"
            label="Request timeout"
            description="The longest a non-streaming request may take from sending it to receiving the full answer."
          />
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="upstream.passthrough_headers"
            label="Forward provider headers"
            description="Pass the provider's rate-limit and request-id response headers on to the client."
          />
        <//>
      <//>
    <//>
  `;
}
