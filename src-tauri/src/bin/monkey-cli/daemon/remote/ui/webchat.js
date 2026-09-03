// The whole client. It knows three things: how to get a visitor identifier the
// daemon minted, how to post a message, and how to read the conversation back.
// Every message is untrusted text, so it only ever reaches the DOM through
// textContent — never as markup.
const account = document.location.pathname.split('/').filter(Boolean)[1] || '';
const storageKey = `webchat:${account}`;
const transcript = document.getElementById('transcript');
const status = document.getElementById('status');
const form = document.getElementById('composer');
const input = document.getElementById('text');
// 5 seconds while the tab is visible, a minute when it is not: the server's own
// window is 60 requests a minute per visitor, and two open tabs must both fit.
const POLL_MS = 5000;
const HIDDEN_POLL_MS = 60000;

let visitor = null;
let rendered = '';

function say(text) {
  status.textContent = text;
}

function render(messages) {
  const key = JSON.stringify(messages);
  if (key === rendered) return;
  rendered = key;
  transcript.replaceChildren();
  for (const message of messages) {
    const row = document.createElement('div');
    row.className = message.outbound ? 'msg' : 'msg mine';
    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = message.outbound ? 'Little Monkey' : 'You';
    row.appendChild(who);
    const body = document.createElement('span');
    body.textContent = message.text;
    row.appendChild(body);
    transcript.appendChild(row);
  }
  transcript.scrollTop = transcript.scrollHeight;
}

async function session() {
  const stored = window.localStorage.getItem(storageKey);
  if (stored) return stored;
  const response = await fetch(`/webchat/${account}/session`, { method: 'POST' });
  if (!response.ok) throw new Error(`session ${response.status}`);
  const payload = await response.json();
  window.localStorage.setItem(storageKey, payload.visitor_id);
  return payload.visitor_id;
}

async function poll() {
  if (!visitor) return;
  const response = await fetch(`/webchat/${account}/messages`, {
    headers: { 'x-webchat-visitor': visitor },
  });
  if (response.status === 401) {
    window.localStorage.removeItem(storageKey);
    say('This browser is no longer recognised. Reload to start again.');
    visitor = null;
    return;
  }
  if (!response.ok) return;
  const payload = await response.json();
  render(payload.messages || []);
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const text = input.value.trim();
  if (!text || !visitor) return;
  input.value = '';
  const response = await fetch(`/webchat/${account}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ visitor_id: visitor, text }),
  });
  if (!response.ok) {
    say(`That message was not accepted (${response.status}).`);
    input.value = text;
    return;
  }
  // The transcript only carries messages the daemon accepted for a run, and a
  // first message waits on pairing, so the local echo is what keeps a visitor
  // from thinking their message vanished.
  say('Sent.');
  const echo = document.createElement('div');
  echo.className = 'msg mine';
  echo.textContent = text;
  transcript.appendChild(echo);
  transcript.scrollTop = transcript.scrollHeight;
  rendered = '';
  await poll();
});

async function start() {
  try {
    visitor = await session();
  } catch (error) {
    say('This page could not start a session.');
    return;
  }
  say('Ready. A first message is answered with a pairing code to give the operator.');
  const tick = async () => {
    try {
      await poll();
    } catch (error) {
      /* a poll that fails is retried on the next tick */
    }
    window.setTimeout(tick, document.hidden ? HIDDEN_POLL_MS : POLL_MS);
  };
  await tick();
}

start();
