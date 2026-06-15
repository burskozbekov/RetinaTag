// Extract the largest inline <script> (no src=) from dist/index.html and
// write it to a temp .js so `node --check` can validate syntax.
const fs = require('fs');
const path = require('path');
const html = fs.readFileSync(process.argv[2], 'utf8');
const re = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
let m, best = '';
while ((m = re.exec(html)) !== null) {
  const attrs = m[1] || '';
  if (/\bsrc\s*=/.test(attrs)) continue;
  if (m[2].length > best.length) best = m[2];
}
const out = path.join(require('os').tmpdir(), 'retina_script_check.js');
fs.writeFileSync(out, best, 'utf8');
console.log('Largest inline script: ' + best.length + ' bytes -> ' + out);
