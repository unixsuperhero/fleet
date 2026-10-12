import type { AppError, ErrorCode, Stage } from './bindings';
export { invokeCommand } from './bindings';

export interface MetadataContext {
  epoch(): number;
  restoring(): boolean;
}

const stageNames: Record<Stage, string> = {
  local: 'Local', resolve: 'DNS', tcp: 'TCP', hostKey: 'Host key',
  auth: 'Authentication', session: 'Session', storage: 'Storage', discovery: 'Discovery',
};
const stages: Stage[] = ['local', 'resolve', 'tcp', 'hostKey', 'auth', 'session', 'storage', 'discovery'];
const codes: ErrorCode[] = [
  'invalidInput', 'unavailable', 'missing', 'declined', 'changed', 'authentication',
  'connectionLost', 'connectionFailed', 'canceled', 'busy', 'timeout', 'corruptBackup',
  'newerSchema', 'migrationFailed', 'storage', 'ended', 'unknown',
];

export function stageName(stage: Stage): string { return stageNames[stage]; }

export function nativeError(value: unknown): AppError {
  if (typeof value === 'object' && value !== null && 'stage' in value && 'code' in value && 'message' in value) {
    const stage = stages.find(candidate => candidate === value.stage);
    const code = codes.find(candidate => candidate === value.code);
    if (stage !== undefined && code !== undefined && typeof value.message === 'string') {
      return { stage, code, message: value.message };
    }
  }
  return {
    stage: 'local', code: 'unknown',
    message: value instanceof Error ? value.message : typeof value === 'string' ? value : 'The native operation failed. Open Fleet in its desktop application.',
  };
}

export function errorText(value: unknown): string {
  const error = nativeError(value);
  return `${stageName(error.stage)} · ${error.code}: ${error.message}`;
}

export function exhaustive(value: never): never {
  throw new Error(`Unsupported protocol variant: ${String(value)}`);
}

export function assertMetadataAvailable(context: MetadataContext): void {
  if (context.restoring()) {
    throw { stage: 'storage', code: 'busy', message: 'Metadata restore is in progress. Wait before changing hosts or sessions.' } satisfies AppError;
  }
}

export function element<K extends keyof HTMLElementTagNameMap>(tag: K, className = '', text = ''): HTMLElementTagNameMap[K] {
  const result = document.createElement(tag);
  result.className = className;
  if (text !== '') result.textContent = text;
  return result;
}

export function text(node: HTMLElement, value: string): void {
  if (node.textContent !== value) node.textContent = value;
}

export function button(label: string, action?: () => void): HTMLButtonElement {
  const result = element('button', '', label);
  result.type = 'button';
  if (action !== undefined) result.addEventListener('click', action);
  return result;
}

export function status(node: HTMLElement, message: string, tone: 'normal' | 'error' = 'normal'): void {
  text(node, message);
  node.dataset.tone = tone;
}

export async function perform(control: HTMLButtonElement, output: HTMLElement, action: () => Promise<void>): Promise<void> {
  if (control.disabled) return;
  control.disabled = true;
  control.setAttribute('aria-busy', 'true');
  try { await action(); }
  catch (error: unknown) { status(output, errorText(error), 'error'); }
  finally {
    control.disabled = false;
    control.removeAttribute('aria-busy');
  }
}

export const plainWarning = 'Plain SSH has no process-survival guarantee. Closing this terminal, losing the connection or restarting Fleet can end its remote shell and programs. Fleet will not reconnect automatically.';

export function confirmAction(title: string, message: string, acceptLabel: string, dangerous = false): Promise<boolean> {
  const dialog = element('dialog', 'confirmation-dialog');
  const heading = element('h2', '', title);
  const description = element('p', 'confirmation-copy', message);
  const actions = element('div', 'actions');
  const cancel = button('Cancel');
  const accept = button(acceptLabel);
  accept.className = dangerous ? 'danger' : 'primary';
  actions.append(cancel, accept);
  dialog.append(heading, description, actions);
  document.body.append(dialog);
  return new Promise(resolve => {
    let settled = false;
    const finish = (value: boolean): void => {
      if (settled) return;
      settled = true;
      dialog.close();
      dialog.remove();
      resolve(value);
    };
    cancel.addEventListener('click', () => finish(false));
    accept.addEventListener('click', () => finish(true));
    dialog.addEventListener('cancel', event => { event.preventDefault(); finish(false); });
    dialog.addEventListener('close', () => finish(false));
    dialog.showModal();
    cancel.focus();
  });
}
