import { Channel } from '@tauri-apps/api/core';
import { Terminal, type IDisposable } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import type { AppError, Challenge, ChallengeAnswer, ChallengeId, ConnectIntent, ConnectionEvent, ConnectionLease, ConnectionState, Host } from './bindings';
import { assertMetadataAvailable, button, confirmAction, element, errorText, exhaustive, invokeCommand, nativeError, perform, plainWarning, stageName, status, text, type MetadataContext } from './ipc';

type ActiveAttempt = {
  kind: 'active'; epoch: number; metadataEpoch: number; lease: ConnectionLease;
  connectionBytes: Uint8Array; attemptBytes: Uint8Array; lastSequence: number | null;
  input: Promise<void>; acknowledgements: Promise<void>; pendingInputBytes: number;
};
type Attempt = { kind: 'idle' } | { kind: 'allocating'; epoch: number } | ActiveAttempt;

export interface TerminalSnapshot {
  id: string;
  hostId: Host['id'];
  lease: ConnectionLease | null;
  state: ConnectionState;
  cols: number;
  rows: number;
  buffer: string;
  selection: string;
  active: boolean;
}
export interface TerminalSmoke {
  snapshots(): TerminalSnapshot[];
  paste(tabId: string, value: string): void;
  select(tabId: string, column: number, row: number, length: number): void;
  activate(tabId: string): void;
}

function uuidBytes(id: string): Uint8Array {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id)) throw new Error('Invalid connection identifier');
  const hex = id.replaceAll('-', '');
  return Uint8Array.from({ length: 16 }, (_, index) => Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16));
}
function matches(bytes: Uint8Array, expected: Uint8Array, offset: number): boolean {
  return expected.every((value, index) => bytes[offset + index] === value);
}
function sameLease(left: ConnectionLease, right: ConnectionLease): boolean {
  return left.connectionId === right.connectionId && left.attemptId === right.attemptId;
}
const setupNotice = 'Setup output is shown exactly as produced. Fleet reads no tokens. Exit status alone is not verification, and a partial or canceled run may have left changes. This single-use ticket cannot be retried; run the step again from the host setup panel.';

function intentLabel(intent: ConnectIntent): string {
  switch (intent.kind) {
    case 'plain': return 'Plain SSH';
    case 'createPersistent': return intent.label;
    case 'attachSaved': return 'Saved tmux session';
    case 'hostSetup': return 'TUIOS setup';
    default: return exhaustive(intent);
  }
}

class TerminalTab {
  readonly pane = element('section', 'terminal-pane');
  readonly tab = button('');
  readonly tabWrapper = element('div', 'terminal-tab');
  readonly terminal: Terminal;
  state: ConnectionState = { kind: 'disconnected', reason: 'Not connected' };
  private readonly surface = element('div', 'terminal-surface');
  private readonly stateText = element('p', 'connection-status');
  private readonly notice = element('p', 'operation-status terminal-notice');
  private readonly retry = button('Retry');
  private readonly stop = button('Cancel');
  private readonly plain = button('Use plain SSH…');
  private readonly challengePanel = element('section', 'challenge-panel');
  private readonly challengeTitle = element('h3');
  private readonly challengeDetail = element('p', 'challenge-detail');
  private readonly challengePrompt = element('p', 'challenge-prompt');
  private readonly challengeForm = element('form', 'challenge-form');
  private readonly secretLabel = element('label', 'field');
  private readonly secret = element('input');
  private readonly accept = button('Accept verified key');
  private readonly decline = button('Decline');
  private readonly challengeStatus = element('p', 'operation-status');
  private readonly fit = new FitAddon();
  private readonly observer: ResizeObserver;
  private readonly disposables: IDisposable[] = [];
  private attempt: Attempt = { kind: 'idle' };
  private epoch = 0;
  private challengeId: ChallengeId | null = null;
  private closed = false;
  private activated = false;
  private fitFrame: number | null = null;
  private readonly encoder = new TextEncoder();

  constructor(
    readonly id: string,
    readonly host: Host,
    private intent: ConnectIntent,
    private readonly context: MetadataContext,
    activateTab: () => void,
    closeTab: () => void,
    private readonly sessionChanged: () => void,
    private readonly report: (message: string) => void,
  ) {
    this.tab.id = `tab-${id}`;
    this.tab.setAttribute('role', 'tab');
    this.tab.setAttribute('aria-controls', `panel-${id}`);
    text(this.tab, `${host.input.label} · ${intentLabel(intent)}`);
    this.tab.dataset.tabId = id;
    this.tab.addEventListener('click', activateTab);
    const close = button('×', closeTab);
    close.dataset.action = 'close-terminal';
    close.setAttribute('aria-label', `Close terminal for ${host.input.label}`);
    this.tabWrapper.setAttribute('role', 'presentation');
    this.tabWrapper.append(this.tab, close);
    this.pane.id = `panel-${id}`;
    this.pane.dataset.tabId = id;
    this.pane.dataset.hostId = host.id;
    this.pane.setAttribute('role', 'tabpanel');
    this.pane.setAttribute('aria-labelledby', this.tab.id);
    this.stateText.setAttribute('role', 'status');
    this.notice.setAttribute('role', 'status');
    const toolbar = element('div', 'terminal-toolbar');
    const controls = element('div', 'actions compact');
    this.retry.dataset.action = 'retry-terminal';
    this.stop.dataset.action = 'cancel-terminal';
    this.plain.dataset.action = 'plain-terminal';
    const copy = button('Copy selection', () => {
      void perform(copy, this.notice, async () => {
        const selection = this.terminal.getSelection();
        if (selection === '') { status(this.notice, 'Select terminal text before copying.'); return; }
        await navigator.clipboard.writeText(selection);
        status(this.notice, 'Selected terminal text copied by your action.');
      });
    });
    controls.append(this.retry, this.stop, this.plain, copy);
    toolbar.append(this.stateText, controls);
    this.challengePanel.setAttribute('role', 'dialog');
    this.challengePanel.setAttribute('aria-label', 'SSH consent or authentication');
    this.challengePanel.hidden = true;
    this.secret.type = 'password';
    this.secret.autocomplete = 'off';
    this.secret.spellcheck = false;
    this.secret.name = 'ephemeral-authentication';
    this.secret.dataset.action = 'authentication-value';
    this.secretLabel.append(element('span', 'field-label', 'Authentication response'), this.secret);
    this.accept.type = 'submit';
    this.accept.className = 'primary';
    this.accept.dataset.action = 'accept-challenge';
    this.decline.dataset.action = 'decline-challenge';
    this.challengeStatus.setAttribute('role', 'status');
    const challengeActions = element('div', 'actions');
    challengeActions.append(this.decline, this.accept);
    this.challengeForm.append(this.secretLabel, challengeActions, this.challengeStatus);
    this.challengePanel.append(this.challengeTitle, this.challengeDetail, this.challengePrompt, this.challengeForm);
    this.pane.append(toolbar, this.challengePanel, this.surface, this.notice);
    this.terminal = new Terminal({
      allowProposedApi: true, cursorBlink: true, scrollback: 10000,
      fontFamily: '"SF Mono", Menlo, Monaco, monospace', fontSize: 13,
      minimumContrastRatio: 4.5, screenReaderMode: true, disableStdin: true,
      macOptionClickForcesSelection: true, logLevel: 'off',
      theme: { background: '#17191c', foreground: '#e6e2d9', cursor: '#e0b66b', selectionBackground: '#665332', black: '#23262b', red: '#e68e86', green: '#a5c397', yellow: '#e0b66b', blue: '#96b7ce', magenta: '#c4a8c8', cyan: '#91c1c0', white: '#e6e2d9' },
      linkHandler: {
        activate: (event, url) => {
          if (event.button !== 0) return;
          let parsed: URL;
          try { parsed = new URL(url); }
          catch { status(this.notice, 'This terminal link is not a valid HTTP or HTTPS URL.', 'error'); return; }
          if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
            status(this.notice, 'Only HTTP and HTTPS terminal links can be opened.', 'error');
            return;
          }
          void invokeCommand('open_link', { url }).catch((error: unknown) => status(this.notice, errorText(error), 'error'));
        },
      },
    });
    this.terminal.loadAddon(this.fit);
    this.disposables.push(this.terminal.parser.registerOscHandler(52, () => true));
    this.disposables.push(this.terminal.onData(value => this.send(this.encoder.encode(value))));
    this.disposables.push(this.terminal.onBinary(value => {
      const bytes = Uint8Array.from(value, character => character.charCodeAt(0));
      this.send(bytes);
    }));
    this.disposables.push(this.terminal.onResize(size => {
      const attempt = this.attempt;
      if (attempt.kind !== 'active' || this.state.kind !== 'connected') return;
      this.enqueue(attempt, async () => {
        await invokeCommand('terminal_resize', { lease: attempt.lease, cols: size.cols, rows: size.rows });
      });
    }));
    this.observer = new ResizeObserver(() => this.scheduleFit());
    this.retry.addEventListener('click', () => { void this.retryExplicitly(); });
    this.stop.addEventListener('click', () => { void this.cancel(); });
    this.plain.addEventListener('click', () => {
      void perform(this.plain, this.notice, async () => {
        if (!await confirmAction('Use a plain SSH terminal?', plainWarning, 'Open plain SSH')) return;
        assertMetadataAvailable(this.context);
        this.intent = { kind: 'plain' };
        await this.begin();
      });
    });
    this.decline.addEventListener('click', () => { void this.answer({ kind: 'decline' }); });
    this.challengeForm.addEventListener('submit', event => {
      event.preventDefault();
      if (this.state.kind !== 'awaitingConsent') { this.secret.value = ''; return; }
      switch (this.state.challenge.kind) {
        case 'hostKey':
          if (event.submitter === this.accept && this.keyIsComplete(this.state.challenge)) void this.answer({ kind: 'accept' });
          break;
        case 'authentication': {
          const value = this.secret.value;
          this.secret.value = '';
          void this.answer({ kind: 'authentication', value });
          break;
        }
        default: exhaustive(this.state.challenge.kind);
      }
    });
    this.renderState();
  }

  mount(): void {
    this.terminal.open(this.surface);
    this.observer.observe(this.surface);
  }

  activate(active: boolean, focus = false): void {
    this.activated = active;
    this.pane.hidden = !active;
    this.tab.setAttribute('aria-selected', String(active));
    this.tab.tabIndex = active ? 0 : -1;
    this.tabWrapper.dataset.active = String(active);
    if (active) {
      this.scheduleFit();
      if (focus) {
        if (this.state.kind === 'awaitingConsent') this.focusChallenge();
        else this.terminal.focus();
      }
    }
  }

  private scheduleFit(): void {
    if (!this.activated || this.closed || this.fitFrame !== null) return;
    this.fitFrame = requestAnimationFrame(() => {
      this.fitFrame = null;
      if (!this.activated || this.closed || this.surface.clientWidth === 0 || this.surface.clientHeight === 0) return;
      this.fit.fit();
    });
  }

  private current(attempt: ActiveAttempt): boolean { return !this.closed && this.attempt === attempt; }

  async begin(): Promise<void> {
    if (this.closed) return;
    assertMetadataAvailable(this.context);
    const previous = this.attempt;
    const epoch = ++this.epoch;
    const metadataEpoch = this.context.epoch();
    this.attempt = { kind: 'allocating', epoch };
    this.state = { kind: 'connecting', stage: 'local' };
    this.clearChallenge();
    this.renderState();
    status(this.notice, this.intent.kind === 'hostSetup' ? setupNotice : this.intent.kind === 'plain' ? plainWarning : 'Persistent sessions require remote tmux. Fleet never installs it or recreates an ended process automatically.');
    try {
      if (previous.kind === 'active') await invokeCommand('terminal_disconnect', { lease: previous.lease });
      await new Promise<void>(resolve => this.terminal.write('', resolve));
      if (this.closed || this.epoch !== epoch) return;
      if (metadataEpoch !== this.context.epoch() || this.context.restoring()) {
        this.attempt = { kind: 'idle' };
        this.state = { kind: 'disconnected', reason: 'Metadata changed before connection startup. Retry explicitly after restore.' };
        this.renderState();
        return;
      }
      const lease = await invokeCommand('allocate_connection');
      if (this.closed || this.epoch !== epoch || metadataEpoch !== this.context.epoch() || this.context.restoring()) {
        await invokeCommand('terminal_disconnect', { lease });
        if (!this.closed && this.epoch === epoch) {
          this.attempt = { kind: 'idle' };
          this.state = { kind: 'disconnected', reason: 'Connection startup canceled by metadata restore. Retry explicitly.' };
          this.renderState();
        }
        return;
      }
      const attempt: ActiveAttempt = {
        kind: 'active', epoch, metadataEpoch, lease,
        connectionBytes: uuidBytes(lease.connectionId), attemptBytes: uuidBytes(lease.attemptId),
        lastSequence: null, input: Promise.resolve(), acknowledgements: Promise.resolve(), pendingInputBytes: 0,
      };
      this.attempt = attempt;
      if (this.activated && this.surface.clientWidth > 0 && this.surface.clientHeight > 0) this.fit.fit();
      const output = new Channel<ArrayBuffer>(frame => this.receive(attempt, frame));
      const events = new Channel<ConnectionEvent>(event => this.event(attempt, event));
      await invokeCommand('connect', { lease, hostId: this.host.id, intent: this.intent, cols: this.terminal.cols, rows: this.terminal.rows, output, events });
    } catch (error: unknown) {
      if (!this.closed && this.epoch === epoch) this.fail(nativeError(error));
      else this.report(errorText(error));
    }
  }

  private async retryExplicitly(): Promise<void> {
    void perform(this.retry, this.notice, async () => {
      assertMetadataAvailable(this.context);
      if (this.intent.kind === 'createPersistent') {
        const approved = await confirmAction('Create another persistent session?', 'The previous attempt did not return a saved reference. It may have started a remote process before the connection failed. Retrying creation makes a new session; it does not resume that process.', 'Create a new session');
        if (!approved) return;
      }
      await this.begin();
    });
  }

  private enqueue(attempt: ActiveAttempt, operation: () => Promise<void>): void {
    attempt.input = attempt.input.then(async () => {
      if (!this.current(attempt)) return;
      await operation();
    }).catch((error: unknown) => { if (this.current(attempt)) this.fail(nativeError(error)); });
  }

  private send(bytes: Uint8Array): void {
    const attempt = this.attempt;
    if (attempt.kind !== 'active' || this.state.kind !== 'connected') return;
    if (attempt.pendingInputBytes + bytes.byteLength > 512 * 1024) {
      status(this.notice, 'The input queue is full. This input was not sent. Wait for pending input before trying again.', 'error');
      return;
    }
    attempt.pendingInputBytes += bytes.byteLength;
    this.enqueue(attempt, async () => {
      try {
        await invokeCommand('terminal_input', { lease: attempt.lease, bytes: Array.from(bytes) });
      } finally { attempt.pendingInputBytes -= bytes.byteLength; }
    });
  }

  private receive(attempt: ActiveAttempt, buffer: ArrayBuffer): void {
    if (!this.current(attempt)) return;
    if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < 36) {
      this.fail({ stage: 'local', code: 'connectionFailed', message: 'The native terminal returned an invalid output frame.' });
      return;
    }
    const frame = new Uint8Array(buffer);
    if (!matches(frame, attempt.connectionBytes, 0) || !matches(frame, attempt.attemptBytes, 16)) return;
    const sequence = new DataView(buffer).getUint32(32, false);
    if (attempt.lastSequence !== null && sequence <= attempt.lastSequence) return;
    attempt.lastSequence = sequence;
    this.terminal.write(new Uint8Array(buffer, 36), () => {
      if (!this.current(attempt)) return;
      attempt.acknowledgements = attempt.acknowledgements.then(async () => {
        if (this.current(attempt)) await invokeCommand('acknowledge_output', { lease: attempt.lease, sequence });
      }).catch((error: unknown) => {
        if (this.current(attempt) && this.state.kind !== 'disconnected' && this.state.kind !== 'failed') this.fail(nativeError(error));
      });
    });
  }

  private event(attempt: ActiveAttempt, event: ConnectionEvent): void {
    if (!this.current(attempt) || !sameLease(attempt.lease, event.lease)) return;
    switch (event.body.kind) {
      case 'state': {
        this.state = event.body.state;
        if (this.state.kind === 'connected') {
          switch (this.state.mode.kind) {
            case 'plain': break;
            case 'setup': break;
            case 'tmux': this.intent = { kind: 'attachSaved', sessionId: this.state.mode.sessionId }; break;
            default: exhaustive(this.state.mode);
          }
          this.enqueue(attempt, async () => {
            await invokeCommand('terminal_resize', { lease: attempt.lease, cols: this.terminal.cols, rows: this.terminal.rows });
          });
        }
        this.renderState();
        break;
      }
      case 'session':
        if (attempt.metadataEpoch !== this.context.epoch() || this.context.restoring()) return;
        if (event.body.session.hostId !== this.host.id) return;
        this.intent = { kind: 'attachSaved', sessionId: event.body.session.id };
        this.sessionChanged();
        break;
      case 'exit':
        this.state = { kind: 'disconnected', reason: this.intent.kind === 'hostSetup'
          ? `Setup process ended${event.body.exitCode === null ? '' : ` with status ${event.body.exitCode}`}. That is not verification. Run a step again from the host setup panel.`
          : event.body.exitCode === null ? 'SSH process ended. Reconnect explicitly.' : `SSH exited with status ${event.body.exitCode}. Reconnect explicitly.` };
        this.renderState();
        break;
      default: exhaustive(event.body);
    }
  }

  private keyIsComplete(challenge: Challenge): boolean {
    return challenge.algorithm !== null && challenge.algorithm.trim() !== '' && challenge.fingerprint !== null && /^SHA256:[A-Za-z0-9+/]{43}=?$/.test(challenge.fingerprint);
  }

  private clearChallenge(): void {
    this.secret.value = '';
    this.challengeId = null;
    this.challengePanel.hidden = true;
    status(this.challengeStatus, '');
  }

  private showChallenge(challenge: Challenge): void {
    if (this.challengeId === challenge.id) return;
    this.secret.value = '';
    this.challengeId = challenge.id;
    this.secret.disabled = false;
    this.decline.disabled = false;
    this.accept.disabled = false;
    text(this.challengePrompt, challenge.prompt);
    switch (challenge.kind) {
      case 'hostKey':
        text(this.challengeTitle, 'Verify the SSH host key');
        text(this.challengeDetail, `Host: ${challenge.host}\nAlgorithm: ${challenge.algorithm ?? 'not provided'}\nFingerprint: ${challenge.fingerprint ?? 'not provided'}`);
        text(this.accept, 'Accept verified key');
        this.secretLabel.hidden = true;
        this.accept.disabled = !this.keyIsComplete(challenge);
        status(this.challengeStatus, this.keyIsComplete(challenge) ? 'Compare this fingerprint through a trusted channel before accepting.' : 'The native challenge lacks a valid algorithm or SHA256 fingerprint. Acceptance is blocked.', this.keyIsComplete(challenge) ? 'normal' : 'error');
        break;
      case 'authentication':
        text(this.challengeTitle, 'SSH authentication');
        text(this.challengeDetail, `Host: ${challenge.host}`);
        text(this.accept, 'Send response');
        this.secretLabel.hidden = false;
        status(this.challengeStatus, 'The response is sent only to this SSH attempt. It is not saved or added to diagnostics.');
        break;
      default: exhaustive(challenge.kind);
    }
    this.challengePanel.hidden = false;
    if (this.activated) this.focusChallenge();
  }

  private focusChallenge(): void {
    if (this.state.kind !== 'awaitingConsent') return;
    switch (this.state.challenge.kind) {
      case 'hostKey': this.decline.focus(); break;
      case 'authentication': this.secret.focus(); break;
      default: exhaustive(this.state.challenge.kind);
    }
  }

  private async answer(answer: ChallengeAnswer): Promise<void> {
    this.secret.value = '';
    const attempt = this.attempt;
    if (attempt.kind !== 'active' || this.state.kind !== 'awaitingConsent') return;
    const challenge = this.state.challenge;
    if (answer.kind === 'accept' && (challenge.kind !== 'hostKey' || !this.keyIsComplete(challenge))) return;
    this.accept.disabled = true;
    this.decline.disabled = true;
    this.secret.disabled = true;
    status(this.challengeStatus, 'Sending this response to the native SSH attempt…');
    try {
      await invokeCommand('answer_challenge', { lease: attempt.lease, challengeId: challenge.id, answer });
      if (this.current(attempt) && this.state.kind === 'awaitingConsent' && this.state.challenge.id === challenge.id) status(this.challengeStatus, 'Response sent. Waiting for SSH…');
    } catch (error: unknown) {
      if (!this.current(attempt) || this.state.kind !== 'awaitingConsent' || this.state.challenge.id !== challenge.id) return;
      status(this.challengeStatus, errorText(error), 'error');
      this.accept.disabled = challenge.kind === 'hostKey' && !this.keyIsComplete(challenge);
      this.decline.disabled = false;
      this.secret.disabled = false;
    }
  }

  private renderState(): void {
    this.pane.dataset.connectionState = this.state.kind;
    this.tab.dataset.connectionState = this.state.kind;
    this.terminal.options.disableStdin = this.state.kind !== 'connected';
    this.retry.hidden = this.intent.kind === 'hostSetup' || this.state.kind === 'connected' || this.state.kind === 'connecting' || this.state.kind === 'awaitingConsent';
    this.stop.hidden = this.state.kind === 'disconnected' || this.state.kind === 'failed';
    this.plain.hidden = !(this.state.kind === 'failed' && this.state.error.stage === 'session' && this.intent.kind !== 'plain' && this.intent.kind !== 'hostSetup');
    if (this.state.kind !== 'awaitingConsent') this.clearChallenge();
    switch (this.state.kind) {
      case 'disconnected': text(this.stateText, this.state.reason); break;
      case 'connecting': text(this.stateText, `Connecting · ${stageName(this.state.stage)}`); text(this.stop, 'Cancel'); break;
      case 'awaitingConsent':
        text(this.stateText, this.state.challenge.kind === 'hostKey' ? 'Waiting for host-key consent' : 'Waiting for authentication');
        text(this.stop, 'Cancel');
        this.showChallenge(this.state.challenge);
        break;
      case 'connected':
        text(this.stop, 'Disconnect');
        switch (this.state.mode.kind) {
          case 'plain': text(this.stateText, 'Connected · plain SSH · no process-survival guarantee'); break;
          case 'setup': text(this.stateText, `Connected · ${this.state.mode.label} · exit status alone is not verification`); break;
          case 'tmux': text(this.stateText, 'Connected · persistent tmux · reattachment is explicit'); break;
          default: exhaustive(this.state.mode);
        }
        break;
      case 'failed': text(this.stateText, errorText(this.state.error)); break;
      default: exhaustive(this.state);
    }
  }

  private fail(error: AppError): void {
    const attempt = this.attempt;
    this.epoch += 1;
    this.attempt = { kind: 'idle' };
    this.state = { kind: 'failed', error };
    this.renderState();
    if (attempt.kind === 'active') {
      void invokeCommand('terminal_disconnect', { lease: attempt.lease }).catch((cleanupError: unknown) => this.report(`Terminal cleanup failed. ${errorText(cleanupError)}`));
    }
  }

  private async cancel(): Promise<void> {
    const attempt = this.attempt;
    this.epoch += 1;
    this.attempt = { kind: 'idle' };
    this.state = { kind: 'disconnected', reason: 'Disconnected by your action. Any saved tmux reference remains; reattach explicitly.' };
    this.renderState();
    if (attempt.kind === 'active') {
      try { await invokeCommand('terminal_disconnect', { lease: attempt.lease }); }
      catch (error: unknown) { status(this.notice, `Disconnect cleanup failed. ${errorText(error)}`, 'error'); }
    }
  }

  close(): void {
    if (this.closed) return;
    const attempt = this.attempt;
    this.closed = true;
    this.epoch += 1;
    this.attempt = { kind: 'idle' };
    this.secret.value = '';
    this.observer.disconnect();
    if (this.fitFrame !== null) cancelAnimationFrame(this.fitFrame);
    for (const disposable of this.disposables) disposable.dispose();
    this.terminal.dispose();
    this.tabWrapper.remove();
    this.pane.remove();
    if (attempt.kind === 'active') {
      void invokeCommand('terminal_disconnect', { lease: attempt.lease }).catch((error: unknown) => this.report(`Closed terminal cleanup failed. ${errorText(error)}`));
    }
  }

  snapshot(): TerminalSnapshot {
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    for (let index = 0; index < buffer.length; index += 1) {
      const line = buffer.getLine(index);
      if (line !== undefined) lines.push(line.translateToString(true));
    }
    return { id: this.id, hostId: this.host.id, lease: this.attempt.kind === 'active' ? this.attempt.lease : null, state: this.state, cols: this.terminal.cols, rows: this.terminal.rows, buffer: lines.join('\n'), selection: this.terminal.getSelection(), active: this.activated };
  }
}

export class Terminals implements TerminalSmoke {
  private readonly tabs = new Map<string, TerminalTab>();
  private readonly tabList = element('div', 'terminal-tabs');
  private readonly panes = element('div', 'terminal-panes');
  private readonly empty = element('section', 'terminal-empty');
  private active: string | null = null;
  private sequence = 0;

  constructor(root: HTMLElement, private readonly context: MetadataContext, private readonly sessionChanged: () => void, private readonly report: (message: string) => void) {
    this.tabList.setAttribute('role', 'tablist');
    this.tabList.setAttribute('aria-label', 'Open remote terminals');
    this.empty.id = 'terminal-empty';
    this.empty.append(element('p', 'eyebrow', 'LOCAL HUB · STUDIO'), element('h2', '', 'Choose a host. Open a terminal.'),
      element('p', '', 'Fleet uses your installed OpenSSH and existing credentials. Discovery is separate from enrollment. Nothing connects automatically.'),
      element('p', 'help', 'Create tmux for a new persistent process. Attach verifies a saved original process. Plain SSH has no process-survival guarantee.'));
    root.append(this.tabList, this.empty, this.panes);
    this.tabList.addEventListener('keydown', event => {
      const ids = Array.from(this.tabs.keys());
      if (ids.length === 0 || this.active === null) return;
      const index = ids.indexOf(this.active);
      let target: string | undefined;
      switch (event.key) {
        case 'ArrowRight': target = ids[(index + 1) % ids.length]; break;
        case 'ArrowLeft': target = ids[(index + ids.length - 1) % ids.length]; break;
        case 'Home': target = ids[0]; break;
        case 'End': target = ids[ids.length - 1]; break;
        default: return;
      }
      event.preventDefault();
      if (target !== undefined) { this.activate(target); this.tabs.get(target)?.tab.focus(); }
    });
  }

  open(host: Host, intent: ConnectIntent): void {
    assertMetadataAvailable(this.context);
    const id = `terminal-${++this.sequence}`;
    const tab = new TerminalTab(id, host, intent, this.context, () => this.activate(id), () => this.close(id), this.sessionChanged, this.report);
    this.tabs.set(id, tab);
    this.tabList.append(tab.tabWrapper);
    this.panes.append(tab.pane);
    this.empty.hidden = true;
    tab.mount();
    this.activate(id);
    void tab.begin().catch((error: unknown) => this.report(errorText(error)));
  }

  activate(id: string): void {
    if (!this.tabs.has(id)) throw new Error('This terminal tab is no longer open.');
    this.active = id;
    for (const [tabId, tab] of this.tabs) tab.activate(tabId === id, tabId === id);
  }

  private close(id: string): void {
    const tab = this.tabs.get(id);
    if (tab === undefined) return;
    const wasActive = this.active === id;
    tab.close();
    this.tabs.delete(id);
    if (wasActive) {
      this.active = null;
      const next = this.tabs.keys().next();
      if (!next.done) this.activate(next.value);
    }
    this.empty.hidden = this.tabs.size !== 0;
  }

  snapshots(): TerminalSnapshot[] { return Array.from(this.tabs.values(), tab => tab.snapshot()); }

  paste(id: string, value: string): void {
    const tab = this.tabs.get(id);
    if (tab === undefined) throw new Error('This terminal tab is no longer open.');
    tab.terminal.paste(value);
  }

  select(id: string, column: number, row: number, length: number): void {
    const tab = this.tabs.get(id);
    if (tab === undefined) throw new Error('This terminal tab is no longer open.');
    tab.terminal.select(column, row, length);
  }
}
