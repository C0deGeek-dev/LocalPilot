// Static shell only. No takeover, private response caching or mutation replay.
/* global self, caches, fetch */
const cacheName = 'locallink-shell-058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47';
const shellFiles = [{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/accessibility.js","integrity":"sha256-cdM1DPkH0iWZsOIFG+5vzzJUhTx8W8HnddPX1p2Tfcc="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/app.js","integrity":"sha256-BITOAdamajzcM1QnjJ0hcgxCvDptgUn979wNmHiAXQw="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/companion-view.js","integrity":"sha256-xVND0kLRQ1k3loT/ny2s83grCAbm2LbFe7sfaCfZgbE="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/compatibility.js","integrity":"sha256-utyfSYFqEeEZZC4M7vqp3Y2ApalA6gIRkDbeoaNf12A="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/device.js","integrity":"sha256-Ji6Pce589OjTBag8cdr7iRkGwEYheTxpVARgxbk+1Uk="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/icon-192.png","integrity":"sha256-GI3gyF3pZ/HXF1+u6FRsv/62h4IstOL8RDMJJUAaOC4="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/icon-512.png","integrity":"sha256-CnGHsMaceVj8am4qOCHsWa33jdzrHfDwY63TMXXD+VU="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/icon.svg","integrity":"sha256-ABMKrfZLBTil9LJWUYH4KVUNm1uKiJOTOo2ObbRgAZA="},{"url":"/index.html","integrity":"sha256-0ZIt/m+Nq6UB0ACY33+ioJ+pPW2KGaG2e+pOUl5zfRU="},{"url":"/manifest.webmanifest","integrity":"sha256-shjBI/0FUAtcaCKZfyL37RlTFdFZ7c/MJkuid2ul8QU="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/notifications.js","integrity":"sha256-s0Br2FdHyvXzN9S9lGOqPoa209xiLmxGVY6lkJSyPng="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/offline.js","integrity":"sha256-DeSZgkquA1vWuAwpYlzV2eBeS4zCTWh6dIyKHc87/hk="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/pairing.js","integrity":"sha256-wfm/IGM8D4lYRQyWBv2GrCkqGPiCn2D32aDf1VeWmGE="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/session-view.js","integrity":"sha256-kXaCtv1Mez4ZbH1fNFwfgKg/GezEu2AlSiO2iLP1nms="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/session.js","integrity":"sha256-isAGsxOmTyP7iezh8eTcjqPRdn+BMHFasvNbRdvHlnA="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/style.css","integrity":"sha256-ZRoQ2Wz3YwRQ3g40SSH/hy028la1pLFFkUemVF48LAw="},{"url":"/assets/058fb88b2a206885786dcaf09fd33cb017a9039394e976f8237c349d8dc0dd47/theme.js","integrity":"sha256-xchf1jxFSRGD0jvbh0OmhUbb/aPOx4x9gWJ7JNgFhYw="}];
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
    } catch (error) { await caches.delete(cacheName); throw error; }
  })());
});
self.addEventListener('activate', (event) => {
  // Natural activation waits for every old controlled window to close.
  event.waitUntil((async () => {
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
