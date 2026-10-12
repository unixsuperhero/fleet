// Real SSH proof: Fleet's generated private SSH config carries the canonical collector export/ack path to a live fixture.
// Usage: bun scripts/native-setup-transfer.mjs FIXTURE_MANIFEST INBOX_CHECKOUT   (run only while the native-setup fixture is alive)
import { Database } from 'bun:sqlite';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import assert from 'node:assert/strict';

assert.ok(process.argv[3], 'Pass the selected Inbox checkout as the second argument');
const INBOX = join(process.argv[3], 'scripts');
const { openCollector } = await import(`${INBOX}/collector.mjs`);
const { createHistory } = await import(`${INBOX}/history.mjs`);
const { loadSources, createPuller, buildCommand } = await import(`${INBOX}/pull-collectors.mjs`);

const manifestPath = process.argv[2];
assert.ok(manifestPath, 'usage: bun fleet-setup-transfer.mjs FIXTURE_MANIFEST.json');
const fixture = JSON.parse(readFileSync(manifestPath, 'utf8'));
const { root, profile, config, setup } = fixture;
for (const [k, v] of Object.entries({ root, profile, config, 'setup.route': setup?.route, 'setup.hostKeyAlias': setup?.hostKeyAlias })) assert.equal(typeof v, 'string', `manifest ${k}`);

const sha = data => createHash('sha256').update(data).digest('hex');
const run = async argv => {
  const p = Bun.spawn(argv, { stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' });
  const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
  return { out, err, code };
};
const sshG = async cfg => Object.fromEntries((await run(['/usr/bin/ssh', '-F', cfg, '-G', '--', setup.route])).out.split('\n').map(l => [l.split(' ')[0], l.slice(l.indexOf(' ') + 1)]));

// The actual Fleet-generated config: the one file in profile/setup-ssh whose resolved route carries the fixture's host key alias.
const setupSsh = join(profile, 'setup-ssh');
const candidates = statSync(setupSsh).isDirectory() ? readdirSync(setupSsh).map(f => join(setupSsh, f)).filter(f => statSync(f).isFile()) : [setupSsh];
const matching = [];
for (const f of candidates) if ((await sshG(f)).hostkeyalias === setup.hostKeyAlias) matching.push(f);
assert.equal(matching.length, 1, `exactly one Fleet config must match ${setup.route} (found ${matching.length} of ${candidates.length})`);
const fleetCfg = matching[0];
assert.equal(statSync(fleetCfg).mode & 0o777, 0o600, 'Fleet config is private');

const queueDir = join(root, 'test-collector');
const central = mkdtempSync(join(tmpdir(), 'fleet-transfer-'));
const bun = join(root, 'bin/bun');
assert.ok(existsSync(bun), `fixture bun missing at ${bun}`);
const opened = [];
let db, history;
try {
  // Seed a private synthetic Claude transcript through the real scan reader into the fixture's source queue.
  const claudeRoot = join(central, 'claude');
  mkdirSync(join(claudeRoot, 'proj'), { recursive: true });
  const response = 'x'.repeat(50000) + ' FLEETTRANSFERMARKER 🌍é';
  const entry = (type, content, n) => JSON.stringify({ type, uuid: crypto.randomUUID(), sessionId: 'fleet-transfer', timestamp: `2026-01-01T00:00:0${n}Z`, message: { role: type, content } }) + '\n';
  writeFileSync(join(claudeRoot, 'proj', 'fleet-transfer.jsonl'), entry('user', 'synthetic fleet transfer prompt', 0) + entry('assistant', [{ type: 'text', text: response }], 1));
  const open = roots => { const c = openCollector({ dataDir: queueDir, host: 'fleet-transfer', tuiosBin: '/nonexistent/tuios', roots }); opened.push(c); return c; };
  const seeded = open([{ harness: 'claude-code', root: claudeRoot, patterns: ['*/*.jsonl'] }]);
  await seeded.scan();
  assert.equal(seeded.exportBatch(100).records.length, 2, 'two records queued');
  seeded.close();

  // Private central history on the minimal base schema from history.test.mjs.
  db = new Database(join(central, 'history.db'));
  db.exec(`CREATE TABLE tasks (id TEXT PRIMARY KEY, title TEXT NOT NULL);
CREATE TABLE agents (id TEXT PRIMARY KEY, session TEXT NOT NULL, name TEXT NOT NULL, harness TEXT NOT NULL, kind TEXT NOT NULL, task_id TEXT, state TEXT NOT NULL, seen TEXT NOT NULL, host TEXT NOT NULL DEFAULT '');
CREATE TABLE threads (id TEXT PRIMARY KEY, task_id TEXT, pane_id TEXT, subject TEXT NOT NULL, kind TEXT NOT NULL, created TEXT NOT NULL, updated TEXT NOT NULL);
CREATE TABLE messages (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, role TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL, meta TEXT NOT NULL DEFAULT '{}', created TEXT NOT NULL);
CREATE TABLE turns (id TEXT PRIMARY KEY, session TEXT NOT NULL, pane_id TEXT NOT NULL, pane_name TEXT NOT NULL, harness TEXT NOT NULL, prompt TEXT NOT NULL, response TEXT NOT NULL, source TEXT NOT NULL, state TEXT NOT NULL, unread INTEGER NOT NULL DEFAULT 0, started TEXT NOT NULL, finished TEXT, task_id TEXT, archived INTEGER NOT NULL DEFAULT 0);`);
  history = createHistory(db);
  const rows = () => db.query('SELECT rowid, id, record_id, host FROM history_records ORDER BY rowid').all();

  // The registration exactly as the installer would write it; loadSources must carry ssh_config through unchanged.
  const registered = { host: 'fleet-transfer', ssh: setup.route, ssh_config: fleetCfg, bun, collector_path: `${INBOX}/collector.mjs`, data_dir: queueDir };
  writeFileSync(join(central, 'collectors.json'), JSON.stringify([registered]), { mode: 0o600 });
  const sources = await loadSources(central);
  assert.deepEqual(sources, [registered], 'loadSources preserves ssh_config and every field');

  // Base fixture config has no Fleet overrides, so the same target must actually fail in real OpenSSH.
  const base = await run(buildCommand({ ...registered, ssh_config: config }, ['status']));
  assert.equal(base.code, 255, `base config must fail to connect (exit ${base.code}: ${base.err.trim().slice(0, 200)})`);
  assert.equal(base.out, '');

  const puller = createPuller({ history, sources });
  const first = (await puller.pull())[0];
  assert.deepEqual(first, { host: 'fleet-transfer', ok: true, imported: 2, duplicates: 0, batches: 2, error: '' });
  const afterFirst = rows();
  assert.equal(afterFirst.length, 2);
  assert.ok(afterFirst.every(r => r.host === 'fleet-transfer'));

  const hit = history.search({ q: 'FLEETTRANSFERMARKER', host: 'fleet-transfer' });
  assert.equal(hit.total, 1);
  const full = history.get(hit.records[0].id);
  assert.equal(full.kind, 'response');
  assert.equal(full.body, response, 'full response body is byte-exact');

  const drained = open([]);
  assert.equal(drained.exportBatch(100).records.length, 0, 'collector queue empty after ack');
  drained.close();

  const second = (await puller.pull())[0];
  assert.deepEqual(second, { host: 'fleet-transfer', ok: true, imported: 0, duplicates: 0, batches: 1, error: '' });
  assert.deepEqual(rows(), afterFirst, 'row identities stable, no duplicate history');

  const proof = {
    ok: true, manifest: manifestPath, route: setup.route, hostKeyAlias: setup.hostKeyAlias,
    sourceConfig: { name: fleetCfg.split('/').pop(), sha256: sha(readFileSync(fleetCfg)) },
    baseConfig: { exit: base.code, failed: true },
    bodySha256: sha(full.body), bodyChars: full.body.length, records: afterFirst.map(r => r.record_id),
    firstPull: first, secondPull: second, rowCount: afterFirst.length, queueRecordsAfterAck: 0,
  };
  writeFileSync(join(profile, 'transfer-proof.json'), JSON.stringify(proof, null, 2) + '\n', { mode: 0o600 });
  console.log(JSON.stringify(proof, null, 2));
} finally {
  for (const c of opened) c.close();
  db?.close();
  rmSync(central, { recursive: true, force: true });
}
