// Scenario source for the packaged Fleet app. native-setup-dialogs.sh prepends
// `window.fleetSetupFixture = {...};` (manifest + checkout + uid) and runs it
// through native-smoke.sh. Every dialog is answered by the external macOS GUI
// driver; this script only clicks real UI and asserts real console output.
(() => {
  if (window.fleetSetupSmoke) return;
  const invoke = (command, args = {}) => window.__TAURI__.core.invoke(command, args);
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const assert = (value, message) => { if (!value) throw new Error(message); };
  const wait = async (read, message, timeout = 60000) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) { const value = await read(); if (value) return value; await sleep(50); }
    throw new Error(message);
  };
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const pathKey = value => value.replace(/^\/private\/tmp\//, '/tmp/').replace(/\/+$/, '');
  const visible = element => element.getClientRects().length > 0;
  const aside = document.createElement('aside');
  aside.id = 'native-setup-phase';
  aside.style.cssText = 'position:fixed;bottom:8px;right:8px;z-index:10000;max-width:520px;background:#171a1c;color:#e6c07b;border:1px solid #bfa36c;padding:10px;font:12px monospace;white-space:pre-wrap';
  document.body.append(aside);
  const phase = (id, text) => { document.title = `Fleet setup smoke ${id}`; aside.textContent = `${id}: ${text}\nThe external driver answers native dialogs; do not click them.`; };
  const lines = text => text.split(/\r?\n/).map(item => item.trimEnd());

  async function run() {
    const proof = { native: true, scenario: 'native-host-setup', startedAt: new Date().toISOString(), observations: [], limitations: [] };
    const passed = (behavior, evidence = {}) => proof.observations.push({ behavior, value: 'passed', ...evidence });
    try {
      const fixture = window.fleetSetupFixture;
      assert(fixture?.setup && fixture.checkout && Number.isInteger(fixture.uid), 'Run this through native-setup-dialogs.sh with FLEET_SMOKE_SETUP=1');
      const api = await wait(() => window.fleetSmoke, 'Debug-only actual xterm smoke access did not mount', 15000);
      await wait(() => document.getElementById('host-label') && document.getElementById('ssh-target'), 'Host form did not mount', 15000);
      proof.appInfo = await invoke('app_info');
      assert(pathKey(proof.appInfo.databasePath).startsWith(pathKey(fixture.profile) + '/'), 'The app is not using the fixture profile');
      const tag = `setup-${Date.now().toString(36)}`;
      const tabs = () => api.snapshots();
      const live = id => { const tab = tabs().find(item => item.id === id); assert(tab, `Terminal ${id} disappeared`); return tab; };
      const send = (id, text) => invoke('terminal_input', { lease: live(id).lease, bytes: Array.from(new TextEncoder().encode(text)) });
      const hasLine = (id, text) => lines(live(id).buffer).includes(text);
      const metadata = async () => JSON.stringify({ hosts: await invoke('list_hosts'), sessions: await invoke('list_sessions', { hostId: null }) });
      const editField = (id, value) => { const field = document.getElementById(id); assert(field, `Input #${id} is required`); field.value = value ?? ''; field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new Event('change', { bubbles: true })); };
      const rowOf = host => { const row = document.querySelector(`.host-row[data-host-id="${host.id}"]`); assert(row, `Host row missing for ${host.input.label}`); return row; };
      const control = (row, action) => { const item = row.querySelector(`[data-action="${action}"]`); assert(item, `Missing UI control ${action}`); return item; };
      const enroll = async (suffix, draft) => {
        const label = `${tag}-${suffix}`;
        document.getElementById('new-host')?.click();
        editField('host-label', label); editField('ssh-target', draft.sshTarget);
        editField('hostname-override', draft.hostNameOverride); editField('hostkey-alias', draft.hostKeyAliasOverride); editField('host-port', draft.portOverride === null ? '' : String(draft.portOverride));
        document.getElementById('host-label').closest('form').requestSubmit();
        const host = await wait(async () => (await invoke('list_hosts')).find(item => item.input.label === label), `Native host save did not complete for ${label}`);
        assert(same(host.input, { ...draft, label }), `Form did not save exact SSH settings for ${label}`);
        await wait(() => document.querySelector(`.host-row[data-host-id="${host.id}"]`) && !document.getElementById('host-label').closest('fieldset').disabled, 'Inventory did not finish saving');
        return host;
      };
      const plain = async host => {
        const old = new Set(tabs().map(item => item.id));
        control(rowOf(host), 'plain-terminal').click();
        const dialog = await wait(() => [...document.querySelectorAll('dialog.confirmation-dialog[open]')].find(item => visible(item) && item.querySelector('h2')?.textContent === 'Open a plain SSH terminal?'), 'Plain SSH warning did not appear');
        [...dialog.querySelectorAll('button')].find(item => item.textContent.trim() === 'Open plain SSH').click();
        const tab = await wait(() => tabs().find(item => item.hostId === host.id && !old.has(item.id)), 'Plain terminal did not open');
        await wait(() => { const item = live(tab.id); if (item.state.kind === 'failed' || item.state.kind === 'awaitingConsent') throw new Error(`Plain terminal ${item.state.kind}`); return item.state.kind === 'connected'; }, 'Plain terminal did not connect');
        return tab;
      };
      const probe = async (id, token) => {
        const dirs = '"$HOME"/.claude/* "$HOME"/.claude/.[!.]* "$HOME"/.codex/* "$HOME"/Library/LaunchAgents/* "$HOME"/.config/systemd/user/*';
        await send(id, `tuios ls --json >"$HOME/.probe.json" 2>/dev/null; rc=$?; sid=$(bun -e 'try{for(const s of JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")))console.log(s.id)}catch{}' "$HOME/.probe.json"); n=$(printf '%s\\n' "$sid" | grep -c .); h=0; for f in ${dirs}; do [ -e "$f" ] && h=$((h+1)); done; c=0; for f in "$XDG_CONFIG_HOME"/* "$XDG_CONFIG_HOME"/*/*; do [ -e "$f" ] && c=$((c+1)); done; printf '%s%s|%s|%s|%s|%s|%s|%s\\n' 'FLEET_' 'PROBE' '${token}' "$rc" "$n" "$(printf '%s' "$sid" | head -n 1)" "$h" "$c"\r`);
        const found = await wait(() => lines(live(id).buffer).find(item => item.startsWith(`FLEET_PROBE|${token}|`)), `Probe ${token} produced no output`, 20000);
        const [, , rc, count, sid, hooks, config] = found.split('|');
        return { rc: Number(rc), sessions: Number(count), sid, hooks: Number(hooks), config: Number(config) };
      };
      const setupOf = row => { const node = row.querySelector('details.host-setup'); assert(node, 'Setup panel missing from host row'); node.open = true; return node; };
      const outputText = node => node.querySelector('.operation-status')?.textContent ?? '';
      const retryHidden = id => { const pane = document.getElementById(`panel-${id}`); assert(pane, 'Setup terminal pane missing'); return [...pane.querySelectorAll('[data-action="retry-terminal"]')].every(item => !visible(item)); };
      // Clicks the real setup control and waits for the actual console to end.
      const runSetup = async (host, action) => {
        const old = new Set(tabs().map(item => item.id));
        control(setupOf(rowOf(host)), action).click();
        const tab = await wait(() => tabs().find(item => item.hostId === host.id && !old.has(item.id)), `No setup terminal opened for ${action}`, 120000);
        const done = await wait(() => {
          const item = live(tab.id);
          if (item.state.kind === 'failed') throw new Error(`Setup terminal failed: ${item.state.error.code}`);
          if (item.state.kind === 'awaitingConsent') throw new Error('Setup unexpectedly needs host-key or credential consent');
          return item.state.kind === 'disconnected' && item;
        }, `Setup ${action} did not finish`, 180000);
        assert(/Setup process ended with status 0\./.test(done.state.reason), `Setup ${action} ended abnormally: ${done.state.reason}`);
        assert(retryHidden(tab.id), 'Spent setup ticket offers Retry');
        // The epilogue is written into xterm after the state flips; read the buffer once it stops changing.
        let text = live(tab.id).buffer, stable = 0;
        while (stable < 8) { await sleep(100); const now = live(tab.id).buffer; if (now === text) stable += 1; else { text = now; stable = 0; } }
        return { tab, text, reason: done.state.reason };
      };
      const expect = (text, pattern) => assert(pattern.test(text), `Console lacks ${pattern}`);

      phase('S1', 'Enroll the valid host, a wrong-config host with SSH overrides, and open the plain counter terminal.');
      const draft = (sshTarget, hostName = null, keyAlias = null, port = null) => ({ label: '', sshTarget, portOverride: port, hostNameOverride: hostName, hostKeyAliasOverride: keyAlias, tailnetNodeId: null });
      const hostA = await enroll('valid', draft(fixture.aliases.valid));
      const hostB = await enroll('override', draft(fixture.setup.route, '127.0.0.1', fixture.setup.hostKeyAlias, fixture.ports.normal));
      const plainTab = await plain(hostA);
      const token = `${tag}-counter`;
      await send(plainTab.id, `n=0; while :; do n=$((n+1)); printf '%s%s|%s|%s|%s\\n' 'FLEET_' 'COUNT' '${token}' $$ $n; sleep 1; done\r`);
      const counter = id => { let best = null; for (const item of lines(live(id).buffer)) { const f = item.split('|'); if (f.length === 4 && f[0] === 'FLEET_COUNT' && f[1] === token) best = { pid: Number(f[2]), count: Number(f[3]) }; } return best; };
      const first = await wait(() => counter(plainTab.id), 'Counter did not start', 20000);
      const rowA = rowOf(hostA), tabNode = document.getElementById(`tab-${plainTab.id}`), paneNode = document.getElementById(`panel-${plainTab.id}`);
      const leaseBefore = live(plainTab.id).lease;
      assert(tabNode && paneNode, 'Plain terminal tab DOM is missing');
      const panelA = setupOf(rowA);
      const aliasField = control(panelA, 'setup-alias');
      aliasField.value = `${tag}-alias`; aliasField.dispatchEvent(new Event('input', { bubbles: true }));
      const boxes = ['claude', 'omp'].map(name => panelA.querySelector(`input[data-harness="${name}"]`));
      assert(boxes.every(Boolean), 'Harness checkboxes missing');
      for (const box of boxes) { box.checked = true; box.dispatchEvent(new Event('change', { bubbles: true })); }
      passed('Enrolled valid and override hosts through the real form; saved SSH overrides exact', { override: hostB.input });

      phase('S2', 'Native folder picker: the driver cancels, then chooses the real checkout.');
      const beforePicker = await metadata(); const tabsBefore = tabs().length;
      control(panelA, 'choose-setup-checkout').click();
      await wait(() => outputText(panelA) === 'Folder selection canceled.', 'Native folder cancellation was not reported', 90000);
      assert(panelA.querySelector('.setup-path').textContent === 'No folder selected', 'Cancel selected a folder');
      assert(await metadata() === beforePicker && tabs().length === tabsBefore, 'Folder cancel changed metadata or opened a console');
      passed('Actual native folder picker cancellation changes nothing');
      control(panelA, 'choose-setup-checkout').click();
      await wait(() => panelA.querySelector('.setup-path').textContent !== 'No folder selected', 'Native folder was not selected', 120000);
      assert(pathKey(panelA.querySelector('.setup-path').textContent) === pathKey(fixture.checkout), 'The folder shown is not the one the driver chose');
      assert(pathKey(rowOf(hostB).querySelector('.setup-path').textContent) === pathKey(fixture.checkout), 'Checkout is not shared with other hosts');
      passed('Actual native folder picker selected the real checkout', { path: fixture.checkout });

      phase('S3', 'Check: native confirmation canceled, then approved.');
      const beforeCancel = await metadata();
      control(panelA, 'setup-check').click();
      await wait(() => /^Canceled in the native confirmation/.test(outputText(panelA)), 'Native Check cancellation was not reported', 90000);
      assert(await metadata() === beforeCancel && tabs().length === tabsBefore, 'Canceled Check changed metadata or opened a console');
      assert(!control(panelA, 'setup-check').disabled, 'Check stayed disabled after cancel');
      passed('Actual native confirmation cancel opens no console and changes no metadata');
      const check = await runSetup(hostA, 'setup-check');
      expect(check.text, /^== Fleet setup: inspect \(read-only\) ==$/m);
      const account = check.text.match(/^account: \S+ \(uid (\d+)\) on Darwin/m);
      assert(account && Number(account[1]) === fixture.uid, 'Inspection did not report the fixture account');
      for (const tool of ['bun', 'tuios']) {
        const found = check.text.match(new RegExp(`^${tool}: (\\S+)`, 'm'));
        assert(found && pathKey(found[1]).startsWith(pathKey(fixture.setup.home) + '/bin/'), `${tool} was not found under the private HOME`);
      }
      expect(check.text, /^tuios daemon: not running \(tuios ls exited 3\)$/m);
      assert(await metadata() === beforeCancel, 'Inspection changed saved metadata');
      passed('Approved Check inspected the private SSH HOME with actual output', { reason: check.reason });

      phase('S4', 'Reload and re-save metadata; the old plain terminal and setup draft must survive.');
      document.getElementById('reload-inventory').click();
      await sleep(500);
      control(rowOf(hostA), 'edit-host').click();
      await wait(() => document.getElementById('host-label').value === hostA.input.label, 'Edit did not load the host');
      document.getElementById('host-label').closest('form').requestSubmit();
      await wait(() => !document.getElementById('host-label').closest('fieldset').disabled, 'Host save did not finish');
      await sleep(500);
      const advanced = await wait(() => { const item = counter(plainTab.id); return item && item.pid === first.pid && item.count > first.count + 2 && item; }, 'Old plain counter stopped after metadata reload', 20000);
      assert(rowOf(hostA) === rowA && setupOf(rowA) === panelA && document.getElementById(`tab-${plainTab.id}`) === tabNode && document.getElementById(`panel-${plainTab.id}`) === paneNode, 'Reload remounted the host row, setup panel or terminal DOM');
      assert(same(live(plainTab.id).lease, leaseBefore) && live(plainTab.id).state.kind === 'connected', 'The old plain lease changed');
      assert(aliasField.value === `${tag}-alias` && boxes.every(box => box.checked) && pathKey(panelA.querySelector('.setup-path').textContent) === pathKey(fixture.checkout), 'Setup draft was reset');
      passed('Metadata reload/save preserved the old terminal, counter, lease and setup draft', { counter: advanced });

      phase('S5', 'Start daemon: isolated daemon created once, second run reuses it.');
      const control1 = await plain(hostA);
      const none = await probe(control1.id, `${tag}-p0`);
      assert(none.rc === 3 && none.sessions === 0, `Isolation violated: a TUIOS daemon already answers (exit ${none.rc}); not approving a start`);
      const started = await runSetup(hostA, 'setup-start-daemon-if-absent');
      expect(started.text, /^No daemon is running\. Starting one with a detached, automatically named session\.$/m);
      expect(started.text, /^tuios daemon: running$/m);
      const one = await probe(control1.id, `${tag}-p1`);
      assert(one.rc === 0 && one.sessions === 1 && one.sid.length > 0, 'No isolated session exists after Start');
      const again = await runSetup(hostA, 'setup-start-daemon-if-absent');
      expect(again.text, /^A TUIOS daemon is already running for this account\. It was left untouched\.$/m);
      assert(!/Starting one/.test(again.text), 'Second Start attempted to start another daemon');
      const two = await probe(control1.id, `${tag}-p2`);
      assert(two.sid === one.sid && two.sessions === 1, 'Second Start changed the original session');
      passed('Start created an isolated daemon once and preserved its session id on the second run', { sessionId: one.sid, firstExit: started.reason });

      phase('S6', 'Preview capture through the override host: the private config must route to the normal sshd.');
      const preview = await runSetup(hostB, 'setup-preview-capture');
      const would = lines(preview.text).filter(item => item.startsWith('would:'));
      assert(would.length > 0, 'Dry run printed no would: lines');
      expect(preview.text, /^dry run: no service, config or file was changed$/m);
      assert(!/fleet-smoke-does-not-exist|Connection refused|Could not resolve/.test(preview.text), 'Preview did not use the override route');
      const after = await probe(control1.id, `${tag}-p3`);
      assert(after.sid === one.sid && after.rc === 0 && after.hooks === two.hooks && after.config === two.config && after.hooks === 0, 'Preview changed the daemon, hooks or config');
      passed('Preview ran the real installer dry run; no daemon restart, hooks or config', { would: would.slice(0, 20), hooks: after.hooks, config: after.config });

      phase('S7', 'Ticket rejection through the real connect command.');
      const uuid = () => crypto.randomUUID();
      const lease = await invoke('allocate_connection');
      assert(lease?.connectionId && lease?.attemptId, 'allocate_connection returned no lease');
      let rejected = null;
      try {
        const Channel = window.__TAURI__.core.Channel;
        await invoke('connect', { lease, hostId: hostA.id, intent: { kind: 'hostSetup', ticketId: uuid() }, cols: 80, rows: 24, output: new Channel(() => {}), events: new Channel(() => {}) });
      } catch (error) { rejected = error; }
      assert(rejected?.code && /already used, canceled or expired/.test(rejected.message ?? ''), `Unknown ticket was not rejected as an unknown ticket: ${JSON.stringify(rejected)}`);
      passed('Unknown setup ticket rejected by the real connect command', { code: rejected.code });
      proof.limitations.push('Spent-ticket reuse is not driven: the ticket id is private to the UI, and no value is fabricated.',
        'No provider login, Install capture, remote/local tool install or Status ran (production label/UID collisions). Install local tools stays unapproved.',
        'The isolated TUIOS daemon started by this run is not stopped by the scenario; the fixture owner must clean its private XDG runtime.',
        'Native dialog text is read by the external driver through macOS Accessibility, not by this page.');
      for (const id of [control1.id, plainTab.id]) await invoke('terminal_disconnect', { lease: live(id).lease });
      const diagnostics = JSON.stringify(await invoke('diagnostics_preview', { includeHostMetadata: false }));
      assert(!diagnostics.includes('FLEET_COUNT') && !diagnostics.includes('FLEET_PROBE'), 'Diagnostics include terminal content');
      passed('Diagnostics omit terminal output');
      proof.status = 'passed';
      phase('DONE', 'Scenario passed. Recorded observations only.');
    } catch (error) {
      proof.status = 'failed';
      proof.error = error instanceof Error ? error.message : `${error?.stage ?? 'native'}/${error?.code ?? 'unknown'}`;
      phase('FAILED', proof.error);
    }
    proof.finishedAt = new Date().toISOString();
    await invoke('smoke_result', { proof });
  }
  window.fleetSetupSmoke = { run };
  void run();
})();
