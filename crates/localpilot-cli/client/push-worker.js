// Verified static shell updates automatically. Never caches private responses or replays mutations.
/* global self, caches, fetch, MessageChannel, setTimeout, clearTimeout */
const generation = '8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369';
const cacheName = `locallink-shell-${generation}`;
const shellFiles = [{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/accessibility.js","integrity":"sha256-mT12QZLpfrZ7K1Hj3L9OIG5//DjMjAfeEbx60hi0vQM="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/app.js","integrity":"sha256-3AgdiwyGZQLjvcqBqvkV6VvQ40nQYYUbcgiauu9pMv4="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/companion-view.js","integrity":"sha256-6M9py+7EnIY90S1SLFZv4dcZ3NsjhnjsP9CMwGea4vA="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/compatibility.js","integrity":"sha256-utyfSYFqEeEZZC4M7vqp3Y2ApalA6gIRkDbeoaNf12A="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/device.js","integrity":"sha256-JKlT0mjCU0LRRqXBYMYsuHsYKsIlzJfscyeWiUBVmAA="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/icon-192.png","integrity":"sha256-JeHq97uQLPJKIRrm+I6eZzaXBonOOcl14oOaAZKiXl0="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/icon-512.png","integrity":"sha256-M092CgQsFn+0hCfcLSAOAXqYUwtWDJpLnvBJQlPhVe4="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/icon.svg","integrity":"sha256-TzYAdGS3TipGhrnsdNB4z05bHnqI8CY4W22W5gM2T+E="},{"url":"/index.html","integrity":"sha256-V5sAc8XzAvyT/FcSV/bslGD2UuyV00ZJ8ZrMGQKHa6Q="},{"url":"/manifest.webmanifest","integrity":"sha256-mKhpeuW1FecnVfkiFr/dbivhnyZuj2zRqaIKIhmNyHM="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/notifications.js","integrity":"sha256-CzYQJnG0LGr7N0pjwPYU67T7gLBs74m7CsGRvbSfFf8="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/offline.js","integrity":"sha256-LOWQAiC8RqYikqI3Ez4dkugTXeRGKZLxchAm2r4pzVg="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/pairing.js","integrity":"sha256-TtgzsUixGwiBPj9RGw3CeNnsK56rXnl1WnL+znWXIJQ="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/session-view.js","integrity":"sha256-ti+zh7Tj1l3RfwebCtlqk+1jPb1hWvYFt+/h2060xls="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/session.js","integrity":"sha256-BoWtxfoqeeR42Ttzn8UWO7KA6OT7zi7/7HgIYAxrnqE="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/style.css","integrity":"sha256-yjsv72+rSE8FMhsc/AjLUIGOipnN9Nu90lGXZnyEvus="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/theme.js","integrity":"sha256-9Wjmlsb8zp/+YeUQOU/Py0eZt20JwQg94++dXVbAe4U="},{"url":"/assets/8baebf19789a649fce81af62b1281f64ebcf4f7f61805c69661e2ccc3de54369/update-state.js","integrity":"sha256-qmF5Snmjo523e1LcvWKetdx/AKDTK2M18wcG//qj1HM="}];
const shellPaths = new Set(shellFiles.map((file) => file.url));
self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    try {
      const responses = await Promise.all(shellFiles.map(async (file) => {
        const response = await fetch(file.url, { integrity: file.integrity, credentials: 'omit', cache: 'no-store', redirect: 'error' });
        if (!response.ok) throw new Error('Shell unavailable');
        return [file.url, response];
      }));
      const cache = await caches.open(cacheName);
      await Promise.all(responses.map(([url, response]) => cache.put(url, response)));
      await self.skipWaiting();
    } catch (error) { await caches.delete(cacheName); throw error; }
  })());
});
self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    await self.clients.claim();
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    await Promise.all(windows.map(async (client) => {
      const url = new URL(client.url);
      if (url.origin !== self.location.origin || !['/', '/index.html'].includes(url.pathname)) return;
      const state = await new Promise((resolve) => {
        const channel = new MessageChannel();
        const finish = (state) => { clearTimeout(timer); channel.port1.close(); resolve(state); };
        const timer = setTimeout(() => finish(null), 1500);
        channel.port1.onmessage = ({ data }) => finish(data);
        try { client.postMessage({ type: 'shell-update', generation }, [channel.port2]); }
        catch { finish(null); }
      });
      if (state?.generation !== generation && !state?.defer) {
        // Do not await navigation inside activate: that navigation may itself
        // wait for activation to finish.
        void client.navigate(client.url).catch(() => { /* Tab was closed. */ });
      }
    }));
    for (const name of await caches.keys()) {
      if (name.startsWith('locallink-shell-') && name !== cacheName) await caches.delete(name);
    }
  })());
});
self.addEventListener('fetch', (event) => {
  const { request } = event;
  const url = new URL(request.url);
  if (request.method !== 'GET' || url.origin !== self.location.origin || url.search) return;
  const navigation = request.mode === 'navigate' && request.destination === 'document' && ['/', '/index.html'].includes(url.pathname);
  // HTML is only ever returned to a document navigation, never an API fetch.
  if (!navigation && (url.pathname === '/index.html' || !shellPaths.has(url.pathname))) return;
  event.respondWith((async () => {
    // An online navigation gets current HTML, never a stale cached entry point.
    // Failed/offline navigation can still open the last fully verified shell.
    if (navigation) {
      try {
        const live = await fetch(request, { cache: 'no-store', redirect: 'error' });
        if (live.ok && live.headers.get('content-type')?.includes('text/html')) {
          const html = await live.clone().text();
          const identity = /<meta name="locallink-generation" content="([a-f0-9]{64})">/.exec(html)?.[1];
          if (identity === generation) return live;
          // Do not serve an entry point from an unverified newer build. Install
          // its entire shell first; activation will refresh this window.
          event.waitUntil(self.registration.update().catch(() => {}));
        }
      } catch { /* Offline: use the complete installed generation. */ }
    }
    const cache = await caches.open(cacheName);
    const response = await cache.match(navigation ? '/index.html' : url.pathname);
    // A missing pinned file must not fall back to bytes from another generation.
    if (!response) throw new Error('Offline shell incomplete. Reopen online to repair.');
    return response;
  })());
});
// The PC sends only a kind ({"k":"ask"|"stopped"|"test"}); the visible text stays generic.
// The kind decides where a tap leads: an ask to Attention, a finished turn to its session.
const destinations = { ask: 'attention', stopped: 'sessions', test: 'devices' };
function pushKind(event) {
  try { const kind = event.data?.json()?.k; return Object.hasOwn(destinations, kind) ? kind : 'update'; } catch { return 'update'; }
}
self.addEventListener('push', (event) => {
  const kind = pushKind(event);
  event.waitUntil(self.registration.showNotification('LocalLink', {
    body: 'There is an update on your PC. Open LocalLink to check.', tag: `locallink-${kind}`, data: { k: kind },
  }));
});
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const client = windows.find((client) => new URL(client.url).origin === self.location.origin && new URL(client.url).pathname === '/');
    const destination = destinations[event.notification.data?.k] || 'attention';
    if (client) { client.postMessage({ type: 'notification-open', destination }); await client.focus(); }
    else await self.clients.openWindow(`/#${destination}`);
  })());
});
