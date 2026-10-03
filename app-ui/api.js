export const TOKEN_STORAGE_KEY = 'switchyard.hostToken';

export class ApiError extends Error {
  constructor(code, message, { status = 0, uncertain = false, cause } = {}) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
    this.uncertain = uncertain;
  }
}

function browserSessionStorage() {
  try { return globalThis.sessionStorage; } catch { return null; }
}

function validToken(value) {
  return typeof value === 'string' && value.length > 0 && value.length <= 4096
    && !/[^\x21-\x7e]/.test(value);
}

/** Consume the launch fragment before retaining its token for this tab's reloads. */
export function bootstrapToken({
  location = globalThis.location,
  history = globalThis.history,
  storage = browserSessionStorage(),
} = {}) {
  const params = new URLSearchParams((location?.hash ?? '').replace(/^#/, ''));
  if (params.has('token')) {
    const tokens = params.getAll('token');
    const token = tokens.length === 1 && validToken(tokens[0]) ? tokens[0] : '';
    params.delete('token');
    const remaining = params.toString();
    const cleanUrl = `${location?.pathname || '/'}${location?.search || ''}${remaining ? `#${remaining}` : ''}`;
    try {
      if (typeof history?.replaceState !== 'function') throw new Error('History unavailable');
      history.replaceState(history.state, '', cleanUrl);
    } catch (cause) {
      throw new ApiError('token_cleanup_failed', 'The launch token could not be removed from browser history.', { cause });
    }
    try {
      if (token) storage?.setItem(TOKEN_STORAGE_KEY, token);
      else storage?.removeItem(TOKEN_STORAGE_KEY);
    } catch { /* A blocked session store still permits this initial tab session. */ }
    return token;
  }
  try {
    const token = storage?.getItem(TOKEN_STORAGE_KEY);
    return validToken(token) ? token : '';
  } catch { return ''; }
}

function apiPath(path) {
  if (typeof path !== 'string' || !path.startsWith('/') || path.startsWith('//')
    || /[\\#\x00-\x20\x7f]/.test(path)) {
    throw new ApiError('invalid_path', 'API paths must be relative paths beginning with one slash.');
  }
  const url = new URL(`/api${path}`, 'http://switchyard.invalid');
  if (url.origin !== 'http://switchyard.invalid' || !url.pathname.startsWith('/api/')) {
    throw new ApiError('invalid_path', 'API paths must remain within /api/.');
  }
  return `${url.pathname}${url.search}`;
}

/** Paths are relative to /api: request('/status') fetches /api/status. */
export function createApi(token, { fetchImpl = globalThis.fetch } = {}) {
  return {
    async request(path, { method = 'GET', body, signal } = {}) {
      if (!validToken(token)) {
        throw new ApiError('auth_required', 'Open the launch link from the local app to connect this tab.');
      }
      const url = apiPath(path);
      const verb = typeof method === 'string' ? method.toUpperCase() : '';
      if (!['GET', 'HEAD', 'OPTIONS', 'POST', 'PUT', 'PATCH', 'DELETE'].includes(verb)) {
        throw new ApiError('invalid_method', 'Unsupported API request method.');
      }
      const mutation = !['GET', 'HEAD', 'OPTIONS'].includes(verb);
      if (body !== undefined && (verb === 'GET' || verb === 'HEAD')) {
        throw new ApiError('invalid_body', 'Read requests cannot include a JSON body.');
      }
      const headers = { Accept: 'application/json', Authorization: `Bearer ${token}` };
      const options = { method: verb, headers, signal, mode: 'same-origin', redirect: 'error', credentials: 'omit', cache: 'no-store' };
      if (body !== undefined) {
        try {
          options.body = JSON.stringify(body);
          if (options.body === undefined) throw new Error('Body is not JSON');
        } catch (cause) {
          throw new ApiError('invalid_body', 'The request body could not be encoded as JSON.', { cause });
        }
        headers['Content-Type'] = 'application/json';
      }
      if (signal?.aborted) {
        throw new ApiError('aborted', 'The request was cancelled before it was sent.');
      }

      let response;
      try {
        response = await fetchImpl(url, options);
      } catch (cause) {
        const aborted = signal?.aborted || cause?.name === 'AbortError';
        throw new ApiError(aborted ? 'aborted' : 'network_error', mutation
          ? 'The request outcome could not be confirmed. Refresh session state before retrying.'
          : aborted ? 'The request was cancelled.' : 'The local app could not be reached.',
        { uncertain: mutation, cause });
      }

      if (response.ok && (response.status === 204 || verb === 'HEAD')) return null;
      let data;
      try {
        data = await response.json();
      } catch (cause) {
        if (response.ok) {
          throw new ApiError('invalid_response', 'The local app returned an unreadable response.',
            { status: response.status, uncertain: mutation, cause });
        }
      }
      if (!response.ok) {
        const code = typeof data?.error?.code === 'string' ? data.error.code : 'http_error';
        const message = typeof data?.error?.message === 'string' ? data.error.message
          : `The local app request failed (HTTP ${response.status}).`;
        throw new ApiError(code, message, {
          status: response.status,
          uncertain: mutation && response.status >= 500,
        });
      }
      return data;
    },
  };
}
