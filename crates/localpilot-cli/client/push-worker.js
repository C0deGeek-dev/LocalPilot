// Verified static shell updates automatically. Never caches private responses or replays mutations.
/* global self, caches, fetch, MessageChannel, setTimeout, clearTimeout */
const generation = 'c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b';
const cacheName = `locallink-shell-${generation}`;
const shellFiles = [{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/accessibility.js","integrity":"sha256-cdM1DPkH0iWZsOIFG+5vzzJUhTx8W8HnddPX1p2Tfcc="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/app.js","integrity":"sha256-BQYmIG+y3pnGgsyVV0gr85ESFTVrqKHop3our3/1gIg="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/companion-view.js","integrity":"sha256-xVND0kLRQ1k3loT/ny2s83grCAbm2LbFe7sfaCfZgbE="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/compatibility.js","integrity":"sha256-utyfSYFqEeEZZC4M7vqp3Y2ApalA6gIRkDbeoaNf12A="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/device.js","integrity":"sha256-Ji6Pce589OjTBag8cdr7iRkGwEYheTxpVARgxbk+1Uk="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/icon-192.png","integrity":"sha256-GI3gyF3pZ/HXF1+u6FRsv/62h4IstOL8RDMJJUAaOC4="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/icon-512.png","integrity":"sha256-CnGHsMaceVj8am4qOCHsWa33jdzrHfDwY63TMXXD+VU="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/icon.svg","integrity":"sha256-ABMKrfZLBTil9LJWUYH4KVUNm1uKiJOTOo2ObbRgAZA="},{"url":"/index.html","integrity":"sha256-L9FoD5U5ZdkDsjbaObDmPSJ6vdeQeT+cjbAlyZxq5uE="},{"url":"/manifest.webmanifest","integrity":"sha256-ctUkrxV7KJYtuRjbj4sjCFtoRJjlv1f4gHMSSduryJo="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/notifications.js","integrity":"sha256-s0Br2FdHyvXzN9S9lGOqPoa209xiLmxGVY6lkJSyPng="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/offline.js","integrity":"sha256-LOWQAiC8RqYikqI3Ez4dkugTXeRGKZLxchAm2r4pzVg="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/pairing.js","integrity":"sha256-wfm/IGM8D4lYRQyWBv2GrCkqGPiCn2D32aDf1VeWmGE="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/session-view.js","integrity":"sha256-12WV1Olc1bOUrUrZwS3OIHlo2RQmlW7LGleFaAkKgEk="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/session.js","integrity":"sha256-rnODeU8Nlgluvto7Ern7bxYl1PxOc9vO163tpmBed90="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/style.css","integrity":"sha256-KuaNywGDI8dMFMumpN6nppfswolPpX40Fo/x0rCcPI8="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/theme.js","integrity":"sha256-xchf1jxFSRGD0jvbh0OmhUbb/aPOx4x9gWJ7JNgFhYw="},{"url":"/assets/c911885be1f92aca0c9fa6b02bad1614e981d5014e0cfc513353c06a5ed5a96b/update-state.js","integrity":"sha256-qmF5Snmjo523e1LcvWKetdx/AKDTK2M18wcG//qj1HM="}];
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
