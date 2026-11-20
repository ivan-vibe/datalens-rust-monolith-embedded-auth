#!/usr/bin/env node
// Browser-level regression for the dataset editor: add a connection, drag two tables onto the
// canvas and check that the second source and its relation survive the validator round-trip.
//
// This is the scenario that found the original bug (`field ... refers to unknown source`):
// the first table already had fields, the old control-api dropped its source on the second
// drop, and the editor adopted the broken answer.
//
// Prerequisites:
//   * the monolith and the UI are running (see docs/TESTING.md);
//   * Chrome with `--remote-debugging-port=9224`;
//   * the demo workbook + `pg` connection with `public.orders` / `public.managers`.
//
// Usage:
//   node scripts/ui_dataset_editor_e2e.mjs
//   node scripts/ui_dataset_editor_e2e.mjs --ui http://localhost:8091 \
//       --workbook z4wtz6tg5194o --out /tmp/e2e --keep-page
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { connect, createPage, closePage } from './cdp.mjs';

const args = process.argv.slice(2);
function arg(name, fallback) {
  const index = args.indexOf(`--${name}`);
  return index === -1 ? fallback : args[index + 1];
}
const has = (name) => args.includes(`--${name}`);

const cdpUrl = arg('cdp', 'http://127.0.0.1:9224');
const ui = arg('ui', 'http://localhost:8091');
const workbook = arg('workbook', 'z4wtz6tg5194o');
const login = arg('login', 'admin');
const password = arg('password', 'admin');
const outDir = arg('out', '/tmp/dataset-editor-e2e');
mkdirSync(outDir, { recursive: true });

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const fail = (message) => { throw new Error(message); };

const page = await createPage(cdpUrl, 'about:blank');
const cdp = await connect({ cdpUrl, targetId: page.id });
let report = { ok: false, url: null, steps: [], console: [], network: [] };

try {
  // Capture the API answers the editor receives. The gateway is an Express proxy, and the
  // browser-level DevTools response bodies are not always available, so hook fetch + XHR.
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', {
    source: `(() => {
      window.__e2eApi = [];
      const wanted = /(validators|validateDataset|getPreview)/;
      const record = (url, status, body) => {
        try { window.__e2eApi.push({ url: String(url), status, body: String(body).slice(0, 200000) }); } catch {}
      };
      const originalFetch = window.fetch;
      window.fetch = async (...args) => {
        const response = await originalFetch(...args);
        const url = typeof args[0] === 'string' ? args[0] : (args[0]?.url ?? '');
        if (wanted.test(url)) response.clone().text().then((body) => record(url, response.status, body)).catch(() => {});
        return response;
      };
      const originalOpen = XMLHttpRequest.prototype.open;
      const originalSend = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.open = function (method, url, ...rest) {
        this.__e2eUrl = url;
        return originalOpen.call(this, method, url, ...rest);
      };
      XMLHttpRequest.prototype.send = function (...args) {
        if (wanted.test(String(this.__e2eUrl))) {
          this.addEventListener('load', () => {
            try { record(this.__e2eUrl, this.status, this.responseText); } catch {}
          });
        }
        return originalSend.apply(this, args);
      };
    })();`,
  });
  await cdp.navigate(`${ui}/workbooks/${workbook}/datasets/new`);
  await sleep(3500);

  // A fresh tab may land on the sign-in page even when the browser profile has a stale cookie.
  await cdp.evalJs(`(async () => {
    if (!location.pathname.startsWith('/auth/signin')) return 'already';
    const inputs = [...document.querySelectorAll('input')];
    if (inputs.length < 2) return 'no-form';
    const set = (element, value) => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      setter.call(element, value);
      element.dispatchEvent(new Event('input', { bubbles: true }));
    };
    set(inputs[0], ${JSON.stringify(login)});
    set(inputs[1], ${JSON.stringify(password)});
    const button = [...document.querySelectorAll('button')].find((candidate) =>
      (candidate.innerText || '').trim() === 'Sign in');
    button?.click();
    return 'submitted';
  })()`);
  await sleep(4000);
  report.url = await cdp.evalJs('location.href');
  if (report.url.includes('/auth/signin')) fail(`still on sign-in page: ${report.url}`);
  report.steps.push('editor opened');

  // Pick the demo PostgreSQL connection in the Sources panel.
  await cdp.evalJs(`(() => {
    const add = [...document.querySelectorAll('button')].find((candidate) =>
      (candidate.innerText || '').trim() === 'Add');
    if (!add) throw new Error('Sources "+ Add" button not found');
    add.click();
  })()`);
  await sleep(1200);
  const picked = await cdp.evalJs(`(() => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    let node;
    while ((node = walker.nextNode())) {
      if (node.textContent.trim() !== 'pg') continue;
      const row = node.parentElement?.closest('.dl-core-workbook-navigation-minimal__row');
      if (row) { row.click(); return true; }
    }
    return false;
  })()`);
  if (!picked) fail('connection "pg" not found in the Add dialog');
  await sleep(3000);
  report.steps.push('pg connection selected');

  const draggableTables = await cdp.evalJs(
    `[...document.querySelectorAll('[draggable=true]')].map((element) => (element.innerText || '').trim())`,
  );
  for (const table of ['orders', 'managers']) {
    if (!draggableTables.some((title) => title.includes(table))) {
      fail(`table ${table} is not listed; found ${JSON.stringify(draggableTables)}`);
    }
  }

  // react-dnd HTML5 backend: a synthetic dragstart + dragenter/dragover/drop on the target is
  // enough; the editor's handlers are attached to the source/target DOM nodes.
  const drag = async (table, targetSelector, targetText) => cdp.evalJs(`(() => {
    const source = [...document.querySelectorAll('[draggable=true]')]
      .find((element) => (element.innerText || '').includes(${JSON.stringify(table)}));
    if (!source) return { ok: false, reason: 'source not found' };
    let target = ${targetSelector};
    if (!target && ${JSON.stringify(targetText)}) {
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      let node;
      while ((node = walker.nextNode())) {
        if ((node.textContent || '').includes(${JSON.stringify(targetText)})) {
          target = node.parentElement;
          break;
        }
      }
    }
    if (!target) return { ok: false, reason: 'target not found' };
    const rect = target.getBoundingClientRect();
    const x = rect.x + rect.width / 2;
    const y = rect.y + rect.height / 2;
    const dataTransfer = new DataTransfer();
    for (const [type, element] of [
      ['dragstart', source],
      ['dragenter', target],
      ['dragover', target],
      ['drop', target],
    ]) {
      element.dispatchEvent(new DragEvent(type, {
        bubbles: true, cancelable: true, dataTransfer, clientX: x, clientY: y,
      }));
    }
    return { ok: true };
  })()`);

  // 1st table: drop on the "Drag and drop tables to create links" placeholder.
  let dropped = await drag('orders', 'null', 'Drag and drop tables to create links');
  if (!dropped.ok) fail(`first drop failed: ${dropped.reason}`);
  await sleep(4000);

  // 2nd table: drop onto the relations map; the editor adds the source and an empty relation.
  dropped = await drag('managers', "document.querySelector('.relations-map')", null);
  if (!dropped.ok) fail(`second drop failed: ${dropped.reason}`);
  await sleep(5000);
  report.steps.push('two tables dropped');

  const canvasTitles = await cdp.evalJs(
    `[...document.querySelectorAll('.avatar__avatar-title')].map((element) => (element.innerText || '').trim())`,
  );
  if (!canvasTitles.some((title) => title.includes('orders')) || !canvasTitles.some((title) => title.includes('managers'))) {
    fail(`canvas shows ${JSON.stringify(canvasTitles)} instead of both tables`);
  }
  report.steps.push('both avatars visible');

  const validations = cdp.net.filter((entry) => entry.url.includes('/validateDataset'));
  const bad = validations.filter((entry) => entry.status !== 200);
  if (!validations.length || bad.length) {
    fail(`validateDataset statuses: ${validations.map((entry) => entry.status).join(', ') || 'none'}`);
  }
  report.steps.push(`validateDataset x${validations.length} all 200`);

  // The last validation answer must carry both sources and keep every avatar/field reference
  // valid. The answer is captured from the page's own fetch/XHR (see the init script above).
  const captured = await cdp.evalJs('window.__e2eApi || []');
  report.api = captured.map((call) => ({ url: call.url, status: call.status }));
  const answers = captured
    .filter((call) => /validateDataset|validators/.test(call.url) && call.status === 200)
    .map((call) => { try { return JSON.parse(call.body); } catch { return null; } })
    .filter(Boolean);
  const answer = answers.reverse().find((candidate) => candidate.dataset);
  if (!answer) fail('no validation answer captured in the page');
  const dataset = answer.dataset;
  const sourceIds = new Set((dataset.sources ?? []).map((source) => source.id));
  const avatarIds = new Set((dataset.source_avatars ?? []).map((avatar) => avatar.id));
  for (const avatar of dataset.source_avatars ?? []) {
    if (!sourceIds.has(avatar.source_id)) fail(`avatar ${avatar.id} points at missing source ${avatar.source_id}`);
  }
  for (const field of dataset.result_schema ?? []) {
    if (field.calc_mode === 'direct' && !avatarIds.has(field.avatar_id)) {
      fail(`field ${field.guid} points at missing avatar ${field.avatar_id}`);
    }
  }
  if ((dataset.sources ?? []).length !== 2 || (dataset.source_avatars ?? []).length !== 2) {
    fail(`answer has ${(dataset.sources ?? []).length} sources / ${(dataset.source_avatars ?? []).length} avatars, expected 2/2`);
  }
  if ((dataset.avatar_relations ?? []).length !== 1) fail('answer lost the relation');
  const titles = (dataset.result_schema ?? []).map((field) => field.title);
  const duplicates = [...new Set(titles)].filter((title) => titles.filter((candidate) => candidate === title).length > 1);
  if (duplicates.length) fail(`duplicate field titles: ${JSON.stringify(duplicates)}`);
  report.steps.push(`answer: sources=2 avatars=2 relations=1 fields=${titles.length}`);

  // Before the join is configured the preview reports the documented 2-source error.
  const previews = captured.filter((call) => /getPreview/.test(call.url));
  const lastPreview = previews.at(-1);
  if (lastPreview && lastPreview.status !== 400) {
    fail(`preview before join should be HTTP 400, got ${lastPreview.status}`);
  }
  report.steps.push('preview before join: HTTP 400 (expected)');

  const unknownSource = cdp.consoleMessages.find((message) => /unknown source/i.test(message.text));
  if (unknownSource) fail(`console still reports an unknown source: ${unknownSource.text.slice(0, 200)}`);
  report.console = cdp.consoleMessages.slice(-50);
  report.network = cdp.net.map((entry) => ({
    method: entry.method,
    url: entry.url,
    status: entry.status,
    responseBody: entry.responseBody?.slice(0, 4000),
  }));
  await cdp.screenshot(join(outDir, 'dataset-editor-second-drop.png'));
  report.ok = true;
  report.steps.push('screenshot saved');
  console.log('PASS browser dataset editor: second table added, references consistent');
} catch (error) {
  try { await cdp.screenshot(join(outDir, 'dataset-editor-failure.png')); } catch {}
  report.error = String(error);
  console.error(`FAIL browser dataset editor: ${error}`);
  process.exitCode = 1;
} finally {
  writeFileSync(join(outDir, 'report.json'), JSON.stringify(report, null, 2));
  cdp.close();
  if (!has('keep-page')) await closePage(cdpUrl, page.id);
  else console.log(`page kept: ${page.id}`);
  console.log(`report: ${join(outDir, 'report.json')}`);
}
