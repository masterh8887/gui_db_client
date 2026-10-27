// CI smoke test (spec 11.4): no window, no external DB. Builds a deterministic 10,000-row SQLite
// database with node:sqlite, exports it with bulk-cli to CSV and Parquet, and checks that the
// CSV bytes (line endings normalised) hash to the committed reference, so Windows and macOS
// produce identical files.
//
//   node scripts/smoke.mjs --cli target/release/bulk-cli[.exe] [--update]
import { DatabaseSync } from 'node:sqlite';
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const cli = path.resolve(args[args.indexOf('--cli') + 1] || 'target/release/bulk-cli' + (process.platform === 'win32' ? '.exe' : ''));
const update = args.includes('--update');
const refPath = path.join(root, 'scripts', 'smoke-reference.json');

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'bulkdb-smoke-'));
const dbPath = path.join(work, 'smoke.db');
const config = path.join(work, 'config');
fs.mkdirSync(config);

// Deterministic data: LCG, same shape as crates/core/src/testdata.rs `wide` (9 columns).
let seed = 42n;
const lcg = () => {
  seed = (seed * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
  return Number(seed >> 11n);
};
const words = ['lorem', 'ipsum', 'dolor', 'sit', 'amet', 'consectetur', 'adipiscing', 'elit', 'sed', 'do', 'eiusmod', 'tempor', 'incididunt', 'ut', 'labore', 'magna'];
const w = (n) => Array.from({ length: n }, () => words[lcg() % 16]).join(' ');

const db = new DatabaseSync(dbPath);
db.exec('CREATE TABLE wide (id INTEGER PRIMARY KEY, title TEXT, body TEXT, author TEXT, category TEXT, tags TEXT, created_at TEXT, updated_at TEXT, status TEXT)');
db.exec('CREATE TABLE nasty (id INTEGER PRIMARY KEY, text TEXT, note TEXT, unicode TEXT)');
const ins = db.prepare('INSERT INTO wide VALUES (?,?,?,?,?,?,?,?,?)');
const insN = db.prepare('INSERT INTO nasty VALUES (?,?,?,?)');
db.exec('BEGIN');
for (let i = 0; i < 10_000; i++) {
  ins.run(i, w(4), w(18), `user${lcg() % 5000}`, ['news', 'blog', 'music', 'video'][lcg() % 4], `t${lcg() % 100},t${lcg() % 100}`,
    `2026-${String(1 + (lcg() % 12)).padStart(2, '0')}-${String(1 + (lcg() % 28)).padStart(2, '0')} 10:00:00`, '2026-10-23 12:00:00', ['active', 'deleted', 'draft'][lcg() % 3]);
}
const nastyTexts = ['plain', 'has,comma', 'has "quote" inside', 'line1\nline2', 'crlf\r\nline', '', null, ' leading and trailing '];
const unis = ['한글 텍스트', 'emoji 🚀🎉', '日本語, 中文', null, 'ünïcödé'];
for (let i = 0; i < 1_000; i++) insN.run(i, nastyTexts[lcg() % 8], lcg() % 4 === 0 ? null : `note ${lcg() % 1000}`, unis[lcg() % 5]);
db.exec('COMMIT');
db.close();

const env = { ...process.env, BULKDB_CONFIG_DIR: config, BULKDB_CREDENTIALS: 'memory' };
function run(extra) {
  const r = spawnSync(cli, ['--json', 'export', '--driver', 'sqlite', '--path', dbPath, '--yes', '--overwrite', ...extra], { env, encoding: 'utf8' });
  if (r.status !== 0) {
    console.error(r.stdout, r.stderr);
    throw new Error(`bulk-cli exited with ${r.status}`);
  }
  return r.stdout;
}

const csvOut = path.join(work, 'wide.csv');
run(['--query', 'select * from wide order by id', '--out', csvOut, '--lf']);
const nastyOut = path.join(work, 'nasty.csv');
run(['--query', 'select * from nasty order by id', '--out', nastyOut, '--lf']);
const pqOut = path.join(work, 'wide.parquet');
run(['--query', 'select * from wide order by id', '--out', pqOut]);

const norm = (p) => fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n');
const csv = norm(csvOut);
const lines = csv.split('\n').filter((l) => l.length);
if (lines.length !== 10_001) throw new Error(`expected 10,001 CSV lines, got ${lines.length}`);
const hashes = { wide: crypto.createHash('sha256').update(csv).digest('hex'), nasty: crypto.createHash('sha256').update(norm(nastyOut)).digest('hex') };
const pqSize = fs.statSync(pqOut).size;
if (pqSize < 1024 || fs.readFileSync(pqOut).subarray(0, 4).toString() !== 'PAR1') throw new Error('parquet output is not a parquet file');

if (update || !fs.existsSync(refPath)) {
  fs.writeFileSync(refPath, JSON.stringify(hashes, null, 2) + '\n');
  console.log('reference written', hashes);
} else {
  const ref = JSON.parse(fs.readFileSync(refPath, 'utf8'));
  for (const k of Object.keys(hashes)) {
    if (ref[k] !== hashes[k]) throw new Error(`${k}.csv differs from the reference (${hashes[k]} != ${ref[k]})`);
  }
  console.log('smoke ok: 10,000 wide rows + 1,000 nasty rows match the reference; parquet', pqSize, 'bytes');
}
fs.rmSync(work, { recursive: true, force: true });
