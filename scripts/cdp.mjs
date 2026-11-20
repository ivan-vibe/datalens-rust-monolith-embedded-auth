#!/usr/bin/env node
// Minimal Chrome DevTools Protocol client used by the browser-level dataset-editor test.
//
// No dependencies: Node 22 ships a global WebSocket. Chrome must be started with
// `--remote-debugging-port=9224` (or pass --cdp).
//
// CLI:
//   node scripts/cdp.mjs --url http://localhost:8091/ --wait 3000 --shot /tmp/x.png --net
//   node scripts/cdp.mjs --eval "document.title" --click-text "Save"
//   node scripts/cdp.mjs --mouse 700 20 --drag 360 270 620 250
//
// Import:
//   const { connect } = await import('./cdp.mjs');
//   const cdp = await connect({ cdpUrl: 'http://127.0.0.1:9224' });
import { writeFileSync } from 'node:fs';

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function listTargets(cdpUrl) {
  const response = await fetch(`${cdpUrl}/json/list`);
  return response.json();
}

/** Create a new tab (HTTP endpoint; no WebSocket needed). */
export async function createPage(cdpUrl, url = 'about:blank') {
  const response = await fetch(`${cdpUrl}/json/new?${encodeURIComponent(url)}`, { method: 'PUT' });
  return response.json();
}

export async function closePage(cdpUrl, id) {
  await fetch(`${cdpUrl}/json/close/${id}`).catch(() => {});
}

/**
 * Attach to the first page target (or the one whose URL contains `match`).
 * Returns a small client with the calls the scenarios need.
 */
export async function connect({ cdpUrl = 'http://127.0.0.1:9224', match = null, targetId = null } = {}) {
  const targets = await listTargets(cdpUrl);
  const pages = targets.filter((target) => target.type === 'page');
  const page = (targetId && pages.find((target) => target.id === targetId))
    || (match && pages.find((target) => target.url.includes(match)))
    || pages[0];
  if (!page) throw new Error('no page target in Chrome; start it with --remote-debugging-port');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  let nextId = 1;
  const pending = new Map();
  const net = [];
  const consoleMessages = [];
  const inflight = new Map();

  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      message.error ? reject(new Error(JSON.stringify(message.error))) : resolve(message.result);
      return;
    }
    const { method, params = {} } = message;
    if (method === 'Network.requestWillBeSent') {
      const entry = {
        id: params.requestId,
        method: params.request.method,
        url: params.request.url,
        t0: params.timestamp,
        postData: params.request.postData,
      };
      net.push(entry);
      inflight.set(params.requestId, entry);
      if (entry.postData === undefined && entry.method !== 'GET') {
        send('Network.getRequestPostData', { requestId: params.requestId })
          .then((result) => { entry.postData = result.postData; })
          .catch(() => {});
      }
    } else if (method === 'Network.responseReceived') {
      const entry = inflight.get(params.requestId);
      if (entry) { entry.status = params.response.status; entry.mime = params.response.mimeType; }
    } else if (method === 'Network.loadingFinished') {
      const entry = inflight.get(params.requestId);
      if (entry) {
        entry.t1 = params.timestamp;
        if (/validators|preview|createDataset/.test(entry.url)) {
          send('Network.getResponseBody', { requestId: params.requestId })
            .then((result) => {
              entry.responseBody = result.base64Encoded
                ? Buffer.from(result.body, 'base64').toString('utf8')
                : result.body;
            })
            .catch(() => {});
        }
      }
    } else if (method === 'Network.loadingFailed') {
      const entry = inflight.get(params.requestId);
      if (entry) { entry.failed = params.errorText; }
    } else if (method === 'Runtime.consoleAPICalled') {
      consoleMessages.push({
        type: params.type,
        text: params.args.map((arg) => arg.value ?? arg.description ?? '').join(' '),
      });
    } else if (method === 'Runtime.exceptionThrown') {
      consoleMessages.push({
        type: 'exception',
        text: params.exceptionDetails.exception?.description ?? params.exceptionDetails.text,
      });
    }
  });

  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });

  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', reject, { once: true });
  });
  await send('Page.enable');
  await send('Network.enable');
  await send('Runtime.enable');

  const evalJs = async (expression) => {
    const result = await send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    });
    if (result.exceptionDetails) {
      throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    }
    return result.result?.value;
  };

  const navigate = async (url) => {
    const loaded = new Promise((resolve) => {
      const handler = (event) => {
        const message = JSON.parse(event.data);
        if (message.method === 'Page.loadEventFired') {
          ws.removeEventListener('message', handler);
          resolve();
        }
      };
      ws.addEventListener('message', handler);
      setTimeout(resolve, 30000);
    });
    await send('Page.navigate', { url });
    await loaded;
  };

  const clickText = async (wanted) => {
    const point = await evalJs(`(() => {
      const wanted = ${JSON.stringify(wanted)};
      const all = [...document.querySelectorAll('button,a,[role=button],[draggable=true]')];
      let element = all.find((candidate) => (candidate.innerText || candidate.textContent || '').trim() === wanted)
        || all.find((candidate) => (candidate.innerText || candidate.textContent || '').trim().includes(wanted));
      if (!element) {
        const leaves = [...document.querySelectorAll('body *')].filter(
          (candidate) => (candidate.innerText || candidate.textContent || '').trim() === wanted,
        );
        leaves.sort((a, b) => {
          const ra = a.getBoundingClientRect();
          const rb = b.getBoundingClientRect();
          return ra.width * ra.height - rb.width * rb.height;
        });
        element = leaves[0];
        if (element) element = element.closest('button,a,[role=button],[role=option],li,[tabindex]') || element;
      }
      if (!element) return null;
      element.scrollIntoView({ block: 'center' });
      const rect = element.getBoundingClientRect();
      return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
    })()`);
    if (!point) return false;
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await send('Input.dispatchMouseEvent', {
        type,
        x: point.x,
        y: point.y,
        button: type === 'mouseMoved' ? 'none' : 'left',
        clickCount: 1,
      });
    }
    return true;
  };

  const clickAt = async (x, y) => {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await send('Input.dispatchMouseEvent', {
        type,
        x,
        y,
        button: type === 'mouseMoved' ? 'none' : 'left',
        clickCount: 1,
      });
    }
  };

  const screenshot = async (path) => {
    const { data } = await send('Page.captureScreenshot', { format: 'png' });
    writeFileSync(path, Buffer.from(data, 'base64'));
  };

  const close = () => ws.close();

  return { send, evalJs, navigate, clickText, clickAt, screenshot, close, net, consoleMessages, page, sleep };
}

// ---------------------------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------------------------

const isMain = process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop());
if (isMain) {
  const args = process.argv.slice(2);
  const options = [];
  for (let index = 0; index < args.length; index++) {
    const arg = args[index];
    if (['--net', '--console'].includes(arg)) options.push({ arg });
    else if (['--cdp', '--url', '--eval', '--wait', '--shot', '--click-text', '--cookie', '--mouse', '--drag', '--dump-net', '--viewport'].includes(arg)) {
      if (arg === '--mouse' || arg === '--drag') {
        const count = arg === '--mouse' ? 2 : 4;
        options.push({ arg, values: args.slice(index + 1, index + 1 + count).map(Number) });
        index += count;
      } else {
        options.push({ arg, value: args[++index] });
      }
    } else {
      console.error('unknown argument', arg);
      process.exit(2);
    }
  }
  const cdp = await connect({ cdpUrl: options.find((option) => option.arg === '--cdp')?.value });
  for (const option of options) {
    if (option.arg === '--url') await cdp.navigate(option.value);
    else if (option.arg === '--wait') await sleep(Number(option.value));
    else if (option.arg === '--eval') console.log(JSON.stringify(await cdp.evalJs(option.value), null, 2));
    else if (option.arg === '--click-text') console.log('click', await cdp.clickText(option.value));
    else if (option.arg === '--mouse') await cdp.clickAt(...option.values);
    else if (option.arg === '--drag') {
      const [x1, y1, x2, y2] = option.values;
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: x1, y: y1, button: 'none' });
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: x1, y: y1, button: 'left', clickCount: 1 });
      await sleep(120);
      for (let step = 1; step <= 12; step++) {
        const x = x1 + ((x2 - x1) * step) / 12;
        const y = y1 + ((y2 - y1) * step) / 12;
        await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, button: 'left', buttons: 1 });
        await sleep(60);
      }
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: x2, y: y2, button: 'left', clickCount: 1 });
    } else if (option.arg === '--shot') {
      await cdp.screenshot(option.value);
      console.log('shot', option.value);
    } else if (option.arg === '--cookie') {
      const cookie = JSON.parse(option.value);
      await cdp.send('Network.setCookie', { ...cookie, path: '/', httpOnly: true });
    } else if (option.arg === '--dump-net') {
      await sleep(500);
      writeFileSync(option.value, JSON.stringify(cdp.net, null, 2));
      console.log('net dumped', option.value);
    } else if (option.arg === '--net') {
      for (const entry of cdp.net) {
        const ms = entry.t0 && entry.t1 ? Math.round((entry.t1 - entry.t0) * 1000) : null;
        console.log(`${entry.status ?? (entry.failed ? 'ERR' : '...')} ${entry.method} ${entry.url}${ms !== null ? ` ${ms}ms` : ''}`);
      }
      cdp.net.length = 0;
    } else if (option.arg === '--console') {
      for (const message of cdp.consoleMessages) console.log(`[${message.type}] ${message.text.slice(0, 400)}`);
      cdp.consoleMessages.length = 0;
    }
  }
  cdp.close();
  process.exit(0);
}
