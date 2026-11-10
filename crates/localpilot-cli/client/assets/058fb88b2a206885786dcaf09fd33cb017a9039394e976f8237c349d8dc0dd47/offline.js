export async function prepareOffline(browser, changed) {
  const workers = browser.navigator.serviceWorker;
  if (!workers || !browser.isSecureContext) { changed('Offline shell is unavailable in this browser.'); return; }
  try {
    const registration = await workers.register('/push-worker.js', { scope: '/', updateViaCache: 'none' });
    const report = () => changed(registration.waiting
      ? 'App update ready. Your current view and drafts stay here. Finish or copy your drafts, then close every LocalLink browser tab and close LocalLink from Android recent apps. Reopen to update.'
      : registration.active ? 'App shell available offline. Session data stays in this window; reconnect to confirm the PC’s state.' : 'Preparing the app shell for offline use…');
    const observe = () => {
      registration.installing?.addEventListener('statechange', report);
      report();
    };
    registration.addEventListener('updatefound', observe);
    observe();
  } catch { changed('Could not prepare the offline app shell. Keep this window open and try again when the PC is reachable.'); }
}
