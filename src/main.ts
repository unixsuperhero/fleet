/// <reference types="vite/client" />
import '@xterm/xterm/css/xterm.css';
import './styles.css';
import { HostForm } from './host-form';
import { Inventory } from './inventory';
import { Maintenance } from './maintenance';
import { Terminals, type TerminalSmoke } from './terminals';
import { errorText, invokeCommand, status, text, type MetadataContext } from './ipc';

declare global {
  interface Window { fleetSmoke?: TerminalSmoke; }
}

function required(id: string): HTMLElement {
  const node = document.getElementById(id);
  if (!(node instanceof HTMLElement)) throw new Error(`Missing Fleet UI element: ${id}`);
  return node;
}

const notice = required('app-status');
const version = required('app-version');
let metadataEpoch = 0;
let metadataRequest = 0;
let infoRequest = 0;
let restoring = false;
const context: MetadataContext = { epoch: () => metadataEpoch, restoring: () => restoring };

const hostForm = new HostForm(required('host-editor'), context, refreshMetadata);
const terminals = new Terminals(required('terminals'), context, () => {
  if (!restoring) void refreshMetadata().catch((error: unknown) => status(notice, errorText(error), 'error'));
}, message => status(notice, message, 'error'));
const inventory = new Inventory(required('inventory'), context, {
  edit: host => hostForm.edit(host),
  open: (host, intent) => {
    try { terminals.open(host, intent); }
    catch (error: unknown) { status(notice, errorText(error), 'error'); }
  },
  reload: refreshMetadata,
});
const maintenance = new Maintenance({
  restoreStarted: () => {
    restoring = true;
    metadataEpoch += 1;
    metadataRequest += 1;
    status(notice, 'Replacing metadata from the validated backup. Open terminals remain in place.');
  },
  restoreFinished: async () => {
    metadataEpoch += 1;
    metadataRequest += 1;
    restoring = false;
    await Promise.all([refreshMetadata(), refreshInfo()]);
    status(notice, 'Authoritative metadata reloaded after restore. No terminal was opened or remounted.');
  },
  reloadInfo: refreshInfo,
});

required('new-host').addEventListener('click', () => hostForm.newHost());
required('open-maintenance').addEventListener('click', () => {
  maintenance.open();
  void refreshInfo().catch((error: unknown) => status(notice, errorText(error), 'error'));
});

async function refreshMetadata(): Promise<void> {
  if (restoring) return;
  const epoch = metadataEpoch;
  const request = ++metadataRequest;
  const [hosts, sessions] = await Promise.all([
    invokeCommand('list_hosts'), invokeCommand('list_sessions', { hostId: null }),
  ]);
  if (restoring || epoch !== metadataEpoch || request !== metadataRequest) return;
  inventory.update(hosts, sessions);
  text(required('inventory-count'), `${hosts.length} enrolled · ${sessions.length} saved`);
}

async function refreshInfo(): Promise<void> {
  const request = ++infoRequest;
  const info = await invokeCommand('app_info');
  if (request !== infoRequest) return;
  text(version, `Fleet ${info.version} · schema ${info.schemaVersion} · ${info.platform}/${info.arch}`);
  maintenance.updateInfo(info);
}

if (import.meta.env.DEV || import.meta.env.MODE === 'native-smoke') {
  window.fleetSmoke = {
    snapshots: () => terminals.snapshots(),
    paste: (tabId, value) => terminals.paste(tabId, value),
    select: (tabId, column, row, length) => terminals.select(tabId, column, row, length),
    activate: tabId => terminals.activate(tabId),
  };
}

void Promise.all([refreshMetadata(), refreshInfo()]).then(() => {
  status(notice, 'Ready. Choose a host action to connect. Discovery and reattachment are explicit.');
}).catch((error: unknown) => {
  status(notice, `${errorText(error)} Saved data was not replaced. Fleet requires its native desktop backend.`, 'error');
});
