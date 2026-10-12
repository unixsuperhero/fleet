import type { ConnectIntent, Host, SetupAction, SetupCheckout, SetupGuidance, SetupHarness } from './bindings';
import { assertMetadataAvailable, button, element, errorText, exhaustive, invokeCommand, status, text, type MetadataContext } from './ipc';

export interface SetupActions {
  open(host: Host, intent: ConnectIntent): void;
}

const harnesses: readonly SetupHarness[] = ['claude', 'codex', 'omp'];
const aliasPattern = /^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$/;

function harnessName(harness: SetupHarness): string {
  switch (harness) {
    case 'claude': return 'Claude';
    case 'codex': return 'Codex';
    case 'omp': return 'OMP';
    default: return exhaustive(harness);
  }
}

function loginHint(harness: SetupHarness): string {
  switch (harness) {
    case 'omp': return 'OMP sign-in commonly calls back on 54545 (Anthropic) or 1455 (OpenAI). For another provider enter the port its sign-in page reports. Leave blank for no port forwarding.';
    case 'claude':
    case 'codex': return 'Leave blank unless the CLI prints a local callback port. Then enter that exact port. Fleet never assumes one.';
    default: return exhaustive(harness);
  }
}

function defaultAlias(host: Host): string {
  const cleaned = host.input.label.toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^[^a-z0-9]+/, '').slice(0, 63);
  return cleaned;
}

/** The checkout is picked once with the OS folder picker and shared by every host panel. */
export class SetupCheckouts {
  private selected: SetupCheckout | null = null;
  private choosing = false;
  private readonly listeners = new Set<() => void>();

  current(): SetupCheckout | null { return this.selected; }
  isChoosing(): boolean { return this.choosing; }
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  }

  async choose(): Promise<'selected' | 'canceled'> {
    if (this.choosing) return 'canceled';
    this.choosing = true;
    this.notify();
    try {
      const result = await invokeCommand('choose_setup_checkout');
      switch (result.kind) {
        case 'canceled': return 'canceled';
        case 'selected': this.selected = result.value; return 'selected';
        default: return exhaustive(result);
      }
    } finally {
      this.choosing = false;
      this.notify();
    }
  }

  private notify(): void { for (const listener of this.listeners) listener(); }
}

function copyButton(command: string, output: HTMLElement): HTMLButtonElement {
  const copy = button('Copy', () => {
    navigator.clipboard.writeText(command).then(
      () => status(output, 'Command copied. Fleet did not run it.'),
      () => status(output, 'Copy was not permitted. Select the command text and copy it manually.', 'error'),
    );
  });
  copy.setAttribute('aria-label', 'Copy command');
  return copy;
}

export class SetupPanel {
  readonly node = element('details', 'host-setup');
  private readonly folderButton = button('Choose folder…');
  private readonly folderPath = element('p', 'mono setup-path');
  private readonly alias = element('input');
  private readonly aliasHelp = element('p', 'help');
  private readonly selected = new Set<SetupHarness>();
  private readonly loginHarness = element('select');
  private readonly port = element('input');
  private readonly loginHelp = element('p', 'help');
  private readonly portHints = element('span', 'actions compact');
  private readonly controls: HTMLButtonElement[] = [];
  private readonly output = element('p', 'operation-status');
  private readonly guidance = element('div', 'setup-guidance');
  private aliasTouched = false;
  private busy = false;
  private disposed = false;
  private readonly unsubscribe: () => void;

  constructor(
    private host: Host,
    private readonly checkouts: SetupCheckouts,
    private readonly context: MetadataContext,
    private readonly actions: SetupActions,
  ) {
    const summary = element('summary', '', 'Remote TUIOS + Inbox setup');
    const folder = element('div', 'setup-group');
    this.folderButton.dataset.action = 'choose-setup-checkout';
    this.folderButton.addEventListener('click', () => { void this.chooseFolder(); });
    folder.append(
      element('h4', '', 'Trusted tuios-inbox checkout'),
      element('p', 'help', 'Pick your existing, trusted tuios-inbox checkout (it must contain server.mjs and public/). Needed for Preview, Install Capture and Status. Shared by all hosts.'),
      this.folderButton, this.folderPath);

    this.alias.type = 'text';
    this.alias.maxLength = 63;
    this.alias.autocomplete = 'off';
    this.alias.spellcheck = false;
    this.alias.value = defaultAlias(host);
    this.alias.setAttribute('aria-label', `TUIOS host alias for ${host.input.label}`);
    this.alias.dataset.action = 'setup-alias';
    this.alias.addEventListener('input', () => { this.aliasTouched = true; this.alias.setCustomValidity(''); });
    const aliasField = element('label', 'field');
    aliasField.append(element('span', 'field-label', 'TUIOS host alias (ASCII letters, digits, . _ -)'), this.alias, this.aliasHelp);
    text(this.aliasHelp, 'Used in generated hosts add/test/up commands. Fleet does not run them.');

    const tools = element('div', 'setup-group');
    const toolChoices = element('div', 'setup-choices');
    for (const harness of harnesses) {
      const label = element('label', 'checkbox-label');
      const box = element('input');
      box.type = 'checkbox';
      box.dataset.harness = harness;
      box.addEventListener('change', () => { if (box.checked) this.selected.add(harness); else this.selected.delete(harness); });
      label.append(box, element('span', '', harnessName(harness)));
      toolChoices.append(label);
    }
    tools.append(element('h4', '', 'Harnesses to install'), toolChoices,
      element('p', 'help', 'Remote install adds only a missing Bun/TUIOS and the checked harnesses with vendor install commands. It never uses sudo, passwords or tokens.'));

    const steps = element('div', 'setup-group');
    steps.append(element('h4', '', 'Steps'),
      this.op('Check', 'Read-only inspection. Nothing is changed.', () => ({ kind: 'inspect' }), false),
      this.op('Install local tools', 'Installs a missing local Bun/TUIOS after native confirmation. No grants or config changes.', () => ({ kind: 'installLocalTools' }), false),
      this.op('Install selected remote tools', 'Installs only what is missing on the remote account.', () => ({ kind: 'installRemoteTools', harnesses: harnesses.filter(item => this.selected.has(item)) }), false),
      this.op('Start daemon if absent', 'Reuses a running remote daemon. Never kills or restarts one.', () => ({ kind: 'startRemoteDaemon' }), false));

    const capture = element('div', 'setup-group');
    capture.append(element('h4', '', 'Inbox capture'),
      this.op('Preview capture', 'Remote collector dry run. Changes nothing.', () => ({ kind: 'previewCapture' }), true),
      this.op('Install capture…', 'May start or update the central Inbox services and the remote collector services, using the checkout installer. It does not touch the TUIOS daemon or apply grants.', () => ({ kind: 'installCapture' }), true),
      this.op('Status', 'Runs the installer status check. Installed is not the same as a verified transcript.', () => ({ kind: 'captureStatus' }), true));

    const login = element('div', 'setup-group');
    for (const harness of harnesses) this.loginHarness.append(new Option(harnessName(harness), harness));
    this.loginHarness.setAttribute('aria-label', 'Harness to sign in');
    this.loginHarness.addEventListener('change', () => this.renderLoginHint());
    this.port.type = 'text';
    this.port.inputMode = 'numeric';
    this.port.maxLength = 5;
    this.port.autocomplete = 'off';
    this.port.placeholder = 'Callback port (optional)';
    this.port.setAttribute('aria-label', 'Optional local callback port');
    this.port.dataset.action = 'setup-port';
    this.port.addEventListener('input', () => this.port.setCustomValidity(''));
    const loginRow = element('div', 'setup-inline');
    loginRow.append(this.loginHarness, this.port);
    login.append(element('h4', '', 'Human sign-in'), loginRow, this.portHints, this.loginHelp,
      this.op('Login selected harness', 'Opens the harness CLI sign-in in a terminal tab. You sign in; Fleet reads no tokens. A port adds only a 127.0.0.1 local forward.', () => this.loginAction(), false));
    this.renderLoginHint();

    const humans = element('p', 'help setup-human', 'Console exit status never proves success. You must sign in, apply any TUIOS grants yourself, activate hooks only at a safe point, then confirm a real transcript in Inbox.');

    this.output.setAttribute('role', 'status');
    this.node.append(summary, folder, aliasField, tools, steps, capture, login, humans, this.output, this.guidance);
    this.unsubscribe = this.checkouts.subscribe(() => this.renderFolder());
    this.renderFolder();
  }

  /** Stops listening to checkout changes and refuses to open a tab for a confirmation that finishes late. */
  dispose(): void {
    this.disposed = true;
    this.unsubscribe();
  }

  setHost(host: Host): void {
    this.host = host;
    if (!this.aliasTouched) this.alias.value = defaultAlias(host);
    this.alias.setAttribute('aria-label', `TUIOS host alias for ${host.input.label}`);
  }

  private op(label: string, help: string, build: () => SetupAction | null, needsCheckout: boolean): HTMLElement {
    const row = element('div', 'setup-op');
    const control = button(label, () => { void this.run(build, needsCheckout); });
    control.dataset.action = `setup-${label.toLowerCase().replace(/[^a-z]+/g, '-')}`;
    control.title = help;
    this.controls.push(control);
    row.append(control, element('span', 'help', help));
    return row;
  }

  private loginAction(): SetupAction | null {
    const harness = harnesses.find(candidate => candidate === this.loginHarness.value);
    if (harness === undefined) return null;
    const raw = this.port.value.trim();
    if (raw === '') return { kind: 'login', harness, callbackPort: null };
    const port = Number(raw);
    if (!/^\d{1,5}$/.test(raw) || port < 1024 || port > 65535) {
      this.port.setCustomValidity('Enter a port from 1024 to 65535, or leave it blank.');
      this.port.reportValidity();
      return null;
    }
    return { kind: 'login', harness, callbackPort: port };
  }

  private renderLoginHint(): void {
    const harness = harnesses.find(candidate => candidate === this.loginHarness.value) ?? 'claude';
    text(this.loginHelp, loginHint(harness));
    this.portHints.replaceChildren();
    if (harness === 'omp') {
      for (const [port, provider] of [[54545, 'Anthropic'], [1455, 'OpenAI']] as const) {
        this.portHints.append(button(`${port} · ${provider}`, () => { this.port.value = String(port); this.port.setCustomValidity(''); }));
      }
    }
  }

  private renderFolder(): void {
    const checkout = this.checkouts.current();
    text(this.folderPath, checkout === null ? 'No folder selected' : checkout.path);
    this.folderPath.title = checkout === null ? '' : checkout.path;
    text(this.folderButton, checkout === null ? 'Choose folder…' : 'Change folder…');
    this.folderButton.disabled = this.checkouts.isChoosing() || this.busy;
  }

  private setBusy(busy: boolean): void {
    this.busy = busy;
    for (const control of this.controls) control.disabled = busy;
    this.renderFolder();
  }

  private async chooseFolder(): Promise<void> {
    try {
      const result = await this.checkouts.choose();
      status(this.output, result === 'selected' ? 'Checkout selected. Native will confirm its contents again before any launch.' : 'Folder selection canceled.');
    } catch (error: unknown) { status(this.output, errorText(error), 'error'); }
  }

  private async run(build: () => SetupAction | null, needsCheckout: boolean): Promise<void> {
    if (this.busy) return;
    const alias = this.alias.value.trim();
    if (!aliasPattern.test(alias)) {
      this.alias.setCustomValidity('Use 1-63 ASCII letters, digits, dot, underscore or hyphen, starting with a letter or digit.');
      this.alias.reportValidity();
      return;
    }
    const checkout = this.checkouts.current();
    if (needsCheckout && checkout === null) {
      status(this.output, 'Choose the trusted tuios-inbox checkout folder first.', 'error');
      return;
    }
    const action = build();
    if (action === null) return;
    this.setBusy(true);
    try {
      assertMetadataAvailable(this.context);
      const epoch = this.context.epoch();
      const host = this.host;
      status(this.output, 'Waiting for native confirmation. Nothing has run yet.');
      const prepared = await invokeCommand('prepare_host_setup', {
        hostId: host.id, checkoutId: needsCheckout && checkout !== null ? checkout.id : null, alias, action,
      });
      switch (prepared.kind) {
        case 'canceled':
          if (this.disposed) break;
          status(this.output, 'Canceled in the native confirmation. No setup terminal was opened.');
          break;
        case 'ready':
          if (this.disposed) break;
          this.renderGuidance(prepared.guidance);
          if (epoch !== this.context.epoch() || this.context.restoring()) {
            status(this.output, 'Saved metadata changed while confirming. The single-use ticket was not used. Run the step again.', 'error');
            break;
          }
          this.actions.open(host, { kind: 'hostSetup', ticketId: prepared.ticketId });
          status(this.output, `${prepared.label} opened in a terminal tab. Exit status alone is not verification; any partial or canceled run may have left changes. Retry is disabled for single-use setup. Run the step again here.`);
          break;
        default: exhaustive(prepared);
      }
    } catch (error: unknown) {
      status(this.output, errorText(error), 'error');
    } finally { this.setBusy(false); }
  }

  private renderGuidance(items: SetupGuidance[]): void {
    this.guidance.replaceChildren();
    for (const item of items) {
      const card = element('section', 'setup-card');
      card.append(element('h4', '', item.title), element('p', 'setup-detail', item.detail));
      if (item.command !== null) {
        const row = element('div', 'setup-command');
        row.append(element('code', 'mono', item.command), copyButton(item.command, this.output));
        card.append(row);
      }
      this.guidance.append(card);
    }
  }
}
