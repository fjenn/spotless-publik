// Point an installed Spotless binary's update feed at the test feed, byte for byte the same
// length: releases/latest/download/latest.json → releases/download/e2eupd/latest.json. Nothing else
// in the app changes, so what runs is the shipped updater against a real newer release.
import fs from 'node:fs';
const [file] = process.argv.slice(2);
const from = Buffer.from('releases/latest/download/latest.json');
const to = Buffer.from('releases/download/e2eupd/latest.json');
if (from.length !== to.length) throw new Error('length mismatch');
const buf = fs.readFileSync(file);
let count = 0;
for (let i = buf.indexOf(from); i !== -1; i = buf.indexOf(from, i + 1)) {
	to.copy(buf, i);
	count++;
}
if (!count) throw new Error(`feed URL not found in ${file}`);
fs.writeFileSync(file, buf);
console.log(`patched ${count} occurrence(s) in ${file}`);
