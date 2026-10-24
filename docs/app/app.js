// The hosted pages, in the browser.
//
// One view per route, the routes `src/serve/views.rs` serves, behind a hash
// so GitHub Pages can serve them all from one file. Each view decides what it
// needs the way its handler did — signed in, on the plan, a property that is
// somebody's — and the decisions are the same ones, so the two cannot be told
// apart from the other side of the card.
//
// There is no server behind it. Sign-in is Google's flow for browser apps:
// Google hands an access token straight back to this page, which keeps it in
// sessionStorage — this tab, an hour at most, gone when the tab closes — and
// spends it on Google's own APIs. No client secret, no refresh token, nothing
// kept anywhere else. The subscription is the same Supabase RPCs the CLI
// calls, with the same publishable key.

const CONFIG = window.ANACRAFT || {};

const AUTH_URL = 'https://accounts.google.com/o/oauth2/v2/auth';
const ADMIN_API = 'https://analyticsadmin.googleapis.com/v1beta';
// The same four scopes `craft login` asks for, so the consent screen is the
// one people have seen before.
const SCOPE_READ = 'https://www.googleapis.com/auth/analytics.readonly';
const SCOPE_EDIT = 'https://www.googleapis.com/auth/analytics.edit';
const SCOPE = `openid email ${SCOPE_READ} ${SCOPE_EDIT}`;
const USERINFO_URL = 'https://openidconnect.googleapis.com/v1/userinfo';

const PLAN = { tier: 'elite', label: 'Anacrafter Elite', monthly: '$9.99/month' };
const LEVEL = { basic: 1, pro: 2, elite: 3 };
const CHECKOUT = 'https://buy.stripe.com/3cI7sL4wY98r8caern9MY04';

const DEMO = { id: 'demo', name: 'Contoso Labs (demo)', account: 'Anacraft demo' };
const DEMO_TAG = 'G-DEMO1A2B3C4D';

// The order `src/theme.rs` lists them in, which is the order the chips sit.
const THEMES = ['osaka-jade', 'catppuccin', 'github', 'tokyo-night', 'solarized-dark',
  'solarized-light', 'catppuccin-latte'];
const DEFAULT_PAL = 'osaka-jade';

/// How many properties a lookup for an existing stream reads, as
/// `configure.rs` does.
const SCAN_LIMIT = 40;
/// How long a started sign-in may take before its state is stale.
const PENDING_MS = 10 * 60 * 1000;

// ------------------------------------------------------------ storage ---

function stored(store, key) {
  try { return JSON.parse(store.getItem(key) || 'null'); } catch (e) { return null; }
}

function keep(store, key, value) {
  try {
    if (value === null) store.removeItem(key); else store.setItem(key, JSON.stringify(value));
  } catch (e) { /* a private window: this run still works, it just forgets */ }
}

/// The sign-in, while its token is good.
function session() {
  const s = stored(sessionStorage, 'anacraft.session');
  if (!s || !s.token || s.expires <= Date.now() + 30 * 1000) return null;
  return s;
}

// ---------------------------------------------------------------- DOM ---

/// An element. Strings become text nodes, never markup: property names are
/// somebody's typing.
function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs || {})) {
    if (value === false || value === null || value === undefined) continue;
    if (name.startsWith('on')) el.addEventListener(name.slice(2), value);
    else el.setAttribute(name, value === true ? '' : value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

const svg = (markup) => {
  const span = document.createElement('span');
  span.innerHTML = markup; // constant drawings only, never data
  return span.firstElementChild;
};

const COPY_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="5" y="5" width="9" height="9" rx="1.5"/>' +
  '<path d="M11 5V3.5A1.5 1.5 0 0 0 9.5 2h-6A1.5 1.5 0 0 0 2 3.5v6A1.5 1.5 0 0 0 3.5 11H5"/></svg>';
const TRASH_ICON = '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" ' +
  'stroke-width="1.5" stroke-linecap="square" aria-hidden="true">' +
  '<path d="M2.5 4h11M6 4V2.5h4V4M4 4l.7 9.5h6.6L12 4M6.8 6.5v4.5M9.2 6.5v4.5" /></svg>';
const GOOGLE_ICON = '<svg viewBox="0 0 18 18" aria-hidden="true">' +
  '<path fill="#4285F4" d="M17.64 9.2c0-.64-.06-1.25-.16-1.84H9v3.48h4.84a4.14 4.14 0 0 1-1.8 2.72v2.26h2.92c1.7-1.57 2.68-3.88 2.68-6.62z" />' +
  '<path fill="#34A853" d="M9 18c2.43 0 4.47-.8 5.96-2.18l-2.92-2.26c-.8.54-1.84.86-3.04.86-2.34 0-4.32-1.58-5.03-3.7H.96v2.33A9 9 0 0 0 9 18z" />' +
  '<path fill="#FBBC05" d="M3.97 10.72a5.4 5.4 0 0 1 0-3.44V4.95H.96a9 9 0 0 0 0 8.1l3.01-2.33z" />' +
  '<path fill="#EA4335" d="M9 3.58c1.32 0 2.5.45 3.44 1.35l2.58-2.58C13.46.9 11.43 0 9 0A9 9 0 0 0 .96 4.95l3.01 2.33C4.68 5.16 6.66 3.58 9 3.58z" />' +
  '</svg>';

/// Put a view in the card: its state for the dot, its title, its middle.
function show(state, title, ...children) {
  document.getElementById('card').setAttribute('data-state', state);
  document.title = title ? `anacraft — ${title}` : 'anacraft';
  const pane = document.getElementById('pane');
  pane.replaceChildren(...children.flat().filter(Boolean));
  const focus = pane.querySelector('[autofocus]');
  if (focus) focus.focus();
}

function who(email, tier) {
  document.getElementById('who').textContent =
    email ? (tier ? `${email} · ${tier}` : email) : '';
}

function working(what) {
  show('boot', null, h('div', { class: 'work' }, what));
}

/// A refusal with a way out of it — the error pane, with the sentence whole.
function fault(message, back) {
  show('err', 'that did not work',
    h('div', { class: 'err' }, h('b', {}, 'that did not work'), h('span', {}, message)),
    h('div', { class: 'acts' }, h('a', { class: 'go', href: back || '#/', style: 'margin-top:0' }, 'Try again')));
}

/// A button that copies the text of the element it names.
function copier(source, label, cls) {
  const icon = cls === 'ico';
  const button = h('button', {
    class: cls, type: 'button',
    title: icon ? label : null, 'aria-label': icon ? label : null,
  }, icon ? svg(COPY_ICON) : label);
  button.addEventListener('click', () => {
    navigator.clipboard.writeText(source.textContent).then(() => {
      button.classList.add('done');
      if (icon) button.title = 'Copied'; else button.textContent = 'Copied';
      setTimeout(() => {
        button.classList.remove('done');
        if (icon) button.title = label; else button.textContent = label;
      }, 1800);
    }).catch(() => {
      // No clipboard permission, so select it instead and let the keyboard
      // do what the button could not.
      const range = document.createRange();
      range.selectNodeContents(source);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
    });
  });
  return button;
}

function palettes() {
  const current = document.documentElement.getAttribute('data-pal') || DEFAULT_PAL;
  document.getElementById('pals').replaceChildren(...THEMES.map((name) =>
    h('button', {
      class: 'pal', type: 'button', 'data-pal': name, title: name, 'aria-label': name,
      'aria-pressed': String(name === current),
      onclick: () => {
        if (name === DEFAULT_PAL) document.documentElement.removeAttribute('data-pal');
        else document.documentElement.setAttribute('data-pal', name);
        try { localStorage.setItem('palette', name); } catch (e) { /* this tab only */ }
        palettes();
      },
    })));
}

// --------------------------------------------------------------- bits ---

const enc = encodeURIComponent;

function base64url(bytes) {
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function nonce(bytes) {
  return base64url(crypto.getRandomValues(new Uint8Array(bytes)));
}

/// `license::mint_token`: forty letters and digits.
function mintToken() {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  return Array.from(crypto.getRandomValues(new Uint8Array(40)), (b) => alphabet[b % 62]).join('');
}

/// `configure::host_of`: the bare host out of whatever somebody typed.
function hostOf(raw) {
  const trimmed = String(raw || '').trim().replace(/\.+$/, '');
  const rest = trimmed.includes('://') ? trimmed.split('://').slice(1).join('://') : trimmed;
  const host = rest.split(/[/?#]/)[0].trim().toLowerCase();
  if (!host) throw new Error('which domain? e.g. example.com');
  if (host.includes('@')) throw new Error(`${host} looks like an email address, not a domain`);
  if (host.includes(':')) throw new Error(`${host} names a port; Analytics measures an origin — drop the port`);
  if (!host.includes('.')) throw new Error(`${host} is not a domain — it needs a dot, e.g. example.com`);
  if (/[^a-z0-9.-]/.test(host)) throw new Error(`${host} is not a domain anacraft can read — pass it as example.com`);
  return host;
}

/// `configure::is_iana`: shaped like `Area/City`, which is all that can be
/// checked without the tz database. Google rejects anything else, and says so.
function isIana(tz) {
  const [area, city] = String(tz || '').split('/');
  return Boolean(area && city && /^[A-Za-z0-9/_+-]+$/.test(tz));
}

function bare(id) {
  return String(id || '').trim().replace(/^properties\//, '');
}

// ------------------------------------------------------------- Google ---

/// `ga::explain`, in the page's voice: a sentence for each status that has
/// a way out, and Google's own words after it.
function explain(status, body) {
  let detail;
  try { detail = JSON.parse(body).error.message; } catch (e) { detail = String(body).slice(0, 300); }
  switch (true) {
    case status === 401: return `your Google sign-in has expired — sign in again (${detail})`;
    case status === 403 && /has not been used|is disabled/.test(detail):
      return `an API isn't enabled on the Google Cloud project behind this page. (${detail})`;
    case status === 403 && /insufficient authentication scopes/.test(detail):
      return 'this sign-in has not granted permission to change your Analytics setup — sign out, ' +
        `sign in again and leave both Analytics boxes ticked. (${detail})`;
    case status === 403:
      return 'access denied — the signed-in account needs at least Viewer on this property, ' +
        `or Editor on the account to create one. (${detail})`;
    case status === 429: return `Google rate-limited this request; try again shortly. (${detail})`;
    default: return `Google Analytics API error ${status}: ${detail}`;
  }
}

class Expired extends Error {}

async function google(method, url, body) {
  const s = session();
  if (!s) throw new Expired('signed out');
  const res = await fetch(url, {
    method,
    headers: { authorization: `Bearer ${s.token}`, ...(body ? { 'content-type': 'application/json' } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  if (res.status === 401) {
    keep(sessionStorage, 'anacraft.session', null);
    throw new Expired(explain(401, text));
  }
  if (!res.ok) throw new Error(explain(res.status, text));
  return text ? JSON.parse(text) : {};
}

/// Every page of a list, joined.
async function pages(url, field) {
  const out = [];
  let token = '';
  do {
    const page = await google('GET', `${url}${url.includes('?') ? '&' : '?'}pageSize=200${token ? `&pageToken=${enc(token)}` : ''}`);
    out.push(...(page[field] || []));
    token = page.nextPageToken || '';
  } while (token);
  return out;
}

const ga = {
  /// Every property the signed-in account can read.
  async properties() {
    const summaries = await pages(`${ADMIN_API}/accountSummaries`, 'accountSummaries');
    return summaries.flatMap((account) => (account.propertySummaries || []).map((p) => ({
      id: bare(p.property), name: p.displayName || '', account: account.displayName || '',
    })));
  },
  async accounts() {
    return (await pages(`${ADMIN_API}/accounts`, 'accounts'))
      .map((a) => ({ id: a.name.replace(/^accounts\//, ''), name: a.displayName || '' }));
  },
  /// The web streams on a property. App streams carry no measurement id.
  async webStreams(property) {
    const page = await google('GET', `${ADMIN_API}/properties/${property}/dataStreams?pageSize=200`);
    return (page.dataStreams || []).filter((s) => s.webStreamData).map((s) => ({
      name: s.name, displayName: s.displayName || '',
      measurementId: s.webStreamData.measurementId || '', defaultUri: s.webStreamData.defaultUri || '',
    }));
  },
  async createProperty(account, displayName, timeZone) {
    const made = await google('POST', `${ADMIN_API}/properties`, {
      parent: `accounts/${account.id}`, displayName, timeZone, currencyCode: 'USD',
    });
    return { id: bare(made.name), name: made.displayName, account: account.name };
  },
  async createWebStream(property, displayName, defaultUri) {
    const made = await google('POST', `${ADMIN_API}/properties/${property}/dataStreams`, {
      type: 'WEB_DATA_STREAM', displayName, webStreamData: { defaultUri },
    });
    if (!made.webStreamData) {
      throw new Error('Google created the stream but returned no web stream data, so there is no ' +
        'measurement id to show — check the property in the Analytics console');
    }
    return { measurementId: made.webStreamData.measurementId, defaultUri: made.webStreamData.defaultUri };
  },
  /// Into the Analytics trash, restorable from the console for 35 days.
  async deleteProperty(property) {
    await google('DELETE', `${ADMIN_API}/properties/${property}`);
  },
};

// ------------------------------------------------------- subscription ---

async function rpc(name, body) {
  const res = await fetch(`${CONFIG.supabaseUrl.replace(/\/$/, '')}/rest/v1/rpc/${name}`, {
    method: 'POST',
    headers: {
      apikey: CONFIG.supabaseKey, authorization: `Bearer ${CONFIG.supabaseKey}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`subscription service returned ${res.status}: ${text}`);
  return text ? JSON.parse(text) : null;
}

/// `Status::tier`: the plan a live subscription is on, and none for one
/// that is not — asked every time, the way a hosted server did.
async function tierOf(s, token) {
  const rows = await rpc('subscription_status', {
    p_user_id: s.sub, p_token: token || null, p_email: s.email || null,
  });
  const row = Array.isArray(rows) ? rows[0] : rows;
  if (!row) return null;
  const active = typeof row.subscribed === 'boolean'
    ? row.subscribed : ['active', 'trialing'].includes(row.status);
  if (!active) return null;
  return LEVEL[row.tier] ? row.tier : 'basic';
}

const meets = (tier) => Boolean(tier) && LEVEL[tier] >= LEVEL[PLAN.tier];

/// Where somebody stands: signed out, on the plan, or not yet.
async function stand() {
  const s = session();
  if (!s) return { out: true };
  let tier;
  try { tier = await tierOf(s); } catch (e) { throw new Error(`could not check the subscription: ${e.message}`); }
  who(s.email, tier);
  return { s, tier, paid: meets(tier) };
}

// ------------------------------------------------------------ sign-in ---

/// Off to Google, with a state kept in this tab so the way back can tell its
/// own round trip from one somebody else started.
function begin(next) {
  const state = nonce(24);
  keep(sessionStorage, 'anacraft.pending', { state, next: next || '#/', born: Date.now() });
  const params = new URLSearchParams({
    client_id: CONFIG.clientId,
    redirect_uri: `${location.origin}/app/`,
    response_type: 'token',
    scope: SCOPE,
    state,
    prompt: 'select_account',
    include_granted_scopes: 'true',
  });
  location.assign(`${AUTH_URL}?${params}`);
}

/// Google's redirect back, with the token in the fragment.
async function callback(back) {
  const pending = stored(sessionStorage, 'anacraft.pending');
  keep(sessionStorage, 'anacraft.pending', null);
  // A token has no business in the address bar or the history.
  history.replaceState(null, '', `${location.pathname}`);

  if (back.get('error')) return signin('the sign-in was cancelled on Google\'s side.');
  const fresh = pending && pending.state === back.get('state') && Date.now() - pending.born < PENDING_MS;
  if (!fresh || !back.get('access_token')) {
    return signin('that sign-in had expired or was started in another browser — try again.');
  }
  const granted = (back.get('scope') || '').split(' ');
  if (!granted.includes(SCOPE_READ) || !granted.includes(SCOPE_EDIT)) {
    return signin('anacraft needs both Analytics boxes ticked on Google\'s screen — one to read the ' +
      'numbers and one to create the property. Try again and leave both on.');
  }

  working('signing in');
  const token = back.get('access_token');
  const res = await fetch(USERINFO_URL, { headers: { authorization: `Bearer ${token}` } }).catch(() => null);
  const account = res && res.ok ? await res.json() : null;
  if (!account || !account.sub) return signin('Google did not say which account signed in.');

  keep(sessionStorage, 'anacraft.session', {
    token,
    expires: Date.now() + (Number(back.get('expires_in')) || 3600) * 1000,
    sub: account.sub,
    email: account.email || null,
  });
  // `craft login`'s courtesy: register the account so a subscription bought
  // anywhere finds it here. Best-effort, as it is there.
  await rpc('link_account', { p_user_id: account.sub, p_email: account.email || null }).catch(() => undefined);

  const next = pending.next || '#/';
  if (location.hash === next) route(); else location.hash = next;
}

function signin(failure) {
  who(null);
  show('out', 'sign in with Google',
    h('h2', {}, 'Sign in with Google'),
    h('p', {}, 'Google asks for two Analytics permissions: one to read your numbers, one to ' +
      'create the property for your site. Leave both ticked.'),
    h('button', { class: 'go', type: 'button', onclick: () => begin(nextAfterSignin()) },
      svg(GOOGLE_ICON), 'Sign in with Google'),
    h('p', { class: 'fine' }, failure ? `last attempt: ${failure}` :
      'Your Google sign-in is held in this tab only — closing it, or an hour, forgets it.'));
}

/// Where a sign-in started from a signed-out view should land: that view.
function nextAfterSignin() {
  const here = location.hash || '#/';
  return here.startsWith('#/signin') ? '#/' : here;
}

function signout() {
  keep(sessionStorage, 'anacraft.session', null);
  who(null);
  location.hash = '#/signin';
}

// -------------------------------------------------------------- views ---

async function landing() {
  const where = await stand();
  location.replace(where.out ? '#/signin' : '#/properties');
}

function row(property, links) {
  return h('div', { class: 'row' },
    h('a', { class: 'choose', href: `#/properties/${property.id}/streams?n=${enc(property.name)}` },
      h('span', { class: 'nm' }, property.name),
      h('span', { class: 'id' }, property.id),
      h('span', { class: 'ac' }, property.account)),
    h('a', { class: 'bin mcp', href: `#/connect/${property.id}?n=${enc(property.name)}`,
      title: links.demo ? 'Copy the MCP connector for the demo' : 'Copy the MCP connector for this property' }, 'mcp'),
    links.demo ? null : h('a', {
      class: 'bin trash', href: `#/properties/${property.id}/trash?n=${enc(property.name)}`,
      title: 'Move this property to the Analytics trash',
      'aria-label': `Move ${property.name} to the Analytics trash`,
    }, svg(TRASH_ICON)));
}

async function properties(where) {
  // The demo is free to look at, so somebody on no plan still gets the list
  // — with the demo on it and nothing of theirs, since reading their account
  // is what the plan buys.
  let found = [];
  if (where.paid) {
    working('reading your properties');
    found = await ga.properties();
  }
  show('pick', 'pick this site\'s property',
    h('h2', {}, 'Pick this site\'s property'),
    h('p', {}, 'One property per site, so each site\'s numbers are its own. A site rebuilt a few ' +
      'times leaves a few behind — the bin on a row throws one into the Analytics trash, where ' +
      'Google keeps it restorable for 35 days.'),
    h('div', { class: 'list' },
      h('div', { class: 'row make' },
        h('a', { class: 'choose', href: where.paid ? '#/properties/new' : '#/unlock' },
          h('span', { class: 'nm' }, '+  Create a new property'))),
      found.map((p) => row(p, {})),
      row(DEMO, { demo: true }),
      !where.paid
        ? h('div', { class: 'empty' }, 'your own properties show here once you subscribe — the demo is free to look at')
        : found.length === 0
          ? h('div', { class: 'empty' }, 'no GA4 properties on this account yet — make the first one above')
          : null),
    h('p', { class: 'fine' }, h('b', {}, 'mcp'), ' on a row hands you the line that wires an AI ' +
      'assistant to that property — one connector per site, each reading only its own.'),
    h('div', { class: 'acts' }, h('button', { class: 'lnk', type: 'button', onclick: signout }, 'sign out')));
}

/// The form both "a new property" and "this property has no stream" are.
function siteForm({ heading, body, submit, back, onsite }) {
  const input = h('input', {
    id: 'url', name: 'url', type: 'url', inputmode: 'url', autofocus: true, required: true,
    placeholder: 'https://myapp.lovable.app',
  });
  const form = h('form', {
    onsubmit: (event) => {
      event.preventDefault();
      let host;
      try { host = hostOf(input.value); } catch (e) { return fault(e.message, location.hash); }
      run(() => onsite(host));
    },
  },
  h('label', { for: 'url' }, 'Your site\'s URL'),
  input,
  h('div', { class: 'acts' },
    h('button', { class: 'primary', type: 'submit', style: 'margin-top:0' }, submit),
    h('a', { class: 'lnk', href: back }, 'back to the list')));
  show('pick', heading, h('h2', {}, heading), h('p', {}, body), form,
    h('p', { class: 'fine' }, 'Reused if this site already has a property — the same check ',
      h('code', {}, 'craft configure'), ' makes before it creates anything.'));
}

const tagHref = (mid, property, name) => `#/tag/${mid}?p=${enc(property)}&n=${enc(name)}`;

function newProperty(where) {
  if (!where.paid) return location.replace('#/unlock');
  siteForm({
    heading: 'A new property',
    body: 'anacraft creates the property and its web data stream, then reads the measurement ID back.',
    submit: 'Create the property',
    back: '#/properties',
    onsite: (host) => setup(host),
  });
}

/// `configure::setup`: reuse what already measures this host, finish a
/// property that never got a stream, or create both.
async function setup(host, account) {
  working(`looking for ${host}`);
  const all = await ga.properties();
  all.sort((a, b) => (a.name.toLowerCase() !== host) - (b.name.toLowerCase() !== host));
  let unfinished = null;
  for (const property of all.slice(0, SCAN_LIMIT)) {
    let streams;
    // A property this login cannot read streams on is not a match, and not
    // a reason to fail.
    try { streams = await ga.webStreams(property.id); } catch (e) { continue; }
    const measured = streams.find((s) => { try { return hostOf(s.defaultUri) === host; } catch (e) { return false; } });
    if (measured) return location.assign(tagHref(measured.measurementId, property.id, property.name));
    if (!unfinished && streams.length === 0 && property.name.toLowerCase() === host) unfinished = property;
  }
  if (unfinished) return finish(unfinished, host);

  const accounts = await ga.accounts();
  if (accounts.length === 0) {
    throw new Error('this Google account has no Analytics account to create a property in. Create one at ' +
      'https://analytics.google.com/analytics/web/#/provision (accepting the terms has to happen ' +
      'there), then try again.');
  }
  if (!account && accounts.length > 1) return pickAccount(host, accounts);
  const into = account || accounts[0];

  working(`creating a property for ${host}`);
  let tz = '';
  try { tz = Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch (e) { /* UTC below */ }
  const property = await ga.createProperty(into, host, isIana(tz) ? tz : 'UTC');
  return finish(property, host);
}

/// The web stream that makes a property measurable. A failure leaves the
/// property behind, and trying again finishes it rather than making another.
async function finish(property, host) {
  working(`adding the web stream for ${host}`);
  let stream;
  try {
    stream = await ga.createWebStream(property.id, host, `https://${host}`);
  } catch (e) {
    if (e instanceof Expired) throw e;
    throw new Error(`property ${property.id} was created, but adding its web stream failed — ` +
      `try again to finish it: ${e.message}`);
  }
  location.assign(tagHref(stream.measurementId, property.id, property.name || host));
}

/// Several Analytics accounts: picking one for somebody would put the
/// property, its retention and who can see it somewhere they did not choose.
function pickAccount(host, accounts) {
  show('pick', 'pick the account',
    h('h2', {}, 'Which Analytics account?'),
    h('p', {}, `This login can see ${accounts.length} Analytics accounts. Pick the one ${host} belongs in.`),
    h('div', { class: 'list' }, accounts.map((account) => h('div', { class: 'row' },
      h('a', {
        class: 'choose', href: '#', onclick: (event) => { event.preventDefault(); run(() => setup(host, account)); },
      }, h('span', { class: 'nm' }, account.name), h('span', { class: 'id' }, account.id))))),
    h('div', { class: 'acts' }, h('a', { class: 'lnk', href: '#/properties' }, 'back to the list')));
}

async function streams(where, id, query) {
  const property = bare(id);
  if (property === DEMO.id) return location.replace(tagHref(DEMO_TAG, DEMO.id, DEMO.name));
  if (!where.paid) return location.replace('#/unlock');
  const named = query.get('n') || '';

  working('reading its data streams');
  const found = await ga.webStreams(property);
  if (found.length === 1) return location.replace(tagHref(found[0].measurementId, property, named));
  if (found.length === 0) {
    return siteForm({
      heading: 'This property measures no website yet',
      body: `“${named || `property ${property}`}” has no web data stream, so there is no measurement ` +
        'ID on it. Adding one mints it.',
      submit: 'Add the web stream',
      back: '#/properties',
      onsite: async (host) => {
        working(`adding the web stream for ${host}`);
        const stream = await ga.createWebStream(property, host, `https://${host}`);
        location.assign(tagHref(stream.measurementId, property, host));
      },
    });
  }
  show('pick', 'pick this site\'s data stream',
    h('h2', {}, 'More than one website here'),
    h('p', {}, 'Each web data stream has its own measurement ID. Pick this site\'s.'),
    h('div', { class: 'list' }, found.map((s) => h('div', { class: 'row' },
      h('a', { class: 'choose', href: tagHref(s.measurementId, property, named) },
        // The URL says more than the name does when there are several.
        h('span', { class: 'nm' }, s.defaultUri || s.displayName),
        h('span', { class: 'ac' }, s.measurementId))))),
    h('div', { class: 'acts' }, h('a', { class: 'lnk', href: '#/properties' }, 'back to the properties')));
}

function trash(where, id, query) {
  const property = bare(id);
  const name = query.get('n') || `property ${property}`;
  if (property === DEMO.id) {
    return fault('the demo is synthetic and in nobody\'s Analytics account, so there is nothing to ' +
      'throw away.', '#/properties');
  }
  show('pick', 'trash this property?',
    h('h2', {}, `Move “${name}” to the trash?`),
    h('p', {}, 'It stops collecting straight away. Google keeps it restorable from the Analytics ' +
      'console for 35 days, and nothing here can shorten that.'),
    h('div', { class: 'list' }, h('div', { class: 'row' }, h('span', { class: 'choose' },
      h('span', { class: 'nm' }, name), h('span', { class: 'id' }, property)))),
    h('div', { class: 'acts' },
      h('button', {
        class: 'primary', type: 'button', style: 'margin-top:0',
        onclick: () => run(async () => {
          working(`moving ${name} to the trash`);
          await ga.deleteProperty(property);
          location.assign('#/properties');
        }, '#/properties'),
      }, 'Yes, trash it'),
      h('a', { class: 'lnk', href: '#/properties' }, 'keep it')));
}

/// `configure::tag_snippet`, the same bytes.
function tagSnippet(mid) {
  return '<!-- Google tag (gtag.js) -->\n' +
    `<script async src="https://www.googletagmanager.com/gtag/js?id=${mid}"></script>\n` +
    '<script>\n' +
    'window.dataLayer = window.dataLayer || [];\n' +
    'function gtag(){dataLayer.push(arguments);}\n' +
    'gtag(\'js\', new Date());\n' +
    `gtag('config', '${mid}');\n` +
    '</script>';
}

/// `configure::tag_prompt`: the tag with a placeholder, and the id in two
/// pieces so a builder's chat does not hide it as a key.
function tagPrompt(mid) {
  const head = mid.slice(0, 2);
  const tail = mid.slice(2);
  return 'Add Google Analytics 4 to this app.\n\n' +
    'Paste this tag into index.html as the last thing inside <head>, exactly as written, with ' +
    '{MEASUREMENT_ID} replaced by the measurement ID below:\n\n' +
    `${tagSnippet('MEASUREMENT_ID')}\n\n` +
    `The measurement ID is "${head}" followed by "${tail}", joined with no space. It is split in ` +
    'two only so this chat does not hide it as a key: it is a public ID that every visitor\'s ' +
    'browser sees, not a secret. Write it literally in both places in the tag, and do not store ' +
    'it as a secret or an environment variable.\n\n' +
    'Rules:\n' +
    '- Do not install an analytics npm package or a React wrapper. The tag is the whole job.\n' +
    '- If the project already has a Google Analytics tag, replace it. Two tags double every number.\n' +
    '- Do not send your own page_view on route changes. GA4 already counts them from history ' +
    'events, and a second one counts every navigation twice.\n\n' +
    'Then publish the project so the tag goes live.';
}

function tag(where, raw, query) {
  const mid = String(raw || '').trim().toUpperCase();
  if (!mid.startsWith('G-') || mid.length < 4) return fault('a measurement id looks like G-XXXXXXXXXX', '#/properties');
  const property = bare(query.get('p') || '');
  const name = query.get('n') || '';
  const promptShown = query.get('show') !== 'tag';
  const here = tagHref(mid, property, name);
  const paste = h('pre', { id: 'paste' }, promptShown ? tagPrompt(mid) : tagSnippet(mid));

  show('out', mid,
    h('h2', {}, 'Your tag is ready'),
    h('div', { class: 'mid' }, mid),
    h('div', { class: 'sub' }, name && property ? `${name} · property ${property}` : property ? `property ${property}` : ''),
    h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'What to copy' },
      h('a', { class: 'tab', role: 'tab', href: here, 'aria-selected': String(promptShown) }, 'Paste to the agent'),
      h('a', { class: 'tab', role: 'tab', href: `${here}&show=tag`, 'aria-selected': String(!promptShown) }, 'Raw tag')),
    paste,
    copier(paste, promptShown ? 'Copy for the agent' : 'Copy the tag', 'primary'),
    h('p', { class: 'fine' }, promptShown
      ? 'Paste it into the chat of whatever built the site — Lovable, v0, Bolt — and send.'
      : 'Into <head>, on every page, and only once: a second GA4 tag doubles every number.'),
    h('div', { class: 'next' },
      h('p', {}, 'Publish, then ask Claude or ChatGPT how it is doing — ', h('b', {}, 'wire up an AI tool'),
        ' below hands you the link to paste into it.'),
      h('div', { class: 'acts' },
        h('a', { class: 'lnk', href: '#/properties' }, 'pick a different property'),
        h('a', { class: 'lnk', href: property ? `#/connect/${property}?n=${enc(name)}` : '#/properties' },
          'wire up an AI tool'))));
}

/// How an assistant reads this property: `craft mcp`, on the machine the
/// assistant runs on. The commands are written out with the property in
/// them, so they can be pasted as they are.
function connect(where, id, query) {
  const property = bare(id);
  if (!property) return location.replace('#/properties');
  const demo = property === DEMO.id;
  const name = query.get('n') || '';
  const reads = demo
    ? `Reads ${DEMO.name} — synthetic numbers, on the same tools as the real thing.`
    : name ? `Reads ${name} — property ${property}.` : `Reads property ${property}.`;

  const steps = h('pre', { id: 'steps' }, [
    'curl -fsSL https://anacraft.dev/install.sh | bash',
    ...(demo ? ['craft mcp --install --demo'] : ['craft login', `craft use ${property}`, 'craft mcp --install']),
  ].join('\n'));
  const code = h('pre', { id: 'code' }, 'claude mcp add anacraft -- craft mcp' + (demo ? ' --demo' : ''));

  show('out', 'wire an AI tool to this property',
    h('h2', {}, 'Wire an AI tool to this property'),
    h('p', {}, `${reads} An assistant gets the twelve tools `, h('code', {}, 'craft mcp'),
      ' serves, run on your own machine: your Google sign-in stays there, and the numbers go from ' +
      'Google to the assistant without passing anybody else.'),
    h('p', {}, 'For ', h('b', {}, 'Claude Desktop'), ', in a terminal:'),
    h('div', { class: 'copyable' }, steps, copier(steps, 'Copy the commands', 'ico')),
    copier(steps, 'Copy the commands', 'primary'),
    h('p', { class: 'fine' }, 'Restart Claude Desktop afterwards and ask it how the site is doing. ',
      h('code', {}, 'craft mcp --uninstall'), ' takes it back out.'),
    h('div', { class: 'next' },
      h('p', {}, 'For ', h('b', {}, 'Claude Code'), ', Cursor or any client that runs a command:'),
      h('div', { class: 'copyable' }, code, copier(code, 'Copy the command', 'ico'))),
    h('div', { class: 'acts' },
      h('a', { class: 'lnk', href: '#/properties' }, 'pick a different property'),
      h('a', { class: 'lnk', href: 'https://anacraft.dev/mcp.html' }, 'what the tools do')));
}

/// `license::checkout_url`: the Payment Link, carrying the token and the
/// signed-in email so the Stripe customer matches the Google account.
function checkoutUrl(token, email) {
  let url = `${CHECKOUT}${CHECKOUT.includes('?') ? '&' : '?'}client_reference_id=${token}`;
  if (email) url += `&prefilled_email=${enc(email)}`;
  return url;
}

function unlock(where) {
  if (where.paid) return location.replace('#/');
  // `tier` rather than paid: somebody on a smaller plan is being asked to
  // move up, which is a different sentence and a different button.
  const subscribed = Boolean(where.tier);
  show('pay', 'unlock the tag',
    h('h2', {}, subscribed ? 'This needs the Elite plan' : 'Unlock the tag'),
    h('p', {}, subscribed
      ? 'Your subscription is live, but the API is on Anacrafter Elite — the same plan craft mcp is ' +
        'on. Moving up is billed going forward, not charged again.'
      : 'The API is part of Anacrafter Elite — the same plan craft mcp is on, so one subscription ' +
        'covers both the tag and the assistant that reads the numbers afterwards.'),
    h('div', { class: 'price' }, h('span', { class: 'fig' }, PLAN.monthly),
      h('span', { class: 'per' }, 'per month · cancel any time')),
    h('button', {
      class: 'primary', type: 'button',
      onclick: () => {
        // Stripe opens from the click itself, so no popup blocker stands in
        // the way; the claim that ties it to this account races it, and a
        // claim that loses still leaves the checkout claimable by its email.
        const token = mintToken();
        window.open(checkoutUrl(token, where.s.email), '_blank', 'noopener');
        rpc('claim_checkout', { p_token: token, p_user_id: where.s.sub, p_email: where.s.email || null })
          .catch(() => undefined);
        location.assign(`#/unlock/checkout?t=${token}`);
      },
    }, subscribed ? 'Move up to Elite →' : 'Continue to checkout →'),
    h('p', { class: 'fine' }, 'Stripe opens in a new tab; this page waits for it and takes you on by ' +
      'itself. Already subscribed? ', h('a', { class: 'lnk', href: '#/' }, 'check again')),
    h('div', { class: 'acts' },
      // Somebody who signed in with the wrong Google account otherwise has
      // two ways forward, and both of them are a credit card.
      h('span', { class: 'fine', style: 'margin:0' }, where.s.email ? `signed in as ${where.s.email}` : ''),
      h('button', { class: 'lnk', type: 'button', onclick: signout }, 'sign out and use a different account')));
}

let polling = 0;

function checkout(where, query) {
  const token = query.get('t') || '';
  if (!/^[A-Za-z0-9]{1,64}$/.test(token)) return location.replace('#/unlock');
  if (where.paid) return location.replace('#/');
  show('pay', 'waiting for Stripe',
    h('h2', {}, 'Finish in the Stripe tab'),
    h('p', {}, 'The checkout is already tied to this account, so the payment lands on the row every ',
      h('code', {}, 'craft'), ' command reads afterwards.'),
    h('a', { class: 'primary', href: checkoutUrl(token, where.s.email), target: '_blank', rel: 'noopener' },
      'Reopen the checkout →'),
    h('div', { class: 'work' }, 'waiting for Stripe'),
    h('p', { class: 'fine' }, 'This page notices by itself and takes you on. Nothing was charged until ' +
      'the Stripe tab says so.'),
    h('div', { class: 'acts' }, h('a', { class: 'lnk', href: '#/unlock' }, 'back')));

  const mine = ++polling;
  const ask = async () => {
    if (mine !== polling || !location.hash.startsWith('#/unlock/checkout')) return;
    try {
      if (meets(await tierOf(where.s, token))) return location.assign('#/');
    } catch (e) { /* the next ask will try again */ }
    setTimeout(ask, 2000);
  };
  setTimeout(ask, 2000);
}

// ------------------------------------------------------------- router ---

/// Run something that talks to Google, and turn whatever goes wrong into the
/// error pane — or the sign-in, when it was the sign-in that ran out.
async function run(work, back) {
  try {
    await work();
  } catch (e) {
    if (e instanceof Expired) return signin(e.message === 'signed out' ? null : e.message);
    fault(e.message || String(e), back || location.hash || '#/');
  }
}

const ROUTES = [
  [/^\/?$/, () => landing(), false],
  [/^\/signin$/, () => (session() ? location.replace('#/') : signin()), false],
  [/^\/properties$/, (w) => properties(w), true],
  [/^\/properties\/new$/, (w) => newProperty(w), true],
  [/^\/properties\/([^/]+)\/streams$/, (w, m, q) => streams(w, m[1], q), true],
  [/^\/properties\/([^/]+)\/trash$/, (w, m, q) => trash(w, m[1], q), true],
  [/^\/tag\/([^/]+)$/, (w, m, q) => tag(w, m[1], q), true],
  [/^\/connect\/([^/]+)$/, (w, m, q) => connect(w, m[1], q), false],
  [/^\/unlock$/, (w) => unlock(w), true],
  [/^\/unlock\/checkout$/, (w, m, q) => checkout(w, q), true],
];

async function route() {
  const hash = location.hash.replace(/^#/, '') || '/';
  const [path, search] = hash.split('?');
  const query = new URLSearchParams(search || '');
  for (const [pattern, view, needsSignin] of ROUTES) {
    const match = path.match(pattern);
    if (!match) continue;
    return run(async () => {
      let where = {};
      if (needsSignin) {
        working('loading');
        where = await stand();
        if (where.out) return signin();
      }
      await view(where, match, query);
    });
  }
  location.replace('#/');
}

function boot() {
  palettes();
  if (!CONFIG.supabaseUrl || !CONFIG.supabaseKey || !CONFIG.clientId) {
    return fault('this page is missing its configuration (config.js), so it cannot sign anybody in.', '#/');
  }
  window.addEventListener('hashchange', route);
  // Google's answer arrives as the fragment, which is otherwise a route.
  const back = new URLSearchParams(location.hash.replace(/^#/, ''));
  if (back.has('state') && (back.has('access_token') || back.has('error'))) return run(() => callback(back));
  route();
}

boot();
