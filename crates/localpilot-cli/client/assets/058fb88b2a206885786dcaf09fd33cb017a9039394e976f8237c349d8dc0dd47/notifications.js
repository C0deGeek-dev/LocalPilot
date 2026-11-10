export class Notifications {
  constructor({ browser, fetch, changed }) {
    Object.assign(this, { browser, fetch, changed });
    this.generation = 0; this.authorized = false; this.busy = false; this.enabled = false; this.message = 'Pair this phone before enabling notifications.';
  }
  get supported() { return !!(this.browser.Notification && this.browser.PushManager && this.browser.navigator.serviceWorker); }
  access(authorized) {
    if (this.authorized === authorized) return;
    this.authorized = authorized; ++this.generation; this.busy = false; this.enabled = false; this.workspace = '';
    this.message = authorized ? 'Notifications are not confirmed. Choose a project, then enable or re-enable notifications.' : 'Pair this phone before enabling notifications.';
    this.changed(this);
  }
  select(workspace) {
    if (this.workspace === workspace) return;
    ++this.generation; this.workspace = workspace; this.busy = false; this.enabled = false;
    this.message = 'Notifications are not confirmed for this project. Use Enable notifications to register or re-enable.'; this.changed(this);
  }
  async request(body) {
    const response = await this.fetch(`/api/push?workspace=${encodeURIComponent(this.workspace)}`, {
      method: body ? 'POST' : 'GET', credentials: 'same-origin', mode: 'same-origin', cache: 'no-store', redirect: 'error',
      ...(body ? { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) } : {}), signal: AbortSignal.timeout(15000),
    });
    if (!response.ok) {
      const messages = { 401: 'Access ended. Check your connection.', 403: 'Your PC does not allow notifications for this device.', 409: 'Notifications are off or the key changed. Enable notifications again.', 410: 'The subscription expired. Enable notifications again.', 429: 'Please wait before testing again (at least 30 seconds).', 503: 'Notifications are turned off on your PC. Enable remote push in LocalPilot.', 502: 'The push service could not deliver the test. Check your PC and try later.' };
      throw new Error(messages[response.status] || 'The PC could not confirm this change. Check its notification settings before trying again.');
    }
    return response.json();
  }
  async run(operation) {
    if (!this.authorized || !this.workspace || this.busy || !this.supported) return;
    const generation = this.generation;
    const current = () => generation === this.generation && this.authorized;
    this.busy = true; this.changed(this);
    try { await operation(current); }
    catch (error) { if (current()) { this.message = error.message; this.enabled = false; } }
    finally { if (current()) { this.busy = false; this.changed(this); } }
  }
  enable() {
    // run invokes the callback synchronously, preserving the click activation.
    return this.run(async (current) => {
      const permission = await this.browser.Notification.requestPermission();
      if (!current()) return;
      if (permission !== 'granted') { this.message = permission === 'denied' ? 'Notifications blocked. Allow notifications in this site’s browser settings and Android notification settings, then use Enable notifications again.' : 'Permission was not granted. Use Enable notifications when ready.'; this.enabled = false; return; }
      const key = await this.request();
      if (!current()) return;
      if (typeof key.public_key !== 'string' || typeof key.key_id !== 'string') throw new Error('The PC returned an incompatible notification key.');
      const registration = await this.browser.navigator.serviceWorker.register('/push-worker.js', { scope: '/' });
      if (!current()) return;
      await new Promise((resolve, reject) => {
        const worker = registration.installing || registration.waiting || registration.active;
        if (registration.active) { resolve(); return; }
        const timer = this.browser.setTimeout(() => reject(new Error('Notification worker did not activate. Reload and enable again.')), 15000);
        worker.addEventListener('statechange', () => {
          if (worker.state === 'activated') { this.browser.clearTimeout(timer); resolve(); }
          else if (worker.state === 'redundant') { this.browser.clearTimeout(timer); reject(new Error('Notification worker could not activate.')); }
        });
      });
      if (!current()) return;
      const raw = this.browser.atob(key.public_key.replaceAll('-', '+').replaceAll('_', '/'));
      const bytes = Uint8Array.from(raw, (character) => character.charCodeAt(0));
      let subscription = await registration.pushManager.getSubscription();
      if (!current()) return;
      if (subscription && [...new Uint8Array(subscription.options.applicationServerKey || [])].join() !== [...bytes].join()) {
        await subscription.unsubscribe(); subscription = null;
      }
      if (!current()) return;
      subscription ||= await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: bytes });
      if (!current()) return;
      const json = subscription.toJSON();
      const result = await this.request({ op: 'subscribe', endpoint: json.endpoint, p256dh: json.keys.p256dh, auth: json.keys.auth, key_id: key.key_id });
      if (!current()) return;
      if (result.subscribed !== true) throw new Error('Your PC did not confirm notification registration.');
      this.enabled = true; this.message = 'Notifications enabled for this project. Use Test notification to check delivery on this phone.';
    });
  }
  test() {
    if (!this.enabled) return;
    return this.run(async (current) => {
      const result = await this.request({ op: 'test' });
      if (!current()) return;
      if (result.sent !== true) throw new Error('The PC did not confirm sending the test.');
      this.message = 'Test sent through the push service. Check this phone’s notification tray; sending alone does not prove display. If missing, check browser and Android notification settings.';
    });
  }
  off() {
    return this.run(async (current) => {
      const result = await this.request({ op: 'unsubscribe' });
      if (!current()) return;
      if (result.subscribed !== false) throw new Error('Your PC did not confirm notifications off.');
      this.enabled = false;
      // Server off is authoritative. Keep the browser subscription reusable:
      // another workspace on this origin may still use it.
      this.message = 'Notifications off for this project. Browser permission remains available for re-enabling.';
    });
  }
}
