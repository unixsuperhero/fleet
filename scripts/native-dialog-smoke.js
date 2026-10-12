(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const proof = { native: true, scenario: 'native-dialogs-and-staged-restore', startedAt: new Date().toISOString(), observations: [] };
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const metadata = async () => JSON.stringify({ hosts: await invoke('list_hosts'), sessions: await invoke('list_sessions', { hostId: null }) });
  const notice = text => {
    let panel = document.getElementById('native-smoke-instructions');
    if (!panel) {
      panel = document.createElement('aside');
      panel.id = 'native-smoke-instructions';
      panel.style.cssText = 'position:fixed;bottom:12px;right:12px;z-index:10000;max-width:600px;background:#202020;color:#ffd076;padding:16px;border:1px solid #ffd076;white-space:pre-wrap';
      document.body.append(panel);
    }
    panel.textContent = text;
  };
  try {
    const deadline = Date.now() + 15000;
    while (!document.querySelector('form')) {
      if (Date.now() > deadline) throw new Error('Packaged UI did not mount');
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const before = await metadata();
    for (const [command, args] of [['choose_backup_directory', {}], ['export_backup', {}], ['choose_restore', {}], ['export_diagnostics', { includeHostMetadata: false }]]) {
      notice(`Cancellation proof: press Cancel in the actual native panel for ${command}.`);
      const result = await invoke(command, args);
      assert(result.kind === 'canceled', `${command} was not canceled`);
      assert(await metadata() === before, `${command} cancellation changed metadata`);
      proof.observations.push({ behavior: `${command} actual native cancellation`, value: 'passed' });
    }
    notice('Save a real Fleet SQLite backup in the next native save panel. Remember its path.');
    const exported = await invoke('export_backup');
    assert(exported.kind === 'completed', 'Backup export was canceled');
    notice(`Select the exported backup in the native Open panel:\n${exported.path}\nThen press Cancel in the native Replace metadata confirmation.`);
    const first = await invoke('choose_restore');
    assert(first.kind === 'selected', 'Restore selection was canceled');
    const canceled = await invoke('commit_restore', { ticketId: first.value.ticketId });
    assert(canceled.kind === 'canceled', 'Restore confirmation was not canceled');
    assert(await metadata() === before, 'Canceled staged restore changed metadata');
    let rejected = false;
    try { await invoke('commit_restore', { ticketId: first.value.ticketId }); } catch (error) { rejected = error.code === 'canceled'; }
    assert(rejected, 'Consumed canceled ticket could be reused');
    proof.observations.push({ behavior: 'actual native restore consent cancellation and ticket rejection', value: 'passed' });
    notice(`Select the same exported backup again:\n${exported.path}\nThis preview will be canceled without a database write.`);
    const second = await invoke('choose_restore');
    assert(second.kind === 'selected', 'Second restore selection was canceled');
    await invoke('cancel_restore', { ticketId: second.value.ticketId });
    assert(await metadata() === before, 'Canceling a restore preview changed metadata');
    notice(`Select the same exported backup again:\n${exported.path}\nPress Replace metadata in the native confirmation. This isolated smoke profile will restore its own snapshot.`);
    const third = await invoke('choose_restore');
    assert(third.kind === 'selected', 'Third restore selection was canceled');
    const restored = await invoke('commit_restore', { ticketId: third.value.ticketId });
    assert(restored.kind === 'completed', 'Approved restore did not complete');
    assert(await metadata() === before, 'The exported metadata did not round-trip through native restore');
    proof.observations.push({ behavior: 'native export, immutable preview, explicit committed restore', value: 'passed', preview: third.value, exportedPath: exported.path });
    proof.status = 'passed';
    notice('Native dialog scenario passed. The runner will capture a real macOS screenshot. Corrupt/newer-schema files and terminal/restart scenarios remain separate checks.');
  } catch (error) {
    proof.status = 'failed';
    proof.error = error instanceof Error ? error.message : String(error && error.code || 'native operation failed');
    notice(`Native dialog scenario failed: ${proof.error}`);
  }
  proof.finishedAt = new Date().toISOString();
  await invoke('smoke_result', { proof });
})();
