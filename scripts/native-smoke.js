(async () => {
  const waitFor = async (predicate, message, timeout = 15000) => {
    const deadline = Date.now() + timeout;
    while (!predicate()) {
      if (Date.now() >= deadline) throw new Error(message);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  };
  const invoke = window.__TAURI__.core.invoke;
  const proof = { native: true, scenario: 'packaged-ui-api', startedAt: new Date().toISOString(), observations: [] };
  try {
    await waitFor(() => document.querySelector('form') && document.querySelector('button'), 'Fleet enrollment UI did not mount');
    const info = await invoke('app_info');
    const before = await invoke('list_hosts');
    const form = document.querySelector('form');
    const editable = Array.from(form.querySelectorAll('input')).find(input => !input.disabled && !['hidden', 'checkbox', 'password', 'number'].includes(input.type));
    if (!editable) throw new Error('Enrollment form has no editable text input');
    const previous = editable.value;
    const marker = `native-form-${Date.now()}`;
    editable.value = marker;
    editable.dispatchEvent(new Event('input', { bubbles: true }));
    editable.focus();
    const discoveryButton = Array.from(document.querySelectorAll('button')).find(button => /discover|refresh/i.test(button.textContent || '') && !button.disabled);
    if (!discoveryButton) throw new Error('Discovery UI action was not found');
    discoveryButton.click();
    await waitFor(() => !discoveryButton.disabled, 'Discovery UI did not complete', 30000);
    await new Promise(resolve => setTimeout(resolve, 100));
    if (!editable.isConnected || editable.value !== marker) throw new Error('Discovery remounted or cleared the enrollment form');
    if (document.activeElement !== editable && document.activeElement !== discoveryButton) throw new Error('Discovery unexpectedly moved focus');
    proof.observations.push({ behavior: 'discovery UI preserves typed enrollment', value: 'passed' });
    editable.value = previous;
    editable.dispatchEvent(new Event('input', { bubbles: true }));
    const after = await invoke('list_hosts');
    if (JSON.stringify(before) !== JSON.stringify(after)) throw new Error('Discovery enrolled or changed hosts without selection');
    let invalid = null;
    try { await invoke('open_link', { url: 'file:///etc/passwd' }); } catch (error) { invalid = error; }
    if (!invalid || invalid.code !== 'invalidInput') throw new Error('Unsafe link was not blocked');
    const diagnostics = await invoke('diagnostics_preview', { includeHostMetadata: false });
    if (diagnostics.hosts.length !== 0) throw new Error('Default diagnostic preview contains host metadata');
    proof.observations.push({ behavior: 'unsafe URL rejected', value: 'passed' }, { behavior: 'default diagnostic host exclusion', value: 'passed' });
    proof.appInfo = info;
    proof.status = 'passed';
  } catch (error) {
    proof.status = 'failed';
    proof.error = error instanceof Error ? error.message : String(error && error.code || 'native operation failed');
  }
  proof.finishedAt = new Date().toISOString();
  await invoke('smoke_result', { proof });
})();
