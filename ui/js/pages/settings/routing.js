// Settings, Routing tab: how a credential is picked, how often a request is
// retried, and how long a failing credential rests.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { Panel } from '../../components/index.js';
import { NumberRow, OptionRow, Rows, SecondsRow, SettingsForm, SwitchRow, useEdits, useSettingsSave } from './common.js';

const STRATEGIES = [
  { value: 'round-robin', label: 'Round robin', description: 'Take turns: each request goes to the next ready credential.' },
  { value: 'fill-first', label: 'Fill first', description: 'Stay on the first credential until it is rate limited or fails, then move to the next.' },
  { value: 'weighted', label: 'Weighted', description: 'Share requests in proportion to the weight set on each credential.' },
  { value: 'least-latency', label: 'Least latency', description: 'Prefer the credential that has been answering fastest.' },
];

export function RoutingTab({ config, onDiskInvalid }) {
  const live = config.data.config;
  const base = useMemo(() => ({ routing: live.routing }), [live]);
  const form = useEdits(base);
  const saver = useSettingsSave({ form, config, name: 'Routing', onDiskInvalid });
  const { issues } = saver;

  const affinity = !!form.value('routing.session_affinity');
  const cooling = !!form.value('routing.cooldown.enabled');
  const attempts = form.value('routing.max_attempts');

  return html`
    <${SettingsForm} form=${form} saver=${saver} what="routing settings">
      <${Panel} title="Picking a credential" description="Applies within a provider's pool, among the credentials that are ready.">
        <${Rows}>
          <${OptionRow}
            form=${form}
            issues=${issues}
            path="routing.strategy"
            label="Strategy"
            description="Providers with a higher priority are always tried first; the strategy decides among credentials of equal priority."
            options=${STRATEGIES}
          />
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="routing.session_affinity"
            label="Session affinity"
            description="Keep a conversation on the credential that served it before, so the provider's prompt cache is hit. The strategy applies to the first request of a conversation only."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="routing.session_affinity_ttl_secs"
            label="Affinity lifetime"
            disabled=${!affinity}
            description="How long after its last request a conversation stays tied to its credential."
          />
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="routing.force_model_prefix"
            label="Require the provider prefix"
            description=${html`On: a provider with a prefix only answers to the prefixed name (<span class="mono">or/model</span>). Off: the bare model name reaches it too.`}
          />
        <//>
      <//>

      <${Panel} title="Retries" description="What happens when an upstream attempt fails.">
        <${Rows}>
          <${NumberRow}
            form=${form}
            issues=${issues}
            path="routing.max_attempts"
            label="Attempts per request"
            min=${1}
            hint=${attempts === 1 ? 'No failover: the first failure is returned to the client.' : undefined}
            description="Upstream attempts for one client request, counted across credentials and providers. 1 turns failover off."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="routing.max_wait_secs"
            label="Wait for a cooling credential"
            zero="Does not wait: the request fails at once."
            description="When every credential is cooling down, wait up to this long for the first one to come back instead of failing."
          />
        <//>
      <//>

      <${Panel} title="Cooldowns" description="How long a credential rests after a failure before it is tried again. An upstream Retry-After always wins.">
        <${Rows}>
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="routing.cooldown.enabled"
            label="Cool down failing credentials"
            description="Off: a failing credential stays in rotation and is tried again on the next request."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="routing.cooldown.rate_limit_base_secs"
            label="Rate limit, first cooldown"
            disabled=${!cooling}
            description="After the first 429 from a credential. Doubles with each consecutive rate limit."
          />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="routing.cooldown.rate_limit_max_secs"
            label="Rate limit, cap"
            disabled=${!cooling}
            description="The doubling stops here. It cannot be shorter than the first cooldown."
          />
          <${SecondsRow} form=${form} issues=${issues} path="routing.cooldown.transient_secs" label="Server and network errors" disabled=${!cooling} description="After a 5xx answer, a timeout or a connection error." />
          <${SecondsRow} form=${form} issues=${issues} path="routing.cooldown.auth_secs" label="Rejected credential" disabled=${!cooling} description="After the upstream answers 401 or 403: the key is wrong, revoked or lacks access." />
          <${SecondsRow} form=${form} issues=${issues} path="routing.cooldown.quota_secs" label="Out of quota" disabled=${!cooling} description="After a billing or quota failure, which rarely clears within minutes." />
          <${SecondsRow}
            form=${form}
            issues=${issues}
            path="routing.cooldown.model_not_found_secs"
            label="Model not found"
            disabled=${!cooling}
            description="After the upstream says it does not have the model. Applies to that model on that credential only."
          />
        <//>
      <//>
    <//>
  `;
}
