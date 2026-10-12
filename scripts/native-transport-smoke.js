(() => {
  if (window.fleetNativeSmoke) return;
  const invoke = (command, args = {}) => window.__TAURI__.core.invoke(command, args);
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const assert = (value, message) => { if (!value) throw new Error(message); };
  const wait = async (read, message, timeout = 30000) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) { const value = await read(); if (value) return value; await sleep(50); }
    throw new Error(message);
  };
  const panel = document.createElement('aside');
  panel.id = 'fleet-transport-smoke';
  panel.style.cssText = 'position:fixed;bottom:8px;right:8px;z-index:10000;width:520px;max-height:40vh;overflow:auto;background:#171a1c;color:#e6c07b;border:1px solid #bfa36c;padding:12px;font:12px monospace';
  const status = document.createElement('pre');
  status.style.whiteSpace = 'pre-wrap';
  const input = document.createElement('textarea');
  input.rows = 5;
  input.style.cssText = 'width:100%;box-sizing:border-box;background:#222;color:#eee';
  input.setAttribute('aria-label', 'Native smoke options JSON');
  input.value = JSON.stringify({ phase: 'fixture' }, null, 2);
  const start = document.createElement('button');
  start.textContent = 'Run native transport scenario';
  panel.append(status, input, start);
  document.body.append(panel);
  const notice = text => { status.textContent = text; };
  notice('Paste options JSON with phase and actual fixture manifest. Phases: fixture, real, restart, dialogs, ended. Native file panels require actual choices; no paths are injected into picker results.');
  let running = false;
  const counter = (buffer, token) => {
    let latest = null;
    for (const line of buffer.split(/\r?\n/)) {
      const fields = line.trimEnd().split('|');
      if (fields.length === 4 && fields[0] === 'FLEET_COUNT' && fields[1] === token && /^\d+$/.test(fields[2]) && /^\d+$/.test(fields[3])) {
        const value = { pid: Number(fields[2]), count: Number(fields[3]) };
        if (!latest || value.count > latest.count) latest = value;
      }
    }
    return latest;
  };
  const shellQuote = value => "'" + value.replaceAll("'", "'\\''") + "'";
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const pathKey = value => value.replace(/^\/private\/tmp\//, '/tmp/');
  const visible = element => element.getClientRects().length > 0;
  const button = (root, pattern) => {
    const matches = [...root.querySelectorAll('button')].filter(item => !item.disabled && visible(item) && pattern.test(item.textContent.trim()));
    assert(matches.length === 1, `Expected one visible ${pattern} action, found ${matches.length}`);
    return matches[0];
  };
  const projectedSessions = sessions => sessions.map(({ status: ignored, ...session }) => session).sort((a, b) => a.id.localeCompare(b.id));
  async function run(options) {
    assert(!running, 'A smoke scenario is already running');
    running = true; start.disabled = true;
    const proof = { native: true, scenario: `native-transport-${options.phase}`, phase: options.phase, startedAt: new Date().toISOString(), observations: [], limitations: [], processId: options.processId ?? null };
    const passed = (behavior, evidence = {}) => proof.observations.push({ behavior, value: 'passed', ...evidence });
    let api;
    let tag;
    try {
      assert(window.__TAURI__?.core?.invoke, 'This script requires the actual Tauri WKWebView');
      api = await wait(() => window.fleetSmoke, 'Debug-only actual xterm smoke access did not mount');
      for (const name of ['snapshots', 'paste', 'select', 'activate']) assert(typeof api[name] === 'function', `fleetSmoke.${name} is required; no fake terminal fallback exists`);
      await wait(() => document.getElementById('host-label') && document.getElementById('ssh-target'), 'Semantic host form did not mount');
      proof.appInfo = await invoke('app_info');
      const fixture = options.fixture;
      const previous = options.previousProof;
      assert(fixture?.profile && fixture.root?.startsWith('/tmp/fleet-smoke-') && pathKey(proof.appInfo.databasePath).startsWith(pathKey(fixture.profile) + '/'), 'Every scenario requires the generated fixture manifest and its isolated native profile');
      tag = previous?.checkpoint?.tag ?? `native-${Date.now().toString(36)}`;
      const tabs = () => api.snapshots();
      const live = id => { const tab = tabs().find(item => item.id === id); assert(tab, `Terminal ${id} disappeared`); return tab; };
      const buffer = id => live(id).buffer;
      const raw = (id, bytes) => {
        const lease = live(id).lease;
        assert(lease, `Terminal ${id} has no active lease`);
        return invoke('terminal_input', { lease, bytes });
      };
      // Scenario commands are native PTY input, not bracketed paste.
      // The separate Unicode exercise below uses the actual xterm paste API.
      const send = (id, text) => raw(id, Array.from(new TextEncoder().encode(text)));
      const line = (id, text) => buffer(id).split(/\r?\n/).some(item => item.trimEnd() === text);
      const hostSelector = id => (options.selectors?.hostRow ?? '.host-row[data-host-id="{id}"]').replaceAll('{id}', id);
      const hostRow = host => { const row = document.querySelector(hostSelector(host.id)); assert(row, `Host row missing: ${hostSelector(host.id)}`); return row; };
      const editField = (id, value, required = true) => {
        const field = document.getElementById(id);
        assert(field || !required, `Input #${id} is required`);
        if (field) { field.value = value ?? ''; field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new Event('change', { bubbles: true })); }
        return field;
      };
      const enroll = async (suffix, draft) => {
        const label = `${tag}-${suffix}`;
        const existing = (await invoke('list_hosts')).find(host => host.input.label === label);
        if (existing) { assert(same(existing.input, { ...draft, label }), 'Existing smoke host differs from its SSH binding'); return existing; }
        const reset = document.querySelector(options.selectors?.newHost ?? '#new-host');
        if (reset) reset.click();
        editField('host-label', label); editField('ssh-target', draft.sshTarget);
        editField('hostname-override', draft.hostNameOverride, Boolean(draft.hostNameOverride));
        editField('hostkey-alias', draft.hostKeyAliasOverride, Boolean(draft.hostKeyAliasOverride));
        editField('host-port', draft.portOverride);
        const form = document.getElementById('host-label').closest('form'); assert(form, 'Host inputs are not in an actual form'); form.requestSubmit();
        const host = await wait(async () => (await invoke('list_hosts')).find(item => item.input.label === label), `Native host save did not complete for ${label}`);
        assert(same(host.input, { ...draft, label }), 'Form did not save exact SSH alias/overrides');
        await wait(() => document.querySelector(hostSelector(host.id)) && !document.getElementById('host-label').closest('fieldset').disabled, 'Inventory/form did not finish saving native host');
        return host;
      };
      const draft = (sshTarget, hostNameOverride = null, hostKeyAliasOverride = null) => ({ label: '', sshTarget, portOverride: null, hostNameOverride, hostKeyAliasOverride, tailnetNodeId: null });
      const begin = async (host, persistent = false) => {
        const old = new Set(tabs().map(item => item.id));
        const row = hostRow(host);
        if (persistent) {
          const field = row.querySelector('[data-action="session-label"]');
          assert(field, 'Actual inline persistent session label is missing');
          field.value = `${tag}-counter`;
          field.dispatchEvent(new Event('input', { bubbles: true }));
          const form = field.closest('form.session-create');
          assert(form, 'Actual inline persistent session form is missing');
          form.requestSubmit();
        } else {
          const action = row.querySelector('[data-action="plain-terminal"]');
          assert(action && !action.disabled, 'Actual Plain SSH action is unavailable');
          action.click();
          const dialog = await wait(() => [...document.querySelectorAll('dialog.confirmation-dialog[open]')].find(item => visible(item) && item.querySelector('h2')?.textContent === 'Open a plain SSH terminal?'), 'Plain SSH warning did not appear');
          button(dialog, /^Open plain SSH$/).click();
        }
        return wait(() => tabs().find(item => item.hostId === host.id && !old.has(item.id)), `UI did not allocate a new independent terminal for ${host.input.label}`);
      };
      const ready = id => wait(() => {
        const tab = live(id);
        if (tab.state.kind === 'failed') throw new Error(`${tab.state.error.stage}/${tab.state.error.code}: ${tab.state.error.message}`);
        return tab.state.kind === 'connected' && tab;
      }, 'SSH terminal did not become connected');
      const disconnect = async id => {
        const before = live(id);
        await invoke('terminal_disconnect', { lease: before.lease });
        await wait(() => ['disconnected', 'failed'].includes(live(id).state.kind), 'Disconnect did not settle', 5000);
      };
      const marker = async (id, token) => {
        await send(id, `printf '%s%s|%s\\n' 'FLEET_' 'OUT' '${token}'\r`);
        await wait(() => line(id, `FLEET_OUT|${token}`), `Actual xterm buffer did not receive ${token}`);
      };
      const consent = async (id, accept) => {
        assert(/^SHA256:[A-Za-z0-9+/]+$/.test(fixture.hostKeyFingerprint), 'Expected fingerprint must come from generated hostkey.pub manifest');
        const tab = await wait(() => {
          const item = live(id);
          assert(item.state.kind !== 'connected', 'Host-key consent was bypassed');
          if (item.state.kind === 'failed') throw new Error(`Host-key attempt failed before consent: ${item.state.error.code}`);
          return item.state.kind === 'awaitingConsent' && item;
        }, 'Unknown host key did not request dedicated consent');
        const challenge = tab.state.challenge;
        assert(challenge.kind === 'hostKey' && challenge.fingerprint === fixture.hostKeyFingerprint && challenge.algorithm?.toLowerCase().replace(/^ssh-/, '') === 'ed25519', 'Challenge did not contain actual generated fingerprint and algorithm');
        const dialog = await wait(() => [...document.querySelectorAll('dialog[open], [role="dialog"]')].find(item => visible(item) && item.textContent.includes(fixture.hostKeyFingerprint)), 'Dedicated fingerprint dialog was not rendered');
        api.activate(id);
        api.paste(id, 'yes');
        try { await raw(id, [13]); } catch { /* Native PTY input must not answer consent. */ }
        await sleep(150);
        assert(live(id).state.kind === 'awaitingConsent', 'Terminal paste authorized an unknown key');
        button(dialog, accept ? /^Accept verified key$/ : /^(Decline|Cancel|Reject)$/i).click();
        if (accept) await ready(id);
        else await wait(() => ['failed', 'disconnected'].includes(live(id).state.kind), 'Declined challenge did not settle', 5000);
        passed(accept ? 'Dedicated explicit trust of independently generated fingerprint' : 'Unknown-key decline and terminal-paste bypass prevention', { fingerprint: challenge.fingerprint, algorithm: challenge.algorithm });
      };
      const discoveryProof = async () => {
        const label = document.getElementById('host-label');
        const original = label.value;
        const selection = { start: label.selectionStart, end: label.selectionEnd };
        const form = label.closest('form');
        const beforeHosts = await invoke('list_hosts');
        const beforeTabs = tabs().map(item => ({ id: item.id, lease: item.lease }));
        label.value = `${tag}-typed-not-saved`;
        label.dispatchEvent(new Event('input', { bubbles: true }));
        label.focus(); label.setSelectionRange(2, 7);
        const discovery = options.selectors?.discover ? document.querySelector(options.selectors.discover) : button(document, /^(Discover|Refresh|Refresh discovery|Discover peers)$/i);
        assert(discovery, 'Actual discovery action was not found');
        discovery.click();
        await wait(() => !discovery.disabled, 'Discovery UI did not complete');
        await sleep(300);
        assert(label.isConnected && label.closest('form') === form && label.value === `${tag}-typed-not-saved` && document.activeElement === label && label.selectionStart === 2 && label.selectionEnd === 7, 'Discovery lost actual typing/focus/selection');
        assert(same(beforeHosts, await invoke('list_hosts')) && same(beforeTabs, tabs().map(item => ({ id: item.id, lease: item.lease }))), 'Discovery changed enrollment or terminal leases');
        label.value = original;
        label.dispatchEvent(new Event('input', { bubbles: true }));
        if (selection.start !== null) label.setSelectionRange(selection.start, selection.end);
        passed('Discovery refresh preserves form node/focus/selection and native metadata');
      };
      const terminalExercise = async (a, b) => {
        const savedSurfaceStyles = new Map();
        const fittedSurface = async (id, dimensions) => {
          api.activate(id);
          await wait(() => live(id).active && document.getElementById(`panel-${id}`)?.getClientRects().length, 'Terminal pane did not activate', 5000);
          const pane = document.getElementById(`panel-${id}`);
          const surface = pane.querySelector('.terminal-surface');
          const terminal = surface?.querySelector('.xterm');
          const screen = surface?.querySelector('.xterm-screen');
          assert(surface && terminal && screen, 'Actual terminal viewport was not mounted');
          if (!savedSurfaceStyles.has(surface)) savedSurfaceStyles.set(surface, surface.getAttribute('style'));
          const before = live(id);
          const rect = screen.getBoundingClientRect();
          const cellWidth = rect.width / before.cols;
          const cellHeight = rect.height / before.rows;
          assert(cellWidth > 0 && cellHeight > 0, 'Actual xterm cells have no rendered dimensions');
          const style = getComputedStyle(terminal);
          const px = name => Number.parseFloat(style.getPropertyValue(name)) || 0;
          const horizontal = px('padding-left') + px('padding-right');
          const vertical = px('padding-top') + px('padding-bottom');
          Object.assign(surface.style, {
            flex: '0 0 auto', alignSelf: 'flex-start', padding: '0',
            width: `${Math.ceil((dimensions.cols + 0.5) * cellWidth + horizontal + 14)}px`,
            height: `${Math.ceil((dimensions.rows + 0.5) * cellHeight + vertical)}px`,
          });
          await wait(() => {
            const current = live(id);
            return current.active && current.cols === dimensions.cols && current.rows === dimensions.rows;
          }, `FitAddon did not produce ${dimensions.cols}x${dimensions.rows}`, 5000);
        };
        try {
          await marker(a.id, `${tag}-a`); await marker(b.id, `${tag}-b`);
          assert(!line(b.id, `FLEET_OUT|${tag}-a`) && !line(a.id, `FLEET_OUT|${tag}-b`), 'Output crossed independent connections');
          for (const [tab, dimensions] of [[a, { cols: 91, rows: 29 }], [b, { cols: 73, rows: 23 }]]) {
            await fittedSurface(tab.id, dimensions);
            await marker(tab.id, `${tag}-fit-${dimensions.cols}`);
            const selected = `FLEET_OUT|${tag}-${tab.id === a.id ? 'a' : 'b'}`;
            const row = buffer(tab.id).split(/\r?\n/).findIndex(item => item.trimEnd() === selected);
            assert(row >= 0, 'Real terminal output for selection is missing');
            api.select(tab.id, 0, row, selected.length);
            assert(live(tab.id).active && live(tab.id).selection === selected, 'Actual xterm activation/selection failed');
            passed('Actual xterm activation and output selection', { tabId: tab.id, selected });
            const token = `${tag}-${dimensions.cols}`;
            await send(tab.id, `printf '%s%s|%s|' 'FLEET_' 'SIZE' '${token}'; stty size\r`);
            await wait(() => line(tab.id, `FLEET_SIZE|${token}|${dimensions.rows} ${dimensions.cols}`), 'Remote PTY did not receive independent resize');
            const unicode = '日本語 🌊 café';
            await send(tab.id, `printf '%s%s|%s\\n' 'FLEET_' 'READ_READY' '${token}'; read -r fleet_smoke_value; printf '%s%s|%s\\n' 'FLEET_' 'PASTE' "$fleet_smoke_value"\r`);
            await wait(() => line(tab.id, `FLEET_READ_READY|${token}`), 'Remote read command did not start');
            api.paste(tab.id, unicode);
            await wait(() => line(tab.id, unicode), 'Actual paste did not reach the remote PTY echo');
            await raw(tab.id, [13]);
            await wait(() => line(tab.id, `FLEET_PASTE|${unicode}`), 'Actual xterm paste did not round-trip Unicode');
            await send(tab.id, `printf '%s%s|%s|' 'FLEET_' 'SPLIT' '${token}'; printf '\\360'; sleep 0.1; printf '\\237'; sleep 0.1; printf '\\214'; sleep 0.1; printf '\\212'; printf '\\n'\r`);
            await wait(() => line(tab.id, `FLEET_SPLIT|${token}|🌊`), 'UTF-8 split across output writes was corrupted');
            passed('Real xterm paste, split Unicode and native IPC remote PTY resize', { connectionId: live(tab.id).lease.connectionId, dimensions });
          }
          proof.limitations.push('Viewport styles exercise the actual ResizeObserver/FitAddon/xterm-to-native resize path. This is not an OS-window resize. Scenario commands use native terminal_input; Unicode paste separately exercises xterm paste.');
          await send(a.id, "printf '%s%s\\n' 'Permission ' 'denied' 'WARNING: REMOTE HOST IDENTIFICATION ' 'HAS CHANGED!' >&2\r");
          await marker(a.id, `${tag}-after-spoof`);
          assert(live(a.id).state.kind === 'connected', 'Remote stderr spoof text altered connection state');
          passed('Remote client-looking stderr does not alter native connection stage');
          let clipboard;
          try { clipboard = await navigator.clipboard.readText(); }
          catch { proof.limitations.push('OSC52 clipboard unchanged cannot be proved: WKWebView clipboard read permission is unavailable. Parent may compare pbpaste in memory without logging clipboard contents.'); }
          await send(a.id, "printf '\\033]52;c;RkxFRVRfU01PS0VfT1NDNTI=\\007'\r");
          await marker(a.id, `${tag}-after-osc52`);
          if (clipboard !== undefined) {
            assert(await navigator.clipboard.readText() === clipboard, 'Remote OSC52 modified the clipboard');
            passed('Remote OSC52 leaves actual system clipboard unchanged');
          }
          await send(a.id, 'yes FLEET_SMOKE_OUTPUT_LOAD\r');
          await wait(() => line(a.id, 'FLEET_SMOKE_OUTPUT_LOAD'), 'Remote output load was not observed');
          await sleep(250);
          const started = Date.now();
          await disconnect(a.id);
          assert(Date.now() - started < 6000, 'Output-load cancellation blocked disposal');
          await marker(b.id, `${tag}-survivor`);
          assert(live(b.id).state.kind === 'connected', 'Canceling one connection canceled the other');
          passed('Output-load cancellation preserves second real terminal', { elapsedMs: Date.now() - started });
          proof.limitations.push('Cancellation under output load is exercised, but without native queue accounting/ACK-pause access the exact 512 KiB bound is not proved.');
        } finally {
          for (const [surface, original] of savedSurfaceStyles) {
            if (original === null) surface.removeAttribute('style');
            else surface.setAttribute('style', original);
          }
        }
      };
      const createCounter = async host => {
        const tab = await begin(host, true); await ready(tab.id);
        const saved = await wait(async () => {
          const state = live(tab.id).state;
          return state.kind === 'connected' && state.mode.kind === 'tmux' && (await invoke('list_sessions', { hostId: host.id })).find(item => item.id === state.mode.sessionId);
        }, 'Persistent session metadata was not saved');
        assert(saved.identity && /^fleet-[a-f0-9]{32}$/.test(saved.remoteSessionName) && same(saved.sshBinding, host.input), 'Saved tmux identity/name/binding is incomplete');
        const token = `${tag}-counter`;
        await send(tab.id, `sh -c 'i=0; while :; do printf "%s%s|%s|%s|%s\\n" FLEET_ COUNT ${token} "$$" "$i"; i=$((i+1)); sleep 1; done'\r`);
        const first = await wait(() => { const item = counter(buffer(tab.id), token); return item?.count >= 3 && item; }, 'Remote counter did not advance');
        await discoveryProof();
        const observation = await wait(() => { const item = counter(buffer(tab.id), token); return item && item.pid === first.pid && item.count > first.count && item; }, 'Discovery stopped the live original counter');
        passed('Discovery while typing preserves advancing original remote counter', { before: first, after: observation });
        proof.checkpoint = { tag, token, host, session: saved, counter: observation, hosts: await invoke('list_hosts'), sessions: await invoke('list_sessions', { hostId: null }), processId: options.processId ?? null };
        await disconnect(tab.id);
        passed('Original counter observed then disconnected without destroying tmux', { sessionId: saved.id, identity: saved.identity, counter: observation });
        proof.limitations.push('This phase does not prove app restart; launch a new native process and run restart with this checkpoint.');
      };
      const attachCheckpoint = async requireRestart => {
        const checkpoint = previous?.checkpoint;
        assert(checkpoint, 'Previous actual counter proof is required');
        if (requireRestart) assert(Number.isInteger(options.processId) && Number.isInteger(checkpoint.processId) && options.processId > 0 && options.processId !== checkpoint.processId, 'Different native OS process IDs from launcher required; WebView reload is not app restart');
        const actualHosts = await invoke('list_hosts');
        for (const host of checkpoint.hosts) assert(actualHosts.some(item => same(item, host)), 'Manual host metadata changed across restart');
        const saved = (await invoke('list_sessions', { hostId: checkpoint.host.id })).find(item => item.id === checkpoint.session.id);
        assert(saved && same(saved.identity, checkpoint.session.identity) && same(saved.sshBinding, checkpoint.session.sshBinding), 'Saved original session identity changed');
        const selector = (options.selectors?.sessionRow ?? '[data-session-id="{id}"]').replaceAll('{id}', saved.id);
        const row = await wait(() => document.querySelector(selector), 'Saved native session was not displayed after restart');
        const old = new Set(tabs().map(item => item.id));
        button(row, /^(Attach|Reattach)$/i).click();
        const tab = await wait(() => tabs().find(item => item.hostId === checkpoint.host.id && !old.has(item.id)), 'Attach UI did not allocate terminal');
        await ready(tab.id);
        const observation = await wait(() => {
          const item = counter(buffer(tab.id), checkpoint.token);
          return item && item.pid === checkpoint.counter.pid && item.count > checkpoint.counter.count + 2 && item;
        }, 'Original PID/counter progress did not survive disconnect and restart');
        const refreshed = (await invoke('list_sessions', { hostId: checkpoint.host.id })).find(item => item.id === saved.id);
        assert(same(refreshed.identity, checkpoint.session.identity), 'Attach replaced original remote session identity');
        proof.checkpoint = { ...checkpoint, counter: observation, processId: options.processId ?? null };
        passed(requireRestart ? 'Reattach original advancing process after native app restart' : 'Reattach original advancing process for boundary scenario', { previousProcessId: checkpoint.processId, processId: options.processId ?? null, remotePid: observation.pid, previousCount: checkpoint.counter.count, count: observation.count, identity: saved.identity });
        if (requireRestart) proof.limitations.push('Native PID values are launcher-supplied; parent must retain OS launch/exit observations with this proof.');
        return { tab, saved, checkpoint };
      };
      if (options.phase === 'fixture') {
        assert(fixture.aliases && fixture.config, 'Generated fixture aliases/config are required');
        const hosts = {};
        for (const [kind, alias] of Object.entries(fixture.aliases)) hosts[kind] = await enroll(kind, draft(alias));
        const a = await begin(hosts.valid); const b = await begin(hosts.valid);
        await ready(a.id); await ready(b.id); await terminalExercise(a, b);
        const agent = await begin(hosts.agentAuth); await ready(agent.id);
        await marker(agent.id, `${tag}-private-agent`); await disconnect(agent.id);
        passed("Native OpenSSH authenticates through the isolated real ssh-agent with only a public IdentityFile");
        const unknown = await begin(hosts.unknownKey); await consent(unknown.id, false);
        const again = await begin(hosts.unknownKey); await consent(again.id, true);
        await marker(again.id, `${tag}-trusted`); await disconnect(again.id);
        for (const kind of ['existingMaster']) {
          const tab = await begin(hosts[kind]); await consent(tab.id, false);
        }
        for (const [kind, stage, code, persistent] of [
          ['changedKey', 'hostKey', 'changed', false], ['authFailure', 'auth', 'authentication', false],
          ['permissiveUnknownKey', 'local', 'unavailable', false],
          ['dnsFailure', 'resolve', 'connectionFailed', false], ['tcpTimeout', 'tcp', 'timeout', false],
          ['authSessionDenied', 'session', null, false], ['missingTmux', 'session', 'missing', true]
        ]) {
          const tab = await begin(hosts[kind], persistent);
          const failed = await wait(() => {
            const state = live(tab.id).state;
            assert(state.kind !== 'awaitingConsent', `${kind} requested inappropriate consent`);
            return state.kind === 'failed' && state;
          }, `${kind} did not settle in typed failure`);
          assert(failed.error.stage === stage && (!code || failed.error.code === code), `${kind} failed at ${failed.error.stage}/${failed.error.code}, expected ${stage}/${code ?? 'failure'}`);
          passed('Typed native failure with isolated real SSH fixture', { kind, stage: failed.error.stage, code: failed.error.code });
        }
        await marker(b.id, `${tag}-after-failures`);
        await createCounter(hosts.valid);
      } else if (options.phase === 'real') {
        const realDraft = name => {
          const binding = options.realHosts?.[name] ?? {};
          return { ...draft(binding.sshTarget ?? name, binding.hostNameOverride ?? null, binding.hostKeyAliasOverride ?? null), portOverride: binding.portOverride ?? null };
        };
        const aHost = await enroll('hscale', realDraft('hscale'));
        const bHost = await enroll('m3', realDraft('m3pro'));
        const vps = await enroll('vps', realDraft('vps'));
        const a = await begin(aHost); const b = await begin(bHost);
        await ready(a.id); await ready(b.id); await terminalExercise(a, b);
        const c = await begin(vps); await ready(c.id);
        await marker(c.id, `${tag}-vps`); await disconnect(c.id);
        passed('Existing VPS alias executed actual terminal command');
        await createCounter(aHost);
      } else if (options.phase === 'restart') {
        const { tab } = await attachCheckpoint(true); await disconnect(tab.id);
      } else if (options.phase === 'dialogs') {
        assert(fixture.nativeBackup && fixture.corruptBackup && fixture.newerBackup, 'Actual restore-case paths are required');
        const { tab, checkpoint } = await attachCheckpoint(false);
        const metadata = async () => ({ hosts: await invoke('list_hosts'), sessions: projectedSessions(await invoke('list_sessions', { hostId: null })) });
        const before = await metadata();
        const nodes = [...document.querySelectorAll('.xterm')];
        const leases = tabs().map(item => ({ id: item.id, lease: item.lease }));
        notice(`Actual native Save panel: save backup to ${fixture.nativeBackup}. Fixture supervisor derives a newer-schema file from this actual export.`);
        const exported = await invoke('export_backup');
        assert(exported.kind === 'completed' && pathKey(exported.path) === pathKey(fixture.nativeBackup), 'Native backup was not saved to selected fixture case path');
        for (const [path, expected] of [[fixture.corruptBackup, 'corruptBackup'], [fixture.newerBackup, 'newerSchema']]) {
          notice(`Actual native Open panel: select ${path}. For newer.sqlite3, wait for supervisor's restore-fixtures-ready message. Refusal must preserve metadata.`);
          let refusal;
          try { await invoke('choose_restore'); } catch (error) { refusal = error; }
          assert(refusal?.code === expected, `Native picker did not reject ${expected}`);
          assert(same(await metadata(), before), 'Refused restore changed metadata');
          passed('Actual native picker refuses incompatible restore and preserves metadata', { code: expected });
        }
        notice('Press Cancel in the next actual native restore Open panel.');
        assert((await invoke('choose_restore')).kind === 'canceled', 'Restore picker was not actually canceled');
        assert(same(await metadata(), before), 'Canceled picker changed metadata');
        notice(`Select exported backup ${fixture.nativeBackup}, then Cancel in actual native replacement confirmation.`);
        const preview = await invoke('choose_restore'); assert(preview.kind === 'selected', 'Preview selection was canceled');
        assert((await invoke('commit_restore', { ticketId: preview.value.ticketId })).kind === 'canceled', 'Native replacement confirmation was not canceled');
        assert(same(await metadata(), before), 'Canceled replacement changed metadata');
        const sentinel = await enroll('restore-mutation', draft(fixture.aliases.valid));
        const labelNode = editField('host-label', `${tag}-unfinished`);
        const formNode = labelNode.closest('form');
        notice(`Clicking actual Restore UI. In native Open choose ${fixture.nativeBackup}; approve web preview and actual native Replace metadata confirmation.`);
        document.getElementById('open-maintenance').click();
        const maintenance = await wait(() => document.querySelector('#maintenance-dialog[open]'), 'Actual maintenance dialog did not open');
        const restoreButton = options.selectors?.restore ? maintenance.querySelector(options.selectors.restore) : maintenance.querySelector('#choose-restore');
        assert(restoreButton && !restoreButton.disabled, 'Actual maintenance Restore action was not found'); restoreButton.click();
        const confirmation = await wait(() => [...document.querySelectorAll('dialog.confirmation-dialog[open]')].find(item => visible(item) && item.querySelector('h2')?.textContent === 'Replace Fleet metadata?'), 'Actual restore preview did not appear', 120000);
        button(confirmation, /^Restore this backup$/).click();
        await wait(async () => same(await metadata(), before), 'Actual native UI restore did not recover exported metadata', 120000);
        await wait(() => !document.querySelector(hostSelector(sentinel.id)), 'Restored inventory still displays removed host');
        assert(labelNode.isConnected && labelNode.closest('form') === formNode && labelNode.value === `${tag}-unfinished`, 'Restore remounted or cleared unfinished form');
        assert(nodes.length === document.querySelectorAll('.xterm').length && nodes.every((node, index) => node === document.querySelectorAll('.xterm')[index]), 'Restore remounted xterm DOM');
        assert(same(leases, tabs().map(item => ({ id: item.id, lease: item.lease }))), 'Restore replaced live leases');
        const advanced = await wait(() => { const item = counter(buffer(tab.id), checkpoint.token); return item && item.pid === checkpoint.counter.pid && item.count > checkpoint.counter.count + 2 && item; }, 'Original counter stopped during restore');
        passed('Actual export, corrupt/newer/canceled refusal and UI restore preserve terminals/form', { exportedPath: exported.path, counter: advanced });
        await disconnect(tab.id);
      } else if (options.phase === 'ended') {
        const { saved, checkpoint } = await attachCheckpoint(false);
        assert(/^fleet-[a-f0-9]{32}$/.test(saved.remoteSessionName) && /^%\d+$/.test(saved.identity.originalPaneId), 'Refusing mutation outside app-owned session identity');
        const control = await begin(checkpoint.host); await ready(control.id);
        const name = shellQuote('=' + saved.remoteSessionName);
        const pane = shellQuote(saved.identity.originalPaneId);
        await send(control.id, `if [ "$(tmux display-message -p -t ${pane} '#{session_name}')" = ${shellQuote(saved.remoteSessionName)} ]; then tmux new-window -d -t ${name} -n smoke-survivor && tmux respawn-pane -k -t ${pane} && printf '%s%s\\n' 'FLEET_' 'RESPAWNED'; fi\r`);
        await wait(() => line(control.id, 'FLEET_RESPAWNED'), 'Own original pane respawn was not observed');
        const ended = await invoke('check_session', { sessionId: saved.id });
        assert(ended.status.kind === 'ended', 'Respawned pane falsely considered surviving original process');
        await send(control.id, `tmux kill-session -t ${name} && if tmux has-session -t ${name} 2>/dev/null; then printf '%s%s\\n' 'FLEET_' 'OWN_SESSION_STILL_PRESENT'; else printf '%s%s\\n' 'FLEET_' 'OWN_SESSION_REMOVED'; fi\r`);
        await wait(() => line(control.id, 'FLEET_OWN_SESSION_REMOVED'), 'Own session removal was not observed');
        const missing = await invoke('check_session', { sessionId: saved.id });
        assert(missing.status.kind === 'ended', 'Missing original session was not marked ended');
        assert((await invoke('list_sessions', { hostId: checkpoint.host.id })).filter(item => item.id === saved.id).length === 1, 'Check implicitly replaced session reference');
        await disconnect(control.id);
        passed('Respawned original process and removed own session are ended, not recreated');
      } else { throw new Error('Unknown native transport phase'); }
      const diagnostics = await invoke('diagnostics_preview', { includeHostMetadata: false });
      assert(diagnostics.hosts.length === 0 && !JSON.stringify(diagnostics).includes('FLEET_COUNT') && !JSON.stringify(diagnostics).includes('FLEET_PASTE'), 'Diagnostics include terminal content or default host metadata');
      passed('Native diagnostics omit default host metadata and terminal bytes');
      proof.status = 'passed';
      notice(`${proof.scenario} passed only recorded observations. Signatures, notarization, real OS reboot and fresh macOS account installation are not inferred.`);
    } catch (error) {
      proof.status = 'failed';
      proof.error = error instanceof Error ? error.message : `${error?.stage ?? 'native'}/${error?.code ?? 'unknown'}`;
      notice(`Native transport failed: ${proof.error}`);
    }
    proof.finishedAt = new Date().toISOString();
    try { await invoke('smoke_result', { proof }); }
    finally { running = false; start.disabled = false; }
    return proof;
  }
  window.fleetNativeSmoke = { run };
  start.addEventListener('click', () => {
    let options;
    try { options = JSON.parse(input.value); }
    catch (error) { notice(`Invalid options JSON: ${error.message}`); return; }
    void run(options);
  });
  if (window.fleetNativeSmokeOptions) void run(window.fleetNativeSmokeOptions);
})();
