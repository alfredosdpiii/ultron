import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
for (const dir of ['host', 'scripts', 'tests']) {
  for (const file of readdirSync(dir).filter(x => x.endsWith('.mjs'))) {
    const result=spawnSync(process.execPath,['--check',`${dir}/${file}`],{stdio:'inherit'});
    if(result.status!==0) process.exit(result.status ?? 1);
  }
}
console.log('JavaScript syntax checks passed. Pi TypeScript loading is checked by scripts/smoke-pi.py.');
