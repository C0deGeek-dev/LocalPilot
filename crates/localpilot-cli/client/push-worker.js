// Verified static shell updates automatically. Never caches private responses or replays mutations.
/* global self, caches, fetch, MessageChannel, setTimeout, clearTimeout */
const generation = 'f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103';
const cacheName = `locallink-shell-${generation}`;
const shellFiles = [{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/accessibility.js","integrity":"sha256-cdM1DPkH0iWZsOIFG+5vzzJUhTx8W8HnddPX1p2Tfcc="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/app.js","integrity":"sha256-e6C8Vwk2uuH4qnwZr0MeYV6MHys2DkqrGbwBkXNi0po="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/companion-view.js","integrity":"sha256-xVND0kLRQ1k3loT/ny2s83grCAbm2LbFe7sfaCfZgbE="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/compatibility.js","integrity":"sha256-utyfSYFqEeEZZC4M7vqp3Y2ApalA6gIRkDbeoaNf12A="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/device.js","integrity":"sha256-Ji6Pce589OjTBag8cdr7iRkGwEYheTxpVARgxbk+1Uk="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/icon-192.png","integrity":"sha256-JeHq97uQLPJKIRrm+I6eZzaXBonOOcl14oOaAZKiXl0="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/icon-512.png","integrity":"sha256-M092CgQsFn+0hCfcLSAOAXqYUwtWDJpLnvBJQlPhVe4="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/icon.svg","integrity":"sha256-TzYAdGS3TipGhrnsdNB4z05bHnqI8CY4W22W5gM2T+E="},{"url":"/index.html","integrity":"sha256-onnVB+ei7ObiPc/nK7BxcxoRkbm5tQecgedGNZogqKo="},{"url":"/manifest.webmanifest","integrity":"sha256-TbP1GZU6oKtbsPzkJY2+vrNQmluZi9N0f9dEUlmeXLY="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/notifications.js","integrity":"sha256-s0Br2FdHyvXzN9S9lGOqPoa209xiLmxGVY6lkJSyPng="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/offline.js","integrity":"sha256-LOWQAiC8RqYikqI3Ez4dkugTXeRGKZLxchAm2r4pzVg="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/pairing.js","integrity":"sha256-wfm/IGM8D4lYRQyWBv2GrCkqGPiCn2D32aDf1VeWmGE="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/session-view.js","integrity":"sha256-9wSMq2V1wqaXlhbet0ta2bq+uuv5T+6oSEEWz7O0Mac="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/session.js","integrity":"sha256-rnODeU8Nlgluvto7Ern7bxYl1PxOc9vO163tpmBed90="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/style.css","integrity":"sha256-LJLhpkZyDoWT4tJqdlZy9ecisJxrJZTWRCYx0inT3Hc="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/theme.js","integrity":"sha256-9Wjmlsb8zp/+YeUQOU/Py0eZt20JwQg94++dXVbAe4U="},{"url":"/assets/f46cb586c52207808881c25a6e73fd69c77f798c9020a8a63a920bc3da0af103/update-state.js","integrity":"sha256-qmF5Snmjo523e1LcvWKetdx/AKDTK2M18wcG//qj1HM="}];
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
self.addEventListener('push', (event) => {
  event.waitUntil(self.registration.showNotification('LocalLink', {
    body: 'There is an update on your PC. Open LocalLink to check.', tag: 'locallink-update',
  }));
});
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const client = windows.find((client) => new URL(client.url).origin === self.location.origin && new URL(client.url).pathname === '/');
    if (client) { client.postMessage({ type: 'notification-open' }); await client.focus(); }
    else await self.clients.openWindow('/#attention');
  })());
});
