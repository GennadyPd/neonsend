// Смоук-тест страницы NeonSend без зависимостей: headless Chrome + CDP.
// Запуск: node tests/ui_smoke.mjs   (нужны запущенное приложение и Chrome)
// Код возврата 0 — все проверки прошли.
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs';

const PORT = Number(process.env.PORT || 8765);
const DBG = 9333;
const CHROME = process.env.CHROME || 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe';
const profile = path.join(os.tmpdir(), 'neonsend-ui-test');
const sleep = ms => new Promise(r => setTimeout(r, ms));

const lan = () => {
  const bad = /virtual|vmware|vbox|hyper-v|loopback|bluetooth|vethernet/i;
  const all = Object.entries(os.networkInterfaces()).filter(([n]) => !bad.test(n)).flatMap(([, v]) => v || []);
  const v4 = all.filter(a => a.family === 'IPv4' && !a.internal).map(a => a.address);
  const rank = ip => ip.startsWith('192.168.') ? 0 : ip.startsWith('10.') ? 1 : /^172\.(1[6-9]|2\d|3[01])\./.test(ip) ? 2 : 3;
  return v4.sort((a, b) => rank(a) - rank(b))[0];
};
const url = process.env.URL || `http://${lan()}:${PORT}/`;
console.log('страница:', url);

fs.rmSync(profile, { recursive: true, force: true });
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${DBG}`, `--user-data-dir=${profile}`,
  '--no-first-run', '--disable-gpu', '--window-size=390,844'], { stdio: 'ignore' });

const fails = [];
const check = (name, ok, extra = '') => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);
  if (!ok) fails.push(name);
};

let ws;
try {
  let ver;
  for (let i = 0; i < 40 && !ver; i++) {
    await sleep(250);
    ver = await fetch(`http://127.0.0.1:${DBG}/json/version`).then(r => r.json()).catch(() => null);
  }
  if (!ver) throw new Error('Chrome не поднялся на порту ' + DBG);

  const tab = await (await fetch(`http://127.0.0.1:${DBG}/json/new?url=about:blank`, { method: 'PUT' })).json();
  ws = new WebSocket(tab.webSocketDebuggerUrl);
  let n = 0;
  const waiters = new Map();
  const errors = [];
  ws.onmessage = ev => {
    const m = JSON.parse(ev.data);
    if (m.id && waiters.has(m.id)) { waiters.get(m.id)(m); waiters.delete(m.id); }
    if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
    if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') errors.push(m.params.args.map(a => a.value ?? a.description).join(' '));
  };
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  const send = (method, params = {}) => new Promise(res => {
    const id = ++n;
    waiters.set(id, res);
    ws.send(JSON.stringify({ id, method, params }));
  });
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
  const nav = await send('Page.navigate', { url });
  check('страница загрузилась', !nav.result?.errorText, nav.result?.errorText || '');

  const evalJs = async expr => (await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })).result?.result?.value;

  let page;
  for (let i = 0; i < 30; i++) {
    await sleep(300);
    page = await evalJs(`(() => ({ h1: !!document.querySelector('h1'), token: !!localStorage.getItem('neon-token') }))()`);
    if (page?.h1 && page.token) break;
  }
  check('токен в localStorage', !!page?.token);
  check('<h1> на странице', !!page?.h1);

  const dom = await evalJs(`(() => ({
    label: !!document.querySelector('label[for="nameInput"]'),
    bars: document.querySelectorAll('[role="progressbar"]').length,
    dialogs: document.querySelectorAll('[role="dialog"][aria-labelledby]').length,
    screens: [...document.querySelectorAll('section.card:not(.hidden)')].length
  }))()`);
  check('подпись у поля имени', !!dom?.label);
  check('две полосы прогресса с role', dom?.bars === 2, String(dom?.bars));

  const api = await evalJs(`fetch('/api/state?id=0').then(async r => ({ status: r.status, body: await r.json() })).catch(e => ({ status: 0, err: String(e) }))`);
  check('API доступен странице (auth через fetch-обёртку)', api?.status === 200, `HTTP ${api?.status}`);
  check('в ответе список устройств', Array.isArray(api?.body?.devices), '');

  check('нет ошибок в консоли', errors.length === 0, errors.slice(0, 2).join(' | '));
} catch (e) {
  check('тест выполнился', false, e.message);
} finally {
  try { ws?.close(); } catch {}
  chrome.kill();
  await sleep(500);
  try { fs.rmSync(profile, { recursive: true, force: true }); } catch {}
}
console.log(fails.length ? `\nПРОВАЛЕНО: ${fails.length}` : '\nВСЕ ПРОВЕРКИ ПРОШЛИ');
process.exit(fails.length ? 1 : 0);
