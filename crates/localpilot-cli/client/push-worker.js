// Static shell only. No takeover, private response caching or mutation replay.
/* global self, caches, fetch */
const cacheName = 'locallink-shell-483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da';
const shellFiles = [{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/accessibility.js","integrity":"sha256-cdM1DPkH0iWZsOIFG+5vzzJUhTx8W8HnddPX1p2Tfcc="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/app.js","integrity":"sha256-LEvps0Nr7fLGSpMettAsZKH/iE/Vm4dD9mjtlc7dYms="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/companion-view.js","integrity":"sha256-CKSbh3imYFMFyhSafo+xEED7ZaHleNH4TjsZY2yLnVw="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/compatibility.js","integrity":"sha256-utyfSYFqEeEZZC4M7vqp3Y2ApalA6gIRkDbeoaNf12A="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/device.js","integrity":"sha256-Ji6Pce589OjTBag8cdr7iRkGwEYheTxpVARgxbk+1Uk="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/icon-192.png","integrity":"sha256-GI3gyF3pZ/HXF1+u6FRsv/62h4IstOL8RDMJJUAaOC4="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/icon-512.png","integrity":"sha256-CnGHsMaceVj8am4qOCHsWa33jdzrHfDwY63TMXXD+VU="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/icon.svg","integrity":"sha256-ABMKrfZLBTil9LJWUYH4KVUNm1uKiJOTOo2ObbRgAZA="},{"url":"/index.html","integrity":"sha256-Sfi0n5x1JWQaahtIPYeE7ZGGeRK0uRWZ5ZP7eX/SU+Q="},{"url":"/manifest.webmanifest","integrity":"sha256-wSrG4HRJgYWtOMNbIkWgpMMDikrEFYWzN1s+Jrab1CA="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/notifications.js","integrity":"sha256-s0Br2FdHyvXzN9S9lGOqPoa209xiLmxGVY6lkJSyPng="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/offline.js","integrity":"sha256-DeSZgkquA1vWuAwpYlzV2eBeS4zCTWh6dIyKHc87/hk="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/pairing.js","integrity":"sha256-wfm/IGM8D4lYRQyWBv2GrCkqGPiCn2D32aDf1VeWmGE="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/session-view.js","integrity":"sha256-4MQgkxZLXTdGsz/V2eA/ZVwAhebH6InRa/PLMU1xIHg="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/session.js","integrity":"sha256-VuuPsGwAVJFYch1KMGxyYIVO7Ttr0gSi0r39Vw/kJwo="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/style.css","integrity":"sha256-SXLJ1FTaHxoFzbCVh6yPk4qvCCMpv1YLJVyT7uYLqIU="},{"url":"/assets/483642f20413b2370a0e765b2ee012ae7525c7d9d8c3175f30939e8630da46da/theme.js","integrity":"sha256-xchf1jxFSRGD0jvbh0OmhUbb/aPOx4x9gWJ7JNgFhYw="}];
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
