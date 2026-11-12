const { app, BrowserWindow, ipcMain } = require('electron');
const fs = require('node:fs');
const path = require('node:path');
const desktop = path.resolve(__dirname, '../../..');
const { EventEmitter } = require('node:events');
const { Client } = require(path.join(desktop, 'node_modules/ssh2'));
const { RemoteTerminalManager } = require(path.join(desktop, 'dist-electron/ssh-terminal.cjs'));
const { registerSshIpcHandlers } = require(path.join(desktop, 'dist-electron/ssh-ipc.cjs'));
const { initializeDaemonSubsystem } = require(path.join(desktop, 'dist-electron/daemon/init.cjs'));
const { TermRouter } = require(path.join(desktop, 'dist-electron/term-router.cjs'));
const { createInProcessBridge } = require(path.join(desktop, 'dist-electron/in-process-bridge.cjs'));
const { registerTerminalSessionIpc } = require(path.join(desktop, 'dist-electron/terminal-reattach.cjs'));
const { registerTerminalPersistence } = require(path.join(desktop, 'dist-electron/terminal-persistence.cjs'));
const root = process.argv[process.argv.indexOf('--fixture-root') + 1];
const entryArg = process.argv.indexOf('--daemon-entry');
const daemonEntry = entryArg < 0 ? path.join(desktop, 'dist-electron/daemon/daemon-main.cjs') : process.argv[entryArg + 1];
const urlArg = process.argv.indexOf('--renderer-url');
const rendererUrl = (urlArg < 0 ? 'http://127.0.0.1:5178' : process.argv[urlArg + 1]).replace(/\/$/, '');
if (!root || root === process.argv[0]) throw new Error('Pass an isolated --fixture-root');
if (process.argv.includes('--fixture-cli-catalog')) {
  process.env.CRAB_DESKTOP_SETTINGS_PATH = path.join(root, 'agent-settings.json');
  const { _setRegistry } = require(path.join(desktop, 'dist-electron/orchestrator-adapters.cjs'));
  _setRegistry({
    createAdapter() { throw new Error('The terminal fixture only launches interactive CLIs'); },
    listRegistered: () => [
      { type: 'claude-code', name: 'Claude Code', capabilities: {} },
      { type: 'aider', name: 'Aider', manifest: { binary: 'crab-fixture-uninstalled-aider' }, capabilities: {} },
    ],
  });
  const { registerAgentDetectionIpc } = require(path.join(desktop, 'dist-electron/ssh-agent-detection.cjs'));
  const { registerTerminalAgentCommand } = require(path.join(desktop, 'dist-electron/terminal-agent-command.cjs'));
  registerAgentDetectionIpc(ipcMain, () => undefined);
  registerTerminalAgentCommand(ipcMain, () => process.env.SHELL || '/bin/sh');
}
app.setPath('userData', path.join(root, 'profile'));
let win;
app.whenReady().then(async () => {
  for (const ws of ['a', 'b']) for (const repo of ['one', 'two']) fs.mkdirSync(path.join(root, ws, repo), { recursive: true });
  // Shell startup files belong to the fixture; host customizations must not alter its input protocol.
  const env = { ...process.env, HOME: root, ZDOTDIR: root, TERM: 'xterm-256color', CRAB_SHELL_INTEGRATION: '1', CRAB_SHELL_INTEGRATION_DIR: path.join(desktop, 'resources/shell-integration') };
  const daemon = await initializeDaemonSubsystem({
    ipcMain, app, getMainWindow: () => win, getWorkspacePath: () => null,
    socketDir: path.join(root, 'daemon'), daemonEntryPath: daemonEntry,
    log: (level, message) => console[level === 'warn' ? 'warn' : 'log'](message),
  });
  if (!daemon.daemonReady && process.env.CRAB_PTY_MODE !== 'in-process') throw new Error('Verification daemon unavailable');
  const local = createInProcessBridge({ app, getMainWindow: () => win, getDefaultShell: () => process.env.SHELL || '/bin/sh', buildTerminalEnv: extra => ({ ...env, ...extra }), shellEnvPromise: Promise.resolve(env), writeSessionLog: () => {} });
  const router = new TermRouter({
    ipcMain, circuitBreaker: daemon.circuitBreaker, getMainWindow: () => win,
    modeResolution: { current: () => ({ daemonReady: daemon.daemonClient.isConnected, configuredMode: process.env.CRAB_PTY_MODE }) },
    daemonBridge: { ...daemon.daemonBridge, handleSpawn: async (payload, id) => ({ daemonSessionId: (await daemon.daemonBridge.handleSpawn({ ...payload, env: { ...env, ...payload.env } }, id)).id }) },
    inProcessBridge: { handleSpawn: (payload, id) => local.handleSpawn(payload, id), handleWrite: (id, data) => local.handleWrite(null, id, data), handleResize: (id, cols, rows) => local.handleResize(null, id, cols, rows), handleKill: id => local.handleKill(null, id), dispose: () => local.dispose() },
  });
  router.register();
  registerTerminalSessionIpc(ipcMain, daemon.daemonClient, daemon.daemonBridge, router);
  registerTerminalPersistence(ipcMain, path.join(app.getPath('userData'), 'terminal'));
  ipcMain.handle('crab:term-list-shells', () => process.platform === 'win32' ? ['cmd.exe'] : ['/bin/zsh', '/bin/bash', '/bin/sh']);
  ipcMain.handle('crab:term-pool-hint', () => {});
  ipcMain.handle('crab:term-notify', () => {});
  ipcMain.handle('crab:rpc', () => ({ exists: false }));
  ipcMain.handle('crab:main-settings-update', () => ({}));
  ipcMain.handle('crab:set-devtools', () => {});
  const remoteFile = path.join(root, 'ssh-connection.json');
  if (fs.existsSync(remoteFile)) {
    const { port, fingerprint } = JSON.parse(fs.readFileSync(remoteFile, 'utf8'));
    let client;
    let status = 'disconnected';
    const profile = { id: 'fixture', host: '127.0.0.1', port, username: 'fixture', label: 'Loopback SSH' };
    const sessions = Object.assign(new EventEmitter(), {
      getConnection: () => ({ status, client, profile }), getStatus: () => status,
      async connect() {
        if (fs.existsSync(path.join(root, 'offline'))) throw new Error('Verification SSH host is offline');
        if (status === 'connected') return;
        client = new Client();
        await new Promise((resolve, reject) => client.once('ready', resolve).once('error', reject).connect({ host: '127.0.0.1', port, username: 'fixture', password: 'fixture-only', hostHash: 'sha256', hostVerifier: hash => hash === fingerprint }));
        status = 'connected';
        sessions.emit('ssh:status-changed', { profileId: 'fixture', status });
      },
    });
    const profiles = { get: () => profile, list: () => [profile] };
    const remote = new RemoteTerminalManager(sessions, () => profile);
    registerSshIpcHandlers(profiles, sessions, {}, { list: () => [], remove: () => false }, remote);
  }
  win = new BrowserWindow({ width: 1200, height: 820, title: 'Crab Terminal Recovery Verification', webPreferences: { preload: path.join(desktop, 'dist-electron/preload.cjs'), contextIsolation: true, nodeIntegration: false, sandbox: true } });
  win.webContents.on('console-message', (_event, level, message) => { if (level >= 2) console.log('[renderer]', message); });
  win.webContents.on('render-process-gone', (_event, details) => console.error('Renderer exited', details));
  await win.loadURL(`${rendererUrl}/tests/fixtures/terminal-recovery/index.html?root=${encodeURIComponent(root)}`);
  app.on('before-quit', () => { router.dispose(); daemon.dispose(); });
});
app.on('window-all-closed', () => app.quit());
