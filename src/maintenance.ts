import type { AppInfo, DiagnosticBundle, OperationResult, RestorePreview } from './bindings';
import { button, confirmAction, element, errorText, exhaustive, invokeCommand, status, text } from './ipc';

interface MaintenanceActions {
  restoreStarted(): void;
  restoreFinished(): Promise<void>;
  reloadInfo(): Promise<void>;
}
type DiagnosticPreview = { kind: 'none' } | { kind: 'ready'; includeHostMetadata: boolean; bundle: DiagnosticBundle };

function operationMessage(result: OperationResult, completed: string): string {
  switch (result.kind) {
    case 'canceled': return 'Canceled. Current metadata was not changed.';
    case 'completed': return `${completed}\n${result.path}`;
    default: return exhaustive(result);
  }
}

export class Maintenance {
  private readonly dialog = element('dialog', 'maintenance-dialog');
  private readonly info = element('dl', 'app-info');
  private readonly output = element('p', 'operation-status');
  private readonly diagnosticOutput = element('pre', 'diagnostic-preview');
  private readonly metadata = element('input');
  private readonly chooseDirectory = button('Choose backup directory…');
  private readonly exportBackup = button('Export SQLite backup…');
  private readonly restore = button('Select restore backup…');
  private readonly preview = button('Preview diagnostics');
  private readonly exportDiagnostics = button('Save reviewed diagnostics…');
  private readonly close = button('Close', () => { if (!this.busy) this.dialog.close(); });
  private previewState: DiagnosticPreview = { kind: 'none' };
  private previewEpoch = 0;
  private busy = false;

  constructor(private readonly actions: MaintenanceActions) {
    this.dialog.id = 'maintenance-dialog';
    const header = element('div', 'section-heading');
    header.append(element('h2', '', 'Backup, recovery & diagnostics'), this.close);
    const backup = element('section', 'maintenance-section');
    backup.append(element('h3', '', 'Metadata backup'), element('p', 'help', 'Backups contain enrolled SSH targets and saved session references, not passwords, keys, terminal output or remote processes. Treat hostnames and usernames as private.'));
    const backupActions = element('div', 'actions');
    this.chooseDirectory.id = 'choose-backup-directory';
    this.exportBackup.id = 'export-backup';
    this.restore.id = 'choose-restore';
    backupActions.append(this.chooseDirectory, this.exportBackup, this.restore);
    backup.append(backupActions);
    const diagnostics = element('section', 'maintenance-section');
    diagnostics.append(element('h3', '', 'Local diagnostics'), element('p', 'help', 'Review the bundle before saving it. No cloud upload is performed. Terminal content, authentication responses and private keys are excluded.'));
    const metadataLabel = element('label', 'checkbox-label');
    this.metadata.id = 'include-host-metadata';
    this.metadata.type = 'checkbox';
    metadataLabel.append(this.metadata, element('span', '', 'Include enrolled hostnames, usernames and host metadata in this preview and export'));
    const diagnosticActions = element('div', 'actions');
    this.preview.id = 'preview-diagnostics';
    this.exportDiagnostics.id = 'export-diagnostics';
    this.exportDiagnostics.disabled = true;
    diagnosticActions.append(this.preview, this.exportDiagnostics);
    this.diagnosticOutput.id = 'diagnostic-preview';
    this.diagnosticOutput.tabIndex = 0;
    this.diagnosticOutput.setAttribute('aria-label', 'Diagnostic bundle preview');
    text(this.diagnosticOutput, 'Preview diagnostics to review the exact metadata option before saving.');
    diagnostics.append(metadataLabel, diagnosticActions, this.diagnosticOutput);
    this.output.setAttribute('role', 'status');
    this.dialog.append(header, this.info, backup, diagnostics, this.output);
    document.body.append(this.dialog);
    this.dialog.addEventListener('cancel', event => { if (this.busy) event.preventDefault(); });
    this.chooseDirectory.addEventListener('click', () => {
      void this.run(async () => {
        const result = await invokeCommand('choose_backup_directory');
        await this.actions.reloadInfo();
        status(this.output, operationMessage(result, 'Backup directory updated.'));
      });
    });
    this.exportBackup.addEventListener('click', () => {
      void this.run(async () => status(this.output, operationMessage(await invokeCommand('export_backup'), 'Consistent SQLite backup exported.')));
    });
    this.restore.addEventListener('click', () => { void this.run(() => this.restoreBackup()); });
    this.metadata.addEventListener('change', () => {
      this.previewEpoch += 1;
      this.previewState = { kind: 'none' };
      this.exportDiagnostics.disabled = true;
      text(this.diagnosticOutput, 'The host-metadata option changed. Preview again before saving.');
    });
    this.preview.addEventListener('click', () => {
      void this.run(async () => {
        const epoch = ++this.previewEpoch;
        const includeHostMetadata = this.metadata.checked;
        const bundle = await invokeCommand('diagnostics_preview', { includeHostMetadata });
        if (epoch !== this.previewEpoch || includeHostMetadata !== this.metadata.checked) return;
        this.previewState = { kind: 'ready', includeHostMetadata, bundle };
        text(this.diagnosticOutput, JSON.stringify(bundle, null, 2));
        status(this.output, includeHostMetadata ? 'Preview includes host metadata by your explicit choice. Review it before saving.' : 'Preview excludes host metadata. Review it before saving.');
      });
    });
    this.exportDiagnostics.addEventListener('click', () => {
      void this.run(async () => {
        const preview = this.previewState;
        if (preview.kind !== 'ready' || preview.includeHostMetadata !== this.metadata.checked) throw new Error('Preview the current metadata option before saving diagnostics.');
        const result = await invokeCommand('export_diagnostics', { includeHostMetadata: preview.includeHostMetadata });
        status(this.output, operationMessage(result, 'Reviewed diagnostic option exported. The native bundle reflects the latest bounded entries.'));
      });
    });
  }

  open(): void {
    if (!this.dialog.open) this.dialog.showModal();
  }

  updateInfo(info: AppInfo): void {
    const entries: [string, string][] = [
      ['Fleet', info.version], ['Schema', String(info.schemaVersion)],
      ['Platform', `${info.platform} · ${info.arch}`], ['Database', info.databasePath],
      ['Backup directory', info.backupDirectory], ['OpenSSH', info.sshVersion ?? 'Not found'],
      ['Tailscale', info.tailscaleVersion ?? 'Not found or unavailable'],
    ];
    if (this.info.childElementCount === 0) {
      for (const [label, value] of entries) this.info.append(element('dt', '', label), element('dd', 'mono', value));
      return;
    }
    for (let index = 0; index < entries.length; index += 1) {
      const entry = entries[index];
      const node = this.info.children.item(index * 2 + 1);
      if (entry !== undefined && node instanceof HTMLElement) text(node, entry[1]);
    }
  }

  private async run(action: () => Promise<void>): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    this.setControls();
    this.dialog.setAttribute('aria-busy', 'true');
    status(this.output, 'Waiting for the native operation…');
    try { await action(); }
    catch (error: unknown) { status(this.output, errorText(error), 'error'); }
    finally {
      this.busy = false;
      this.setControls();
      this.dialog.removeAttribute('aria-busy');
    }
  }

  private setControls(): void {
    for (const control of [this.chooseDirectory, this.exportBackup, this.restore, this.preview, this.close]) control.disabled = this.busy;
    this.metadata.disabled = this.busy;
    this.exportDiagnostics.disabled = this.busy || this.previewState.kind !== 'ready' || this.previewState.includeHostMetadata !== this.metadata.checked;
  }

  private async restoreBackup(): Promise<void> {
    const choice = await invokeCommand('choose_restore');
    switch (choice.kind) {
      case 'canceled': status(this.output, 'Restore selection canceled. Current data is unchanged.'); return;
      case 'selected': await this.confirmRestore(choice.value); return;
      default: exhaustive(choice);
    }
  }

  private async confirmRestore(preview: RestorePreview): Promise<void> {
    const approved = await confirmAction('Replace Fleet metadata?', `Validated backup:\n${preview.path}\n\nSchema ${preview.schemaVersion} · ${preview.hosts} hosts · ${preview.sessions} saved sessions\n\nThis replaces enrolled hosts and saved session references. The native operation first backs up current metadata. Existing terminal nodes and remote processes remain in place. SSH credentials and remote processes are not restored.`, 'Restore this backup', true);
    if (!approved) {
      await invokeCommand('cancel_restore', { ticketId: preview.ticketId });
      status(this.output, 'Staged restore canceled. Current metadata is unchanged.');
      return;
    }
    this.actions.restoreStarted();
    try {
      const result = await invokeCommand('commit_restore', { ticketId: preview.ticketId });
      this.previewEpoch += 1;
      this.previewState = { kind: 'none' };
      text(this.diagnosticOutput, 'Metadata changed. Preview diagnostics again before saving.');
      status(this.output, operationMessage(result, 'Metadata restored. Open terminals were kept in place; reattachment is still explicit.'));
    } finally {
      await this.actions.restoreFinished();
    }
  }
}
