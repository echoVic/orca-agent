import { createHash } from 'node:crypto';
import { readSync } from 'node:fs';

const stdinWait = new Int32Array(new SharedArrayBuffer(4));
const startupDeadline = performance.now() + 30_000;

function receive(length) {
  const bytes = Buffer.alloc(length);
  for (let offset = 0; offset < bytes.length;) {
    let read = 0;
    try {
      // Read exactly this payload so the host's stdin stream sees every reply.
      read = readSync(0, bytes, offset, bytes.length - offset, null);
    } catch (error) {
      if (error.code === 'EAGAIN') {
        const remaining = startupDeadline - performance.now();
        if (remaining <= 0) {
          throw new Error('workflow host timed out waiting for startup input', { cause: error });
        }
        Atomics.wait(stdinWait, 0, 0, Math.min(5, remaining));
        continue;
      }
      if (error.code !== 'EOF') throw error;
    }
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
