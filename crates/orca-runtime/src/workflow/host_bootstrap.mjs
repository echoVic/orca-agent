import { createHash } from 'node:crypto';
import { readSync } from 'node:fs';
function receive(length) {
  const bytes = Buffer.alloc(length);
  for (let offset = 0; offset < bytes.length;) {
    const read = readSync(0, bytes, offset, bytes.length - offset, null);
    if (!read) throw new Error('workflow host input ended early');
    offset += read;
  }
  return bytes;
}
const module = receive(Number(process.argv[8]));
if (createHash('sha256').update(module).digest('hex') !== process.argv[9]) {
  throw new Error('workflow host module changed before launch');
}
const script = receive(Number(process.argv[10]));
const host = await import('data:text/javascript;base64,' + module.toString('base64'));
host.run(script);
