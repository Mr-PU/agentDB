const http = require('http');
const fs = require('fs');
const path = require('path');

const PORT = process.env.PORT || 4173;
const BACKEND_URL = process.env.BACKEND_URL || 'http://127.0.0.1:8080';
const ROOT = __dirname;

function sendJson(res, statusCode, payload) {
  res.writeHead(statusCode, {
    'Content-Type': 'application/json',
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Headers': 'Content-Type, X-API-Key',
    'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
  });
  res.end(JSON.stringify(payload));
}

function serveFile(res, filePath) {
  fs.readFile(filePath, (err, data) => {
    if (err) {
      res.writeHead(404, { 'Content-Type': 'text/plain' });
      res.end('Not found');
      return;
    }

    const ext = path.extname(filePath).toLowerCase();
    const mimeTypes = {
      '.html': 'text/html; charset=utf-8',
      '.css': 'text/css; charset=utf-8',
      '.js': 'application/javascript; charset=utf-8',
      '.json': 'application/json; charset=utf-8',
      '.svg': 'image/svg+xml',
      '.png': 'image/png',
      '.jpg': 'image/jpeg',
      '.ico': 'image/x-icon',
    };

    res.writeHead(200, { 'Content-Type': mimeTypes[ext] || 'application/octet-stream' });
    res.end(data);
  });
}

function proxyToBackend(req, res, urlPath) {
  const upstream = new URL(urlPath, BACKEND_URL);
  const headers = {
    Accept: 'application/json',
  };

  if (req.headers['x-api-key']) {
    headers['X-API-Key'] = req.headers['x-api-key'];
  }

  const options = {
    protocol: upstream.protocol,
    hostname: upstream.hostname,
    port: upstream.port,
    path: `${upstream.pathname}${upstream.search}`,
    method: req.method,
    headers,
  };

  const target = http.request(options, (proxyRes) => {
    const chunks = [];
    proxyRes.on('data', (chunk) => chunks.push(chunk));
    proxyRes.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      res.writeHead(proxyRes.statusCode || 200, {
        'Content-Type': proxyRes.headers['content-type'] || 'application/json',
        'Access-Control-Allow-Origin': '*',
        'Access-Control-Allow-Headers': 'Content-Type, X-API-Key',
        'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
      });
      res.end(body);
    });
  });

  target.on('error', () => {
    sendJson(res, 503, { error: 'Backend unavailable. Start the AgentDB API on port 8080 or keep the demo mode enabled.' });
  });

  if (req.method !== 'GET' && req.method !== 'HEAD') {
    const bodyChunks = [];
    req.on('data', (chunk) => bodyChunks.push(chunk));
    req.on('end', () => {
      const body = Buffer.concat(bodyChunks);
      target.setHeader('Content-Length', body.length);
      target.write(body);
      target.end();
    });
  } else {
    target.end();
  }
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);

  if (req.method === 'OPTIONS') {
    res.writeHead(204, {
      'Access-Control-Allow-Origin': '*',
      'Access-Control-Allow-Headers': 'Content-Type, X-API-Key',
      'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
    });
    res.end();
    return;
  }

  if (url.pathname.startsWith('/api/')) {
    proxyToBackend(req, res, `${BACKEND_URL}${url.pathname.replace('/api', '')}${url.search}`);
    return;
  }

  const safePath = url.pathname === '/' ? '/index.html' : url.pathname;
  const filePath = path.join(ROOT, safePath);

  if (fs.existsSync(filePath) && fs.statSync(filePath).isFile()) {
    serveFile(res, filePath);
    return;
  }

  if (safePath.endsWith('/')) {
    serveFile(res, path.join(ROOT, 'index.html'));
    return;
  }

  res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
  res.end('Not found');
});

server.listen(PORT, () => {
  console.log(`AgentDB frontend running at http://localhost:${PORT}`);
  console.log(`Proxying /api/* to ${BACKEND_URL}`);
});
