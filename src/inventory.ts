import type { ConnectIntent, DiscoveryPeer, DiscoverySnapshot, Host, HostDraft, HostId, SavedSession, SessionId, SessionStatus } from './bindings';
import { SetupCheckouts, SetupPanel } from './setup';
import { assertMetadataAvailable, button, confirmAction, element, errorText, exhaustive, invokeCommand, perform, plainWarning, status, text, type MetadataContext } from './ipc';

interface InventoryActions {
  edit(host: Host): void;
  open(host: Host, intent: ConnectIntent): void;
  reload(): Promise<void>;
}
interface SessionRow {
  node: HTMLLIElement;
  name: HTMLElement;
  state: HTMLElement;
  attach: HTMLButtonElement;
  check: HTMLButtonElement;
  session: SavedSession;
}
interface HostRow {
  node: HTMLLIElement;
  name: HTMLElement;
  target: HTMLElement;
  sessions: HTMLUListElement;
  sessionRows: Map<SessionId, SessionRow>;
  emptySessions: HTMLElement;
  setup: SetupPanel;
  host: Host;
}
interface PeerRow {
  node: HTMLLIElement;
  checkbox: HTMLInputElement;
  name: HTMLElement;
  detail: HTMLElement;
  state: HTMLElement;
  peer: DiscoveryPeer;
  present: boolean;
}

function sessionCaption(state: SessionStatus): string {
  switch (state.kind) {
    case 'unchecked': return 'Unchecked · attach verifies the original process';
    case 'live': return 'Live · last explicit check';
    case 'ended': return `Ended · ${state.reason}`;
    case 'unknown': return `Unknown · ${errorText(state.error)}`;
    default: return exhaustive(state);
  }
}

export class Inventory {
  private readonly hosts = new Map<HostId, HostRow>();
  private readonly peers = new Map<string, PeerRow>();
  private readonly checkouts = new SetupCheckouts();
  private readonly hostList = element('ul', 'host-list');
  private readonly peerList = element('ul', 'peer-list');
  private readonly emptyHosts = element('p', 'empty-copy', 'No enrolled hosts. Save a manual SSH target above or discover and explicitly select peers below.');
  private readonly emptyPeers = element('p', 'empty-copy', 'Discovery is on request. It never enrolls nodes or opens terminals automatically.');
  private readonly hostStatus = element('p', 'operation-status');
  private readonly discoveryStatus = element('p', 'operation-status');
  private readonly refresh = button('Discover peers');
  private readonly enroll = button('Enroll selected');
  private enrolling = false;

  constructor(root: HTMLElement, private readonly context: MetadataContext, private readonly actions: InventoryActions) {
    const enrolled = element('section', 'inventory-section');
    const header = element('div', 'section-heading');
    const reload = button('Reload saved', () => {
      void perform(reload, this.hostStatus, async () => {
        assertMetadataAvailable(this.context);
        await this.actions.reload();
        status(this.hostStatus, 'Saved inventory reloaded. No connections were opened.');
      });
    });
    reload.id = 'reload-inventory';
    header.append(element('h2', '', 'Enrolled hosts'), reload);
    this.hostList.id = 'enrolled-hosts';
    this.hostStatus.setAttribute('role', 'status');
    enrolled.append(header, this.emptyHosts, this.hostList, this.hostStatus);
    const discovered = element('section', 'inventory-section discovery-section');
    const discoveryHeader = element('div', 'section-heading');
    this.refresh.id = 'refresh-discovery';
    this.enroll.id = 'enroll-selected';
    this.enroll.disabled = true;
    this.enroll.className = 'primary';
    discoveryHeader.append(element('h2', '', 'Tailnet discovery'), this.refresh);
    this.peerList.id = 'discovered-peers';
    this.discoveryStatus.setAttribute('role', 'status');
    discovered.append(discoveryHeader, this.emptyPeers, this.peerList, this.enroll, this.discoveryStatus,
      element('p', 'help', 'Online is a Tailscale observation, not SSH readiness. Offline and stale peers may be enrolled for later use. The local hub and NAS cannot be enrolled.'));
    root.append(enrolled, discovered);
    this.refresh.addEventListener('click', () => {
      void perform(this.refresh, this.discoveryStatus, async () => {
        status(this.discoveryStatus, 'Reading local Tailscale inventory…');
        try { this.patchDiscovery(await invokeCommand('discover')); }
        catch (error: unknown) {
          for (const row of this.peers.values()) { row.present = false; this.patchPeer(row); }
          status(this.discoveryStatus, `${errorText(error)} Manual SSH enrollment remains available.`, 'error');
          throw error;
        }
      });
    });
    this.enroll.addEventListener('click', () => { void this.enrollSelected(); });
  }

  update(hosts: Host[], sessions: SavedSession[]): void {
    const scroll = this.hostList.scrollTop;
    const seen = new Set<HostId>();
    for (const host of hosts) {
      seen.add(host.id);
      const row = this.hosts.get(host.id) ?? this.createHost(host);
      row.host = host;
      row.setup.setHost(host);
      text(row.name, host.input.label);
      text(row.target, host.input.sshTarget);
      row.target.title = host.input.sshTarget;
      const ownSessions = sessions.filter(session => session.hostId === host.id);
      const seenSessions = new Set<SessionId>();
      for (const session of ownSessions) {
        seenSessions.add(session.id);
        const saved = row.sessionRows.get(session.id) ?? this.createSession(row, session);
        saved.session = session;
        text(saved.name, session.label);
        text(saved.state, sessionCaption(session.status));
        saved.state.dataset.state = session.status.kind;
        saved.attach.disabled = session.status.kind === 'ended';
        saved.attach.title = session.status.kind === 'ended' ? 'The original process ended. Create a new persistent session explicitly.' : 'Verify and attach to this saved original session';
      }
      for (const [id, saved] of row.sessionRows) {
        if (!seenSessions.has(id)) { saved.node.remove(); row.sessionRows.delete(id); }
      }
      row.emptySessions.hidden = ownSessions.length !== 0;
    }
    for (const [id, row] of this.hosts) {
      if (!seen.has(id)) { row.setup.dispose(); row.node.remove(); this.hosts.delete(id); }
    }
    this.emptyHosts.hidden = hosts.length !== 0;
    this.hostList.scrollTop = scroll;
    for (const peer of this.peers.values()) this.patchPeer(peer);
    this.updateSelection();
  }

  private createHost(host: Host): HostRow {
    const node = element('li', 'host-row');
    node.dataset.hostId = host.id;
    const name = element('h3', 'host-name');
    const target = element('p', 'mono host-target');
    const actions = element('div', 'actions compact');
    const row: HostRow = { node, name, target, sessions: element('ul', 'session-list'), sessionRows: new Map(), emptySessions: element('p', 'help', 'No saved sessions. Plain SSH is not saved as a persistent process.'), setup: new SetupPanel(host, this.checkouts, this.context, this.actions), host };
    const edit = button('Edit', () => this.actions.edit(row.host));
    edit.dataset.action = 'edit-host';
    const remove = button('Delete…', () => {
      void perform(remove, this.hostStatus, async () => {
        assertMetadataAvailable(this.context);
        const epoch = this.context.epoch();
        const approved = await confirmAction('Delete enrolled host?', `Delete ${row.host.input.label} and its saved session references from Fleet? This does not stop remote tmux sessions or close existing terminal tabs.`, 'Delete host', true);
        if (!approved || epoch !== this.context.epoch()) return;
        assertMetadataAvailable(this.context);
        await invokeCommand('delete_host', { hostId: row.host.id });
        if (epoch !== this.context.epoch()) return;
        await this.actions.reload();
        status(this.hostStatus, 'Host metadata deleted. Remote processes were not deleted.');
      });
    });
    remove.dataset.action = 'delete-host';
    const plain = button('Plain SSH…', () => {
      void perform(plain, this.hostStatus, async () => {
        assertMetadataAvailable(this.context);
        const epoch = this.context.epoch();
        if (await confirmAction('Open a plain SSH terminal?', plainWarning, 'Open plain SSH')) {
          if (epoch !== this.context.epoch()) return;
          assertMetadataAvailable(this.context);
          this.actions.open(row.host, { kind: 'plain' });
        }
      });
    });
    plain.dataset.action = 'plain-terminal';
    actions.append(plain, edit, remove);
    const creation = element('form', 'session-create');
    const input = element('input');
    input.type = 'text';
    input.required = true;
    input.maxLength = 120;
    input.autocomplete = 'off';
    input.placeholder = 'New persistent session label';
    input.setAttribute('aria-label', `New persistent session label for ${host.input.label}`);
    input.dataset.action = 'session-label';
    const create = button('Create tmux');
    create.type = 'submit';
    create.dataset.action = 'create-session';
    creation.append(input, create);
    creation.addEventListener('submit', event => {
      event.preventDefault();
      if (!creation.reportValidity()) return;
      try {
        assertMetadataAvailable(this.context);
        const label = input.value.trim();
        if (label === '') { input.setCustomValidity('Enter a session label.'); input.reportValidity(); return; }
        input.setCustomValidity('');
        this.actions.open(row.host, { kind: 'createPersistent', label });
      } catch (error: unknown) { status(this.hostStatus, errorText(error), 'error'); }
    });
    input.addEventListener('input', () => input.setCustomValidity(''));
    node.append(name, target, actions, creation, row.setup.node, row.sessions, row.emptySessions);
    this.hosts.set(host.id, row);
    this.hostList.append(node);
    return row;
  }

  private createSession(host: HostRow, session: SavedSession): SessionRow {
    const node = element('li', 'session-row');
    node.dataset.sessionId = session.id;
    const name = element('strong', 'session-name');
    const state = element('p', 'session-state');
    const attach = button('Attach');
    const check = button('Check');
    attach.dataset.action = 'attach-session';
    check.dataset.action = 'check-session';
    const row: SessionRow = { node, name, state, attach, check, session };
    attach.addEventListener('click', () => {
      try {
        assertMetadataAvailable(this.context);
        if (row.session.status.kind === 'ended') return;
        this.actions.open(host.host, { kind: 'attachSaved', sessionId: row.session.id });
      } catch (error: unknown) { status(this.hostStatus, errorText(error), 'error'); }
    });
    check.addEventListener('click', () => {
      void perform(check, this.hostStatus, async () => {
        assertMetadataAvailable(this.context);
        const epoch = this.context.epoch();
        status(this.hostStatus, 'Checking the saved original remote process…');
        await invokeCommand('check_session', { sessionId: row.session.id });
        if (epoch !== this.context.epoch()) return;
        await this.actions.reload();
        if (epoch === this.context.epoch()) status(this.hostStatus, 'Session check complete. No replacement session was created.');
      });
    });
    const actions = element('div', 'actions compact');
    actions.append(attach, check);
    node.append(name, state, actions);
    host.sessionRows.set(session.id, row);
    host.sessions.append(node);
    return row;
  }

  private patchDiscovery(snapshot: DiscoverySnapshot): void {
    const scroll = this.peerList.scrollTop;
    for (const row of this.peers.values()) row.present = false;
    for (const peer of snapshot.peers) {
      const row = this.peers.get(peer.nodeId) ?? this.createPeer(peer);
      row.peer = peer;
      row.present = true;
    }
    switch (snapshot.availability.kind) {
      case 'available': status(this.discoveryStatus, `Observed ${snapshot.observedAt}. Nothing was enrolled automatically.`); break;
      case 'unavailable':
        for (const row of this.peers.values()) row.present = false;
        status(this.discoveryStatus, `${snapshot.availability.reason} Manual SSH enrollment remains available.`, 'error');
        break;
      default: exhaustive(snapshot.availability);
    }
    for (const row of this.peers.values()) this.patchPeer(row);
    this.emptyPeers.hidden = this.peers.size !== 0;
    if (this.peers.size === 0 && snapshot.availability.kind === 'available') text(this.emptyPeers, 'No peers reported. Add a manual SSH target, or check your local Tailscale connection and discover again.');
    this.peerList.scrollTop = scroll;
    this.updateSelection();
  }

  private createPeer(peer: DiscoveryPeer): PeerRow {
    const node = element('li', 'peer-row');
    node.dataset.nodeId = peer.nodeId;
    const label = element('label', 'peer-choice');
    const checkbox = element('input');
    checkbox.type = 'checkbox';
    const content = element('span', 'peer-copy');
    const name = element('strong');
    const detail = element('span', 'mono peer-detail');
    const state = element('span', 'peer-state');
    content.append(name, detail, state);
    label.append(checkbox, content);
    node.append(label);
    const row: PeerRow = { node, checkbox, name, detail, state, peer, present: true };
    checkbox.addEventListener('change', () => this.updateSelection());
    this.peers.set(peer.nodeId, row);
    this.peerList.append(node);
    return row;
  }

  private patchPeer(row: PeerRow): void {
    const peer = row.peer;
    const enrolled = Array.from(this.hosts.values()).some(host => host.host.input.tailnetNodeId === peer.nodeId);
    const nas = /(^|[._-])nas([._-]|$)/i.test(`${peer.displayName}.${peer.dnsName}`);
    const excluded = peer.excluded || nas;
    const observed = Date.parse(peer.observedAt);
    const stale = !row.present || !Number.isFinite(observed) || Date.now() - observed > 5 * 60 * 1000;
    const labels = [peer.online ? 'Online' : 'Offline'];
    if (stale) labels.push('Stale observation');
    if (enrolled) labels.push('Already enrolled');
    if (excluded) labels.push(nas ? 'Excluded · NAS' : 'Excluded · local hub / protected node');
    text(row.name, peer.displayName || peer.dnsName || peer.nodeId);
    text(row.detail, peer.dnsName || peer.addresses.join(', '));
    text(row.state, labels.join(' · '));
    row.state.title = `Observed ${peer.observedAt}${peer.lastSeenAt === null ? '' : `; last seen ${peer.lastSeenAt}`}`;
    row.node.dataset.online = String(peer.online);
    row.node.dataset.stale = String(stale);
    row.node.dataset.excluded = String(excluded);
    row.checkbox.disabled = excluded || enrolled || this.enrolling;
    if (excluded || enrolled) row.checkbox.checked = false;
    row.checkbox.setAttribute('aria-label', `Select ${peer.displayName || peer.dnsName || peer.nodeId} for enrollment`);
  }

  private updateSelection(): void {
    const count = Array.from(this.peers.values()).filter(row => row.checkbox.checked && !row.checkbox.disabled).length;
    text(this.enroll, count === 0 ? 'Enroll selected' : `Enroll ${count} selected`);
    this.enroll.disabled = count === 0 || this.enrolling;
  }

  private async enrollSelected(): Promise<void> {
    if (this.enrolling) return;
    try { assertMetadataAvailable(this.context); }
    catch (error: unknown) { status(this.discoveryStatus, errorText(error), 'error'); return; }
    const selected = Array.from(this.peers.values()).filter(row => row.checkbox.checked && !row.checkbox.disabled).map(row => row.peer);
    if (selected.length === 0) return;
    const epoch = this.context.epoch();
    this.enrolling = true;
    for (const row of this.peers.values()) this.patchPeer(row);
    this.updateSelection();
    let saved = 0;
    try {
      for (const peer of selected) {
        if (epoch !== this.context.epoch()) return;
        assertMetadataAvailable(this.context);
        const target = peer.dnsName || peer.addresses[0];
        if (target === undefined || target === '') throw { stage: 'discovery', code: 'invalidInput', message: 'The selected peer has no SSH target. Use manual enrollment.' };
        const draft: HostDraft = {
          label: peer.displayName || peer.dnsName || peer.nodeId, sshTarget: target,
          portOverride: null, hostNameOverride: null, hostKeyAliasOverride: null, tailnetNodeId: peer.nodeId,
        };
        await invokeCommand('save_host', { hostId: null, draft });
        saved += 1;
        if (epoch !== this.context.epoch()) return;
        await this.actions.reload();
      }
      if (epoch === this.context.epoch()) status(this.discoveryStatus, `${saved} selected peer${saved === 1 ? '' : 's'} enrolled. Review SSH targets with Edit; no terminal was opened.`);
    } catch (error: unknown) {
      if (epoch === this.context.epoch()) {
        status(this.discoveryStatus, `${saved} peer${saved === 1 ? '' : 's'} saved. ${errorText(error)}`, 'error');
        try { await this.actions.reload(); }
        catch (reloadError: unknown) { status(this.hostStatus, errorText(reloadError), 'error'); }
      }
    } finally {
      this.enrolling = false;
      for (const row of this.peers.values()) this.patchPeer(row);
      this.updateSelection();
    }
  }
}
