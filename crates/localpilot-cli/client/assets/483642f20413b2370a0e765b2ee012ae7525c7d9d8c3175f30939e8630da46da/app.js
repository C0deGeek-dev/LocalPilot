import { Pairing, PairingCamera, cameraMessages, messages, takePairingLink } from './pairing.js';
import { mountSessions } from './session-view.js';
import { mountCompanion } from './companion-view.js';
import { prepareOffline } from './offline.js';
import { mountTheme } from './theme.js';

// Navigation is local. A network connection alone never enables session controls.
const destinations = new Set(['sessions', 'attention', 'devices']);

export function destinationFromHash(hash) {
  const destination = hash.slice(1);
  return destinations.has(destination) ? destination : 'sessions';
}

export function showDestination(document, hash, focus = false) {
  const destination = destinationFromHash(hash);
  for (const panel of document.querySelectorAll('[data-panel]')) {
    panel.hidden = panel.dataset.panel !== destination;
  }
  for (const link of document.querySelectorAll('[data-destination]')) {
    if (link.dataset.destination === destination) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
  if (focus) document.querySelector(`[data-panel="${destination}"] h1:not([hidden]), [data-panel="${destination}"] h2:not([hidden])`)?.focus();
}

if (typeof document !== 'undefined') {
  let incoming = takePairingLink(window.location, window.history);
  mountTheme(document, window);
  const sessions = mountSessions(document, window);
  const element = (id) => document.getElementById(id);
  void prepareOffline(window, (message) => { element('app-update').textContent = message; });
  const form = element('pair-form');
  const code = element('pair-code');
  const workspace = element('pair-workspace');
  const confirm = element('pair-confirm');
  const cameraPanel = element('pair-camera');
  const cameraStatus = element('camera-status');
  const origin = window.location.origin;
  element('pair-origin').textContent = origin;
  element('connected-origin').textContent = origin;
  const pairing = new Pairing({
    origin, secure: window.isSecureContext, fetch: window.fetch.bind(window),
    changed({ state, busy, scope, workspaces, retryAt, issue }) {
      const [title, description] = messages[state];
      element('pair-title').textContent = title;
      element('pair-description').textContent = description;
      element('pair-issue').textContent = issue ? `New pairing: ${messages[issue].join('. ')} Your existing access is still confirmed.` : '';
      element('pair-status').dataset.state = state;
      element('pair-retry').textContent = state === 'throttled' ? `Try after ${new Date(retryAt).toLocaleTimeString()}.` : '';
      element('pair-status').setAttribute('aria-busy', String(busy));
      const paired = state === 'paired';
      if (paired) sessions.access(scope, workspaces);
      else if (['revoked', 'unpaired'].includes(state)) sessions.revoke();
      else if (sessions.ready) sessions.fail('disconnected');
      element('pair-connected').hidden = !paired;
      element('pair-scope').textContent = scope === 'control' ? 'Access granted: control.' : scope === 'observe' ? 'Access granted: observe only.' : 'Access confirmed by your computer.';
      element('connection').textContent = paired ? 'Paired' : state === 'revoked' ? 'Access ended' : 'Not connected';
      form.hidden = paired || state === 'confirm';
      confirm.hidden = state !== 'confirm';
      element('scan-code').hidden = paired || state === 'confirm';
      for (const control of document.querySelectorAll('[data-pair-control]')) control.disabled = busy;
    },
  });
  const camera = new PairingCamera({
    media: window.navigator.mediaDevices, Detector: window.BarcodeDetector, video: element('camera-video'),
    schedule: window.setTimeout.bind(window), unschedule: window.clearTimeout.bind(window),
    found(value) {
      cameraPanel.hidden = true;
      cameraStatus.textContent = '';
      pairing.prepare(value);
      focusPairing();
    },
    changed(state) {
      cameraPanel.hidden = !['starting', 'scanning'].includes(state);
      cameraStatus.textContent = cameraMessages[state] || (state === 'starting' ? 'Waiting for camera access…' : 'Point the camera at the pairing QR code on your PC.');
    },
  });
  sessions.accessLost = () => void pairing.check();
  mountCompanion(document, window, sessions, async () => { await pairing.check(); return pairing.state === 'paired'; });
  function focusPairing() {
    (pairing.state === 'confirm' ? element('confirm-pair') : element('pair-title')).focus();
  }
  function clearInputs() { code.value = ''; workspace.value = ''; }
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    camera.stop();
    cameraPanel.hidden = true;
    cameraStatus.textContent = '';
    pairing.prepare(code.value, workspace.value);
    clearInputs();
    focusPairing();
  });
  element('confirm-pair').addEventListener('click', () => {
    void pairing.exchange().then(() => {
      if (!document.hidden && !element('sessions-title').closest('section').hidden) focusPairing();
    });
  });
  element('cancel-pair').addEventListener('click', () => { pairing.cancel(); code.focus(); });
  element('check-connection').addEventListener('click', () => {
    camera.stop(); cameraPanel.hidden = true; cameraStatus.textContent = '';
    clearInputs(); void pairing.check();
  });
  element('scan-code').addEventListener('click', () => {
    if (!window.isSecureContext || window.location.protocol !== 'https:') { pairing.prepare(''); return; }
    clearInputs(); void camera.start();
  });
  element('stop-camera').addEventListener('click', () => {
    camera.stop(); cameraPanel.hidden = true; cameraStatus.textContent = 'Camera stopped. You can use manual entry.';
    element('scan-code').focus();
  });
  function leaveCamera() { incoming = null; camera.stop(); cameraPanel.hidden = true; cameraStatus.textContent = ''; clearInputs(); pairing.cancel(); }
  showDestination(document, window.location.hash);
  window.addEventListener('hashchange', () => {
    if (window.location.hash === '#main') { element('main').focus(); return; }
    const link = takePairingLink(window.location, window.history);
    leaveCamera();
    showDestination(document, window.location.hash, true);
    if (link) { pairing.prepare(link); focusPairing(); }
  });
  window.addEventListener('pagehide', leaveCamera);
  document.addEventListener('visibilitychange', () => { if (document.hidden) leaveCamera(); });
  // A read-only check is also how a returning phone confirms its cookie.
  void pairing.check().then(() => {
    if (incoming) { pairing.prepare(incoming); incoming = null; focusPairing(); }
  });
}
