export async function prepareOffline(browser, changed, beforeReload = () => true) {
  const workers = browser.navigator.serviceWorker;
  if (!workers || !browser.isSecureContext) { changed('Offline shell is unavailable in this browser.'); return; }
  // The new worker asks each tab to preserve its draft before navigating it.
  // Old clients without this handler are refreshed by the worker as well.
  workers.addEventListener('message', (event) => {
    if (event.data?.type !== 'shell-update' || !event.ports?.[0]) return;
    const generation = browser.document.querySelector('meta[name="locallink-generation"]')?.content;
    const defer = generation !== event.data.generation && !beforeReload();
    if (defer) changed('App update ready. Copy your unsent draft and reload to update; tab storage is unavailable. Nothing was sent again.');
    else if (generation !== event.data.generation) changed('Updating the app… Your session keeps running on your PC.');
    event.ports[0].postMessage({ generation, defer });
  });
  try {
    const registration = await workers.register('/push-worker.js', { scope: '/', updateViaCache: 'none' });
    const report = () => changed(registration.waiting
      ? 'App update ready. Loading the new version automatically…'
      : registration.active ? 'App shell available offline. Online updates load automatically. Session data stays on your PC.' : 'Preparing the app shell for offline use…');
    const observe = () => {
      registration.installing?.addEventListener('statechange', report);
      report();
    };
    registration.addEventListener('updatefound', observe);
    observe();
    // A mobile PWA can stay suspended for days. Check when it is opened again,
    // and periodically while visible, rather than waiting for browser heuristics.
    let checking = false;
    const check = async () => {
      if (checking || browser.document?.hidden || browser.navigator.onLine === false) return;
      checking = true;
      try { await registration.update(); } catch { /* Keep the last complete offline shell. */ }
      finally { checking = false; }
    };
    browser.addEventListener('focus', () => void check());
    browser.addEventListener('online', () => void check());
    browser.document?.addEventListener('visibilitychange', () => { if (!browser.document.hidden) void check(); });
    browser.setInterval(() => void check(), 60000);
    void check();
  } catch { changed('Could not prepare the offline app shell. Keep this window open and try again when the PC is reachable.'); }
}
