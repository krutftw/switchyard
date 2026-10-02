// Settings, Logging and usage tab: application logs, captured request
// bodies and the usage statistics.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { Notice, Panel } from '../../components/index.js';
import { NumberRow, OptionRow, Rows, SelectRow, SettingsForm, SwitchRow, useEdits, useSettingsSave } from './common.js';

const LEVELS = [
  { value: 'trace', label: 'Trace: everything, very noisy' },
  { value: 'debug', label: 'Debug: routing decisions and retries' },
  { value: 'info', label: 'Info: requests, reloads, failures' },
  { value: 'warn', label: 'Warn: problems only' },
  { value: 'error', label: 'Error: failures only' },
];

const CAPTURE = [
  { value: 'off', label: 'Off', description: 'No bodies are stored. The request list still shows model, status, timing and token counts.' },
  { value: 'errors', label: 'Failed requests', description: 'Stores the request and response bodies of requests that failed, for debugging.' },
  { value: 'all', label: 'Every request', description: 'Stores the request and response bodies of every request: prompts, completions, tool calls and attachments.' },
];

export function LoggingTab({ config, status }) {
  const live = config.data.config;
  const base = useMemo(() => ({ logging: live.logging, usage: live.usage }), [live]);
  const form = useEdits(base);
  const saver = useSettingsSave({ form, config, name: 'Logging and usage' });
  const { issues } = saver;

  const capture = form.value('logging.request_log');
  const fileLogs = !!form.value('logging.file');
  const usageOn = !!form.value('usage.enabled');
  const persist = !!form.value('usage.persist');
  const dataDir = status?.data_dir ?? null;
  // "<data dir>/logs", written with the separator the path already uses.
  const logDir = dataDir ? `${dataDir.replace(/[\\/]+$/, '')}${dataDir.includes('\\') ? '\\' : '/'}logs` : '<data_dir>/logs';

  return html`
    <${SettingsForm} form=${form} saver=${saver} what="logging and usage settings">
      <${Panel} title="Application log" description="What the gateway says about its own work. Shown live on the Logs page.">
        <${Rows}>
          <${SelectRow} form=${form} issues=${issues} path="logging.level" label="Level" options=${LEVELS} description="Lines below this level are dropped. Takes effect at once." />
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="logging.file"
            label="Write log files"
            description=${html`Also write rotating files under <span class="mono settings-break">${logDir}</span>. Off: the log is kept in memory only and is lost on restart.`}
          />
          <${NumberRow}
            form=${form}
            issues=${issues}
            path="logging.max_total_size_mb"
            label="Log files, total size"
            min=${1}
            unit="MB"
            disabled=${!fileLogs}
            description="The oldest log files are deleted when all of them together exceed this size."
          />
        <//>
      <//>

      <${Panel} title="Request bodies" description="Whether the content of requests and responses is kept for the request inspector.">
        <${Rows}>
          <${OptionRow}
            form=${form}
            issues=${issues}
            path="logging.request_log"
            label="Capture"
            description="For each captured request the gateway keeps what the client sent, what went upstream, and both answers."
            options=${CAPTURE}
          />
          <${NumberRow}
            form=${form}
            issues=${issues}
            path="logging.request_log_max_body_kb"
            label="Body size cap"
            min=${1}
            unit="KB"
            disabled=${capture === 'off'}
            description="Each captured body is cut off at this size."
          />
        <//>
        ${capture !== 'off' &&
        html`<${Notice} class="settings-inset" tone="caution" title=${capture === 'all' ? 'Everything your users send is stored in readable form' : 'Bodies of failed requests are stored in readable form'}>
          Credentials are redacted, but prompts, completions and pasted data are not. Anyone with the admin secret, or with access to ${dataDir ? html`<span class="mono settings-break">${dataDir}</span>` : 'the data directory'}, can read them. Tell the people who use this gateway, and turn capture off again when you are done debugging.
        <//>`}
      <//>

      <${Panel} title="Usage statistics" description="Token counts, costs and latency per model, provider and client key.">
        <${Rows}>
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="usage.enabled"
            label="Record usage"
            description="Off: nothing new is added to the Usage page. Requests are still served."
          />
          <${SwitchRow}
            form=${form}
            issues=${issues}
            path="usage.persist"
            label="Keep across restarts"
            disabled=${!usageOn}
            description="Write request records to the data directory and load them again on start. Off: statistics start from zero after every restart."
          />
          <${NumberRow}
            form=${form}
            issues=${issues}
            path="usage.retention_days"
            label="Retention"
            min=${1}
            unit="days"
            disabled=${!usageOn || !persist}
            description="Request records older than this are deleted."
          />
        <//>
      <//>
    <//>
  `;
}

// ui/tests/check.mjs asks every module under pages/ for a default export.
export default LoggingTab;
