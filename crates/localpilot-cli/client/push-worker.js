// Push only: no fetch handler, caches, skipWaiting or offline mutations.
/* global self */
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
