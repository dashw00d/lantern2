// Regenerates docs/screenshots/*.png from the isolated demo daemon started by demo.sh.
//
//   scripts/screenshots/demo.sh start
//   (cd scripts/screenshots && npm install && npx playwright-core install chromium && node capture.mjs)
//   scripts/screenshots/gui.sh            # native GUI screenshot (needs xvfb, dbus-run-session, ffmpeg)
//   scripts/screenshots/demo.sh stop
//
// Web UI shots are real headless-Chromium captures of the control center. Terminal shots run the real
// commands and render their actual output (ANSI colors preserved) into a terminal-styled page.
import { chromium } from 'playwright-core';
import sharp from 'sharp';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const out = resolve(process.env.SHOT_OUT ?? join(here, '../../docs/screenshots'));
const root = process.env.LANTERN_SHOT_ROOT ?? '/tmp/lantern-shots';
const port = process.env.LANTERN_SHOT_PORT ?? '4790';
const base = `http://127.0.0.1:${port}`;
mkdirSync(out, { recursive: true });

const browser = await chromium.launch(
  process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {},
);

async function save(name, buffer) {
  const png = await sharp(buffer).png({ palette: true, quality: 92, compressionLevel: 9, effort: 10 }).toBuffer();
  writeFileSync(join(out, name), png);
  console.log(`${name}  ${Math.round(png.length / 1024)} KB`);
}

// Guard: fail loudly if anything from the host (home paths, user name, a real Lantern's projects) leaks in.
const leak = /\/home\/|\/Users\/|glow2|austins|\b(ryan|dashw00d)\b/i;
function assertClean(label, text) {
  const m = text.match(leak);
  if (m) throw new Error(`${label}: output contains host-specific text (${m[0]}); refusing to write screenshot`);
}

// ---- Web UI --------------------------------------------------------------------------------------
async function openUI(colorScheme) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme })).newPage();
  await page.goto(base);
  await page.getByText('Crashed').first().waitFor();
  await page.getByText('Watch job queue').waitFor();
  await page.waitForTimeout(1500);
  assertClean('web UI', await page.evaluate(() => document.body.innerText));
  return page;
}

for (const scheme of ['light', 'dark']) {
  const page = await openUI(scheme);
  // Capture the Tools and Commands sections; stop above "Also running on this machine", which lists
  // every listener on whatever machine the daemon runs on.
  const clipH = await page.evaluate(() => {
    const h = [...document.querySelectorAll('h2')].find((e) => e.textContent.startsWith('Also running'));
    return Math.round((h?.getBoundingClientRect().top ?? document.body.scrollHeight) + scrollY - 28);
  });
  await save(scheme === 'light' ? 'dashboard.png' : 'dashboard-dark.png',
    await page.screenshot({ clip: { x: 0, y: 0, width: 1280, height: clipH }, fullPage: true }));
  if (scheme === 'light') {
    await page.setViewportSize({ width: 1280, height: 480 }); // a short window keeps the drawer on the newest lines
    await page.getByRole('button', { name: 'Logs' }).nth(2).click(); // Mailer Worker's card (sorted by name)
    await page.getByText('cannot connect to Redis').first().waitFor();
    await page.waitForTimeout(500);
    assertClean('logs drawer', await page.evaluate(() => document.body.innerText));
    await save('logs.png', await page.screenshot());
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.reload();
    await page.getByRole('button', { name: 'Add tool' }).click();
    await page.getByText('marketing-site').first().waitFor();
    assertClean('add tool dialog', await page.evaluate(() => document.body.innerText));
    await save('add-tool.png', await page.screenshot());
  }
  await page.context().close();
}

// ---- Terminal renders ------------------------------------------------------------------------------
const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const palette = { 30: '#6e7681', 31: '#ff7b72', 32: '#3fb950', 33: '#d29922', 34: '#58a6ff', 35: '#bc8cff', 36: '#39c5cf', 37: '#c9d1d9',
  90: '#8b949e', 91: '#ffa198', 92: '#56d364', 93: '#e3b341', 94: '#79c0ff', 95: '#d2a8ff', 96: '#56d4dd', 97: '#f0f6fc' };
function ansiToHtml(text) {
  let open = false;
  const html = esc(text).replace(/\x1b\[([0-9;]*)m/g, (_, codes) => {
    let style = '';
    for (const c of codes.split(';').map(Number)) {
      if (palette[c]) style += `color:${palette[c]};`;
      else if (c === 1) style += 'font-weight:700;';
      else if (c === 2) style += 'opacity:.7;';
    }
    const s = (open ? '</span>' : '') + (style ? `<span style="${style}">` : '');
    open = Boolean(style);
    return s;
  });
  return html + (open ? '</span>' : '');
}
// Runs `cmd` in a pty (so the CLI emits its normal colors) with the demo environment and returns the output.
// The command goes through a script file so nothing is expanded by an outer shell with the caller's real environment.
const tmp = mkdtempSync(join(tmpdir(), 'lantern-shot-'));
function run(cmd) {
  const file = join(tmp, 'cmd.sh');
  writeFileSync(file, `source ${root}/env.sh\ncd /tmp\n${cmd}\n`);
  let text;
  try {
    text = execFileSync('script', ['-qec', `bash ${file}`, '/dev/null'], { encoding: 'utf8' });
  } catch (e) {
    if (!e.stdout) throw e;
    text = String(e.stdout); // a non-zero exit (e.g. manifest check) still has output
  }
  return text.replace(/\r/g, '');
}
async function terminal(name, title, steps, width = 1100) {
  const body = steps.map(({ shown, cmd }) => {
    const output = run(cmd).replace(/\n+$/, '');
    assertClean(name, output);
    return `<div class="cmd"><span class="p">$</span> ${esc(shown ?? cmd)}</div><div class="o">${ansiToHtml(output)}</div>`;
  }).join('');
  const page = await (await browser.newContext({ viewport: { width, height: 1400 } })).newPage();
  await page.setContent(`<style>
    body{margin:0;background:#0d1117;font:14px/1.5 "DejaVu Sans Mono","Liberation Mono",monospace;color:#c9d1d9}
    .bar{background:#161b22;border-bottom:1px solid #30363d;padding:10px 16px;font:12px system-ui,sans-serif;color:#8b949e;display:flex;gap:8px;align-items:center}
    .dot{width:11px;height:11px;border-radius:50%;background:#30363d}
    .term{padding:14px 18px 18px}.cmd{margin-top:14px;color:#f0f6fc}.cmd:first-child{margin-top:0}.p{color:#3fb950}
    .o{white-space:pre;margin-top:4px}</style>
    <div class="bar"><span class="dot"></span><span class="dot"></span><span class="dot"></span><span style="margin-left:8px">${esc(title)}</span></div>
    <div class="term" id="t">${body}</div>`);
  const h = Math.ceil(await page.evaluate(() => document.getElementById('t').getBoundingClientRect().bottom));
  await save(name, await page.screenshot({ clip: { x: 0, y: 0, width, height: h } }));
  await page.context().close();
}

await terminal('cli.png', 'lantern: list, logs, validate', [
  { cmd: 'lantern ls --no-listeners' },
  { cmd: 'lantern logs mailer-worker -n 4 -t' },
  { cmd: 'lantern manifest check /tmp/demo-broken' },
]);
const mcp = `curl -s "$(lantern mcp url)" -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \\
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}'`;
await terminal('mcp.png', 'lantern: the MCP endpoint agents connect to', [
  { cmd: 'lantern mcp url' },
  { cmd: 'lantern mcp install claude-code --print | sed -n "/^Claude Code:/,/^$/p"', shown: 'lantern mcp install claude-code --print   # Claude Code section' },
  {
    shown: 'curl -s "$(lantern mcp url)" -d \'{"method":"tools/call","params":{"name":"list_projects"}, ...}\' | jq ...',
    cmd: `${mcp} | jq -r '.result.content[0].text | fromjson[] | [.id, .status, (.port // "-"), .url] | @tsv' | column -t`,
  },
]);

await browser.close();
rmSync(tmp, { recursive: true, force: true });
