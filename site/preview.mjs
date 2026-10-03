import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { dirname, extname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), 'public');
const port = Number(process.env.SWITCHYA_SITE_PORT || 18742);
if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new Error('Invalid SWITCHYA_SITE_PORT');
const types = { '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.txt': 'text/plain; charset=utf-8', '.xml': 'application/xml; charset=utf-8' };
const rules = (await readFile(resolve(root, '_headers'), 'utf8')).split(/\r?\n/);
const headers = {};
let globalRule = false;
for (const line of rules) {
  if (!line.trim()) continue;
  if (!/^\s/.test(line)) { globalRule = line === '/*'; continue; }
  if (!globalRule) continue;
  const colon = line.indexOf(':');
  if (colon > 0) headers[line.slice(0, colon).trim()] = line.slice(colon + 1).trim();
}

const server = createServer(async (request, response) => {
  if (!['GET', 'HEAD'].includes(request.method)) {
    response.writeHead(405, { ...headers, Allow: 'GET, HEAD' });
    response.end();
    return;
  }
  let file;
  try {
    const pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    file = resolve(root, `.${pathname === '/' ? '/index.html' : pathname}`);
    if (!file.startsWith(root + sep) || pathname.split('/').some(part => part.startsWith('_') || part.startsWith('.'))) throw new Error('Invalid path');
    if (!(await stat(file)).isFile()) throw new Error('Not a file');
    const content = await readFile(file);
    response.writeHead(200, { ...headers, 'Content-Type': types[extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-store' });
    response.end(request.method === 'HEAD' ? undefined : content);
  } catch {
    response.writeHead(404, { ...headers, 'Content-Type': 'text/html; charset=utf-8' });
    response.end(request.method === 'HEAD' ? undefined : await readFile(resolve(root, '404.html')));
  }
});
server.listen(port, '127.0.0.1', () => console.log(`Switchya local preview: http://127.0.0.1:${port}`));
