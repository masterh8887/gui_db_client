// Generates apps/desktop/src-tauri/icons/source.png (1024x1024) without any dependency.
// Design: rounded deep-blue tile, three "row" bars fading out, and a down arrow into a file line.
// Then run: pnpm --filter desktop exec tauri icon src-tauri/icons/source.png -o src-tauri/icons
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const out = path.join(root, 'apps', 'desktop', 'src-tauri', 'icons', 'source.png');
const S = 1024;
const px = Buffer.alloc(S * S * 4);

function put(x, y, r, g, b, a = 255) {
  if (x < 0 || y < 0 || x >= S || y >= S) return;
  const i = (y * S + x) * 4;
  const da = px[i + 3] / 255;
  const sa = a / 255;
  const oa = sa + da * (1 - sa);
  if (oa === 0) return;
  px[i] = Math.round((r * sa + px[i] * da * (1 - sa)) / oa);
  px[i + 1] = Math.round((g * sa + px[i + 1] * da * (1 - sa)) / oa);
  px[i + 2] = Math.round((b * sa + px[i + 2] * da * (1 - sa)) / oa);
  px[i + 3] = Math.round(oa * 255);
}

function roundedRect(x0, y0, x1, y1, rad, color) {
  for (let y = y0; y < y1; y++) {
    for (let x = x0; x < x1; x++) {
      const cx = Math.max(x0 + rad, Math.min(x, x1 - 1 - rad));
      const cy = Math.max(y0 + rad, Math.min(y, y1 - 1 - rad));
      const d = Math.hypot(x - cx, y - cy);
      if (d <= rad) {
        const aa = Math.max(0, Math.min(1, rad - d + 0.5));
        put(x, y, color[0], color[1], color[2], Math.round(255 * aa * (color[3] ?? 1)));
      }
    }
  }
}

// background tile with a vertical gradient
for (let y = 0; y < S; y++) {
  const t = y / S;
  const r = Math.round(23 + (37 - 23) * t), g = Math.round(64 + (99 - 64) * t), b = Math.round(150 + (235 - 150) * t);
  for (let x = 0; x < S; x++) {
    const cx = Math.max(180, Math.min(x, S - 1 - 180));
    const cy = Math.max(180, Math.min(y, S - 1 - 180));
    const d = Math.hypot(x - cx, y - cy);
    if (d <= 180) put(x, y, r, g, b, Math.round(255 * Math.max(0, Math.min(1, 180 - d + 0.5))));
  }
}
// three rows (table), decreasing opacity
roundedRect(200, 220, 824, 300, 24, [255, 255, 255, 0.95]);
roundedRect(200, 340, 824, 420, 24, [255, 255, 255, 0.7]);
roundedRect(200, 460, 824, 540, 24, [255, 255, 255, 0.45]);
// arrow shaft + head
roundedRect(472, 560, 552, 720, 20, [255, 255, 255, 1]);
for (let y = 700; y < 840; y++) {
  const w = (840 - y) * 1.3;
  for (let x = Math.round(512 - w); x <= Math.round(512 + w); x++) put(x, y, 255, 255, 255, 255);
}
// file line
roundedRect(200, 860, 824, 900, 20, [255, 255, 255, 1]);

// encode PNG
function crc32(buf) {
  let c, crc = 0xffffffff;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = (crc >>> 8) ^ c;
  }
  return (crc ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}
const raw = Buffer.alloc((S * 4 + 1) * S);
for (let y = 0; y < S; y++) { raw[y * (S * 4 + 1)] = 0; px.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4); }
const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4); ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', ihdr),
  chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
  chunk('IEND', Buffer.alloc(0)),
]);
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, png);
console.log('wrote', out, png.length, 'bytes');
