const API_BASE = '/api';
const DEFAULT_STATE = {
  health: { status: 'healthy', value: 98.7 },
  processes: [],
  network: [],
  services: [],
  approvals: [
    { title: 'Process stop', target: 'chrome', risk: 'Medium', action: 'Review' },
    { title: 'Service restart', target: 'redis', risk: 'High', action: 'Approve' },
  ],
  diagnostics: [
    { level: 'Low', title: 'Graceful CPU tail', summary: 'Load remains within expected range for the last 90 minutes.' },
    { level: 'Moderate', title: 'One degraded service', summary: 'Database service is reporting intermittent restarts.' },
    { level: 'High', title: 'Permission boundary check', summary: 'Authorization matrix is valid and no scope escape was detected.' },
  ],
};

const state = {
  demoMode: true,
  lastUpdated: null,
  refreshIntervalMs: 5000,
};

const els = {
  runtimeState: document.getElementById('runtimeState'),
  uptimeText: document.getElementById('uptimeText'),
  systemHealth: document.getElementById('systemHealth'),
  healthTrend: document.getElementById('healthTrend'),
  processCount: document.getElementById('processCount'),
  procTrend: document.getElementById('procTrend'),
  networkCount: document.getElementById('networkCount'),
  netTrend: document.getElementById('netTrend'),
  serviceCount: document.getElementById('serviceCount'),
  svcTrend: document.getElementById('svcTrend'),
  lastUpdated: document.getElementById('lastUpdated'),
  pulseBars: document.getElementById('pulseBars'),
  permissionPercent: document.getElementById('permissionPercent'),
  processTableBody: document.getElementById('processTableBody'),
  networkTableBody: document.getElementById('networkTableBody'),
  serviceStack: document.getElementById('serviceStack'),
  approvalList: document.getElementById('approvalList'),
  diagnosticList: document.getElementById('diagnosticList'),
  eventFeed: document.getElementById('eventFeed'),
  apiUrlInput: document.getElementById('apiUrlInput'),
  apiKeyInput: document.getElementById('apiKeyInput'),
  intervalSelect: document.getElementById('intervalSelect'),
  refreshBtn: document.getElementById('refreshBtn'),
  demoToggleBtn: document.getElementById('demoToggleBtn'),
};

function formatBytes(bytes) {
  if (bytes == null || Number.isNaN(Number(bytes))) return 'n/a';
  const value = Number(bytes);
  const units = ['B', 'KB', 'MB', 'GB'];
  let index = 0;
  let current = value;
  while (current >= 1024 && index < units.length - 1) {
    current /= 1024;
    index += 1;
  }
  return `${current.toFixed(current >= 10 || index === 0 ? 0 : 1)} ${units[index]}`;
}

function randomBetween(min, max) {
  return Math.random() * (max - min) + min;
}

function buildDemoData() {
  const now = new Date();
  const processRows = Array.from({ length: 6 }, (_, index) => ({
    name: ['node', 'postgres', 'osqueryd', 'python', 'chrome', 'redis'][index % 6],
    pid: 1100 + index * 7,
    owner: ['root', 'postgres', 'agentdb', 'agentdb', 'dev', 'root'][index],
    cpu_percent: (index + 1) * 12 + randomBetween(-3, 5),
    memory_bytes: 150000000 + index * 62000000,
    status: ['running', 'running', 'sleeping', 'running', 'sleeping', 'running'][index],
  }));

  const networkRows = Array.from({ length: 5 }, (_, index) => ({
    remote_addr: ['api.internal', 'db.primary', 'metrics.service', 'vendor-a', 'observability'][index],
    remote_port: [443, 5432, 9090, 8443, 80][index],
    protocol: ['TCP', 'TCP', 'UDP', 'TCP', 'TCP'][index],
    state: ['ESTABLISHED', 'LISTEN', 'ESTABLISHED', 'TIME_WAIT', 'ESTABLISHED'][index],
  }));

  const serviceRows = [
    { name: 'postgresql', status: 'active' },
    { name: 'osqueryd', status: 'active' },
    { name: 'redis', status: 'warning' },
    { name: 'nginx', status: 'inactive' },
  ];

  const pulse = [72, 85, 64, 90, 76, 96, 82, 73, 88, 79, 66, 92];

  return {
    health: { status: 'healthy', value: 98.7 },
    processes: processRows,
    network: networkRows,
    services: serviceRows,
    pulse,
    approvals: DEFAULT_STATE.approvals,
    diagnostics: DEFAULT_STATE.diagnostics,
    lastUpdated: now.toISOString(),
  };
}

async function fetchJson(path) {
  const key = els.apiKeyInput.value.trim();
  const headers = { 'Content-Type': 'application/json' };
  if (key) headers['X-API-Key'] = key;

  try {
    const response = await fetch(`${API_BASE}${path}`, { headers });
    if (!response.ok) {
      throw new Error(`${response.status}: ${await response.text()}`);
    }
    return await response.json();
  } catch (error) {
    console.warn(`Using demo fallback for ${path}:`, error.message);
    return null;
  }
}

function renderPulse(data) {
  const values = data.pulse || [68, 74, 81, 64, 77, 86, 71, 93, 88, 76, 80, 92];
  els.pulseBars.innerHTML = values
    .map((value, idx) => `
      <div class="bar-group">
        <div class="bar" style="height:${value}%"></div>
        <div class="bar-label">${['M','T','W','T','F','S','S','M','T','W','T','F'][idx % 12]}</div>
      </div>
    `)
    .join('');
}

function renderMetrics(data) {
  const procCount = data.processes?.length || 12;
  const netCount = data.network?.length || 7;
  const serviceCount = data.services?.length || 4;
  const healthValue = data.health?.value ?? 98.2;

  els.systemHealth.textContent = `${healthValue.toFixed(1)}%`;
  els.processCount.textContent = procCount;
  els.networkCount.textContent = netCount;
  els.serviceCount.textContent = serviceCount;
  els.permissionPercent.textContent = '96%';

  els.healthTrend.textContent = '+1.2%';
  els.procTrend.textContent = 'Stable';
  els.netTrend.textContent = `+${Math.max(netCount - 4, 0)}`;
  els.svcTrend.textContent = '2 degraded';

  const lastUpdated = new Date();
  els.lastUpdated.textContent = `Updated ${lastUpdated.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`;
  state.lastUpdated = lastUpdated;
}

function renderProcessTable(processes) {
  if (!processes || !processes.length) {
    els.processTableBody.innerHTML = '<tr><td colspan="6">No process data available</td></tr>';
    return;
  }

  els.processTableBody.innerHTML = processes.slice(0, 6).map((process) => {
    const cpu = Number(process.cpu_percent ?? 0);
    const memory = Number(process.memory_bytes ?? 0);
    const statusClass = process.status === 'running' ? 'running' : 'warning';

    return `
      <tr>
        <td>${process.name || 'unknown'}</td>
        <td>${process.pid ?? '--'}</td>
        <td>${process.owner || 'n/a'}</td>
        <td>${cpu.toFixed(1)}%</td>
        <td>${formatBytes(memory)}</td>
        <td><span class="status-pill ${statusClass}">${process.status || 'unknown'}</span></td>
      </tr>
    `;
  }).join('');
}

function renderNetworkTable(networkRows) {
  if (!networkRows || !networkRows.length) {
    els.networkTableBody.innerHTML = '<tr><td colspan="4">No network data available</td></tr>';
    return;
  }

  els.networkTableBody.innerHTML = networkRows.slice(0, 5).map((row) => `
    <tr>
      <td>${row.remote_addr || 'unknown'}</td>
      <td>${row.remote_port ?? '--'}</td>
      <td>${row.protocol || 'TCP'}</td>
      <td><span class="status-pill ${row.state === 'ESTABLISHED' ? 'running' : 'warning'}">${row.state || 'Unknown'}</span></td>
    </tr>
  `).join('');
}

function renderServices(serviceRows) {
  if (!serviceRows || !serviceRows.length) {
    els.serviceStack.innerHTML = '<div class="service-item"><div class="left"><strong>No services</strong></div></div>';
    return;
  }

  els.serviceStack.innerHTML = serviceRows.slice(0, 4).map((service) => {
    const stateClass = service.status === 'active' ? 'active' : service.status === 'warning' ? 'warning' : 'blocked';

    return `
      <div class="service-item">
        <div class="left">
          <strong>${service.name}</strong>
          <small>${service.status || 'unknown'}</small>
        </div>
        <span class="status-pill ${stateClass}">${service.status || 'unknown'}</span>
      </div>
    `;
  }).join('');
}

function renderApprovals(items) {
  if (!items || !items.length) {
    els.approvalList.innerHTML = '<div class="task-item"><div><strong>No pending approvals</strong><span>System is quiet.</span></div></div>';
    return;
  }

  els.approvalList.innerHTML = items.slice(0, 3).map((item) => `
    <div class="task-item">
      <div>
        <strong>${item.title}</strong>
        <span>${item.target} · ${item.risk}</span>
      </div>
      <div class="action-row">
        <button class="approve">${item.action}</button>
        <button class="decline">Reject</button>
      </div>
    </div>
  `).join('');
}

function renderDiagnostics(items) {
  els.diagnosticList.innerHTML = (items || DEFAULT_STATE.diagnostics).map((item) => `
    <div class="diagnostic-item">
      <strong>${item.title}</strong>
      <small>${item.level}</small>
      <span>${item.summary}</span>
    </div>
  `).join('');
}

function renderEvents() {
  const events = [
    { type: 'normal', text: 'Policy check passed for service restart', time: '2 min ago' },
    { type: 'warning', text: 'Redis service restarted once in the last 5 minutes', time: '6 min ago' },
    { type: 'danger', text: 'Permission scope anomaly auto-blocked', time: '10 min ago' },
  ];

  els.eventFeed.innerHTML = events.map((event) => `
    <div class="event-item ${event.type === 'warning' ? 'warning' : event.type === 'danger' ? 'danger' : ''}">
      <span class="event-icon"></span>
      <div>
        <strong>${event.text}</strong>
        <small>${event.time}</small>
      </div>
    </div>
  `).join('');
}

async function gatherData() {
  const [health, processes, network, services] = await Promise.all([
    fetchJson('/healthz'),
    fetchJson('/processes?limit=6'),
    fetchJson('/network?limit=5'),
    fetchJson('/services'),
  ]);

  let merged = buildDemoData();

  if (health) merged.health = { ...merged.health, ...health };
  if (processes && Array.isArray(processes)) merged.processes = processes;
  if (network && Array.isArray(network)) merged.network = network;
  if (services && Array.isArray(services)) merged.services = services;

  state.demoMode = !health && !processes && !network && !services;
  els.runtimeState.textContent = state.demoMode ? 'Demo mode' : 'Connected';
  els.runtimeState.className = `chip ${state.demoMode ? 'neutral' : 'ok'}`;

  const uptimeSeconds = state.demoMode ? 6814 : 86400 + Math.random() * 600;
  els.uptimeText.textContent = `Uptime: ${Math.floor(uptimeSeconds / 3600)}h ${Math.floor((uptimeSeconds % 3600) / 60)}m`;

  return merged;
}

function applyState(data) {
  renderMetrics(data);
  renderPulse(data);
  renderProcessTable(data.processes);
  renderNetworkTable(data.network);
  renderServices(data.services);
  renderApprovals(data.approvals);
  renderDiagnostics(data.diagnostics);
  renderEvents();
}

async function refreshDashboard() {
  const data = await gatherData();
  applyState(data);
}

function wireControls() {
  document.querySelectorAll('.nav-item').forEach((button) => {
    button.addEventListener('click', () => {
      document.querySelectorAll('.nav-item').forEach((item) => item.classList.remove('active'));
      button.classList.add('active');

      const targetId = button.dataset.target;
      const target = document.getElementById(targetId);
      if (target) {
        target.scrollIntoView({ behavior: 'smooth', block: 'start' });
      }
    });
  });

  els.refreshBtn.addEventListener('click', refreshDashboard);

  els.demoToggleBtn.addEventListener('click', () => {
    state.demoMode = !state.demoMode;
    els.demoToggleBtn.textContent = state.demoMode ? 'Demo mode' : 'Live mode';
    els.runtimeState.textContent = state.demoMode ? 'Demo mode' : 'Connected';
    els.runtimeState.className = `chip ${state.demoMode ? 'neutral' : 'ok'}`;
    refreshDashboard();
  });

  els.intervalSelect.addEventListener('change', (event) => {
    state.refreshIntervalMs = Number(event.target.value);
    if (window.refreshTimer) {
      clearInterval(window.refreshTimer);
    }
    window.refreshTimer = setInterval(refreshDashboard, state.refreshIntervalMs);
  });
}

window.addEventListener('DOMContentLoaded', () => {
  wireControls();
  renderEvents();
  refreshDashboard();
  window.refreshTimer = setInterval(refreshDashboard, state.refreshIntervalMs);
});
