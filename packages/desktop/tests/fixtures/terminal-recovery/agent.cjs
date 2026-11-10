const fs = require('node:fs');
const proof = process.argv[2];
if (!proof) throw new Error('Pass a disposable proof file');
process.stdin.setRawMode(true);
process.stdin.setEncoding('utf8');
process.stdout.write('\x1b[?1049h\x1b[?2004h');
function record(input) {
  const value = { pid: process.pid, cwd: process.cwd(), input, cols: process.stdout.columns, rows: process.stdout.rows };
  fs.appendFileSync(proof, JSON.stringify(value) + '\n');
  process.stdout.write('\r\n' + JSON.stringify(value) + '\r\n');
}
let line = '';
record('ready');
process.stdin.on('data', data => {
  for (const char of data) {
    if (char === '\r' || char === '\n') {
      record(line);
      if (line === 'stop') { process.stdout.write('\x1b[?2004l\x1b[?1049l'); process.exit(0); }
      line = '';
    } else if (char === '\u007f') line = line.slice(0, -1);
    else line += char;
  }
});
process.stdout.on('resize', () => record('resize'));
