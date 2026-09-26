// Runs the newest installer from `tauri build` (one-click: closes the app, updates it, reopens it).
import { readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { spawn } from 'node:child_process';

const dir = 'src-tauri/target/release/bundle/nsis';
const setup = readdirSync(dir)
  .filter((f) => f.endsWith('-setup.exe'))
  .map((f) => join(dir, f))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!setup) {
  console.error(`No installer found in ${dir}. Run "npm run build" first.`);
  process.exit(1);
}
console.log(`Installing ${setup}`);
spawn(setup, [], { detached: true, stdio: 'ignore' }).unref();
