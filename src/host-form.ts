import type { Host, HostDraft, HostId } from './bindings';
import { assertMetadataAvailable, button, element, errorText, invokeCommand, status, text, type MetadataContext } from './ipc';

export class HostForm {
  private readonly details = element('details', 'host-editor');
  private readonly heading = element('summary', '', 'Add a manual host');
  private readonly form = element('form');
  private readonly fields = element('fieldset');
  private readonly label = this.input('host-label', 'Host label', 'text', 'A name for this host');
  private readonly target = this.input('ssh-target', 'SSH target', 'text', 'alias or user@hostname');
  private readonly port = this.input('host-port', 'Port override', 'number', 'Use SSH configuration');
  private readonly hostname = this.input('hostname-override', 'Hostname override', 'text', 'Use SSH configuration');
  private readonly keyAlias = this.input('hostkey-alias', 'Host-key alias override', 'text', 'Use SSH configuration');
  private readonly save = button('Save host');
  private readonly cancel = button('Clear form', () => this.newHost(false));
  private readonly output = element('p', 'operation-status');
  private editing: HostId | null = null;
  private association: string | null = null;
  private saving = false;

  constructor(root: HTMLElement, private readonly context: MetadataContext, private readonly onSaved: () => Promise<void>) {
    this.details.open = true;
    this.label.required = true;
    this.label.maxLength = 120;
    this.target.required = true;
    this.target.maxLength = 512;
    this.port.min = '1';
    this.port.max = '65535';
    this.port.step = '1';
    this.hostname.maxLength = 512;
    this.keyAlias.maxLength = 512;
    this.save.id = 'save-host';
    this.save.type = 'submit';
    this.save.className = 'primary';
    this.cancel.id = 'cancel-host-edit';
    const actions = element('div', 'actions');
    actions.append(this.save, this.cancel);
    const note = element('p', 'help', 'Blank overrides preserve your SSH alias settings. Existing SSH configuration, including proxy commands that can run local programs, remains in use. Fleet does not edit that configuration. NAS and the local Studio hub are excluded.');
    this.fields.append(actions);
    this.output.setAttribute('role', 'status');
    this.form.append(this.fields, note, this.output);
    this.form.id = 'host-form';
    this.details.append(this.heading, this.form);
    root.append(this.details);
    this.form.addEventListener('submit', event => { event.preventDefault(); void this.submit(); });
  }

  private input(id: string, label: string, type: string, placeholder: string): HTMLInputElement {
    const wrapper = element('label', 'field');
    const caption = element('span', 'field-label', label);
    const input = element('input');
    input.id = id;
    input.name = id;
    input.type = type;
    input.placeholder = placeholder;
    input.autocomplete = 'off';
    input.spellcheck = false;
    wrapper.append(caption, input);
    this.fields.append(wrapper);
    return input;
  }

  edit(host: Host): void {
    if (this.saving || this.context.restoring()) return;
    this.editing = host.id;
    this.association = host.input.tailnetNodeId;
    this.label.value = host.input.label;
    this.target.value = host.input.sshTarget;
    this.port.value = host.input.portOverride === null ? '' : String(host.input.portOverride);
    this.hostname.value = host.input.hostNameOverride ?? '';
    this.keyAlias.value = host.input.hostKeyAliasOverride ?? '';
    text(this.heading, 'Edit enrolled host');
    text(this.save, 'Save changes');
    text(this.cancel, 'Cancel edit');
    status(this.output, 'Changes apply to future connections. Existing terminals keep their current connection.');
    this.details.open = true;
    this.label.focus({ preventScroll: false });
  }

  newHost(focus = true): void {
    if (this.saving) return;
    this.editing = null;
    this.association = null;
    this.form.reset();
    text(this.heading, 'Add a manual host');
    text(this.save, 'Save host');
    text(this.cancel, 'Clear form');
    status(this.output, '');
    this.details.open = true;
    if (focus) this.label.focus();
  }

  private async submit(): Promise<void> {
    if (this.saving || !this.form.reportValidity()) return;
    try { assertMetadataAvailable(this.context); }
    catch (error: unknown) { status(this.output, errorText(error), 'error'); return; }
    const epoch = this.context.epoch();
    const portOverride = this.port.value.trim() === '' ? null : this.port.valueAsNumber;
    if (portOverride !== null && (!Number.isInteger(portOverride) || portOverride < 1 || portOverride > 65535)) {
      status(this.output, 'Port must be an integer between 1 and 65535.', 'error');
      return;
    }
    const draft: HostDraft = {
      label: this.label.value.trim(), sshTarget: this.target.value.trim(), portOverride,
      hostNameOverride: this.hostname.value.trim() || null,
      hostKeyAliasOverride: this.keyAlias.value.trim() || null,
      tailnetNodeId: this.association,
    };
    this.saving = true;
    this.fields.disabled = true;
    status(this.output, 'Saving host metadata…');
    try {
      await invokeCommand('save_host', { hostId: this.editing, draft });
      if (epoch !== this.context.epoch()) {
        status(this.output, 'Metadata changed during this save. Your form was preserved; review it before saving again.', 'error');
        return;
      }
      await this.onSaved();
      if (epoch !== this.context.epoch()) return;
      this.editing = null;
      this.association = null;
      this.form.reset();
      text(this.heading, 'Add a manual host');
      text(this.save, 'Save host');
      text(this.cancel, 'Clear form');
      status(this.output, 'Host saved. Choose a terminal action to connect.');
    } catch (error: unknown) {
      if (epoch === this.context.epoch()) status(this.output, errorText(error), 'error');
    } finally {
      this.saving = false;
      this.fields.disabled = false;
    }
  }
}
