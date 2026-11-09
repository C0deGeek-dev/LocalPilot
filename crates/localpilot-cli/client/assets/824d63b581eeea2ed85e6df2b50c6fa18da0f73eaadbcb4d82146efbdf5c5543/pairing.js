export class PairingError extends Error {
  constructor(state) { super(state); this.state = state; }
}

export function parsePairing(value, workspace, origin) {
  let code = value.trim();
  let target = workspace.trim();
  if (code.length > 4096) throw new PairingError('invalid');
  if (code.includes('://')) {
    let link;
    try { link = new URL(code); } catch { throw new PairingError('invalid'); }
    if (link.protocol !== 'https:' || link.origin !== origin) throw new PairingError('wrong-origin');
    if (link.username || link.password || link.pathname !== '/' || link.search) throw new PairingError('invalid');
    const params = new URLSearchParams(link.hash.slice(1));
    if ([...params.keys()].some((key) => !['pair', 'w'].includes(key)) ||
        params.getAll('pair').length !== 1 || params.getAll('w').length !== 1) throw new PairingError('invalid');
    code = params.get('pair');
    target = params.get('w');
  }
  if (!target) throw new PairingError('workspace-required');
  if (!/^[a-f0-9]{16}$/.test(target) || !code || code.length > 1024 ||
      /\s/u.test(code) || [...code].some((char) => char.charCodeAt(0) < 32 || char.charCodeAt(0) === 127)) throw new PairingError('invalid');
  return { code, workspace: target };
}

// Remove secret fragments before the first asynchronous operation or navigation.
export function takePairingLink(location, history) {
  if (!location.hash || ['#sessions', '#attention', '#devices', '#main'].includes(location.hash)) return null;
  const link = location.href;
  history.replaceState(null, '', `${location.pathname}${location.search}#sessions`);
  return link;
}

export const messages = {
  checking: ['Checking this phone', 'Contacting your computer securely…'],
  unpaired: ['Connect your computer', 'Scan the QR code on your PC, or enter its pairing link below.'],
  confirm: ['Is this your computer?', 'Check this HTTPS address against the one shown on your PC before pairing.'],
  submitting: ['Pairing this phone', 'Waiting for your computer. Keep this page open.'],
  paired: ['Phone paired', 'Your secure connection is ready. Session viewing and controls are still being built.'],
  invalid: ['Check the pairing details', 'Use the complete pairing link, or the code and 16-character workspace ID shown on your PC.'],
  'workspace-required': ['Workspace ID needed', 'Enter the Workspace line shown with the code on your PC, or copy the complete link from localpilot devices pair --url <your HTTPS address>.'],
  'wrong-origin': ['This link is for another address', 'Open the HTTPS address shown on your PC and enter the link there. No code was sent.'],
  insecure: ['Open your secure PC address', 'Pairing needs the trusted HTTPS address shown on your PC. The local HTTP preview cannot pair.'],
  rejected: ['This code could not be accepted', 'It may be expired, already used, invalid, or closed on the PC. Create a fresh code on your computer.'],
  forbidden: ['The PC refused this address', 'Check the gateway’s configured HTTPS address and proxy settings on your computer.'],
  missing: ['Workspace no longer served', 'Open LocalPilot in that project on your PC and create a fresh pairing link.'],
  offline: ['Workspace is offline', 'Start the project’s LocalPilot server on your PC, then create a fresh pairing link.'],
  throttled: ['Too many requests', 'Pairing is temporarily limited across this computer. Wait before trying again.'],
  server: ['The PC could not finish pairing', 'Check LocalPilot on your computer before creating a fresh code.'],
  protocol: ['Unexpected gateway response', 'Check that your LocalPilot gateway and LocalLink versions are compatible.'],
  unreachable: ['Cannot reach your computer', 'Check that your PC is awake, LocalPilot is running and your phone can reach its HTTPS address.'],
  unknown: ['Pairing result is unknown', 'The reply was lost. Check connection before entering a fresh code. The exchange will not be repeated automatically.'],
  'cookie-missing': ['Pairing could not be confirmed', 'This phone has no accepted credential. Allow cookies for this HTTPS address, check access on the PC, then create a fresh pairing code.'],
  revoked: ['This phone no longer has access', 'Access may have been revoked, or the browser credential was cleared. Create a new pairing code on your PC to connect again.'],
};

export function pairingFailure(status, body) {
  if (status === 403 && body === 'forbidden') return 'forbidden';
  if (status === 403 && body === 'the pairing code was not accepted') return 'rejected';
  return ({ 400: 'invalid', 404: 'missing', 415: 'protocol', 429: 'throttled', 500: 'server', 503: 'offline' })[status] || 'protocol';
}

export function retryDelay(value, now = Date.now()) {
  if (!value) return 60;
  const seconds = /^\d+$/.test(value) ? Number(value) : Math.ceil((Date.parse(value) - now) / 1000);
  return Number.isFinite(seconds) ? Math.max(1, Math.min(seconds, 86400)) : 60;
}

export class Pairing {
  constructor({ origin, secure, fetch, changed, timeout = 15000, now = () => Date.now() }) {
    Object.assign(this, { origin, secure, fetch, changed, timeout, now });
    this.busy = false;
    this.wasPaired = false;
    this.retryAt = 0;
    this.pending = null;
    this.scope = null;
    this.issue = null;
    this.set('checking');
  }
  set(state) {
    this.state = state;
    this.changed({ state, busy: this.busy, scope: this.scope, retryAt: this.retryAt, issue: this.issue });
  }
  async request(path, options = {}) {
    const response = await this.fetch(path, {
      credentials: 'same-origin', mode: 'same-origin', cache: 'no-store', redirect: 'error',
      ...options, signal: AbortSignal.timeout(this.timeout),
    });
    const body = await response.text();
    return { status: response.status, body, retry: response.headers.get('Retry-After') };
  }
  async probe(afterExchange = false, ambiguous = false) {
    try {
      const result = await this.request('/api/workspaces');
      if (result.status === 200) {
        let data;
        try { data = JSON.parse(result.body); } catch { return this.set('protocol'); }
        if (!data || !Array.isArray(data.workspaces)) return this.set('protocol');
        this.wasPaired = true;
        return this.set('paired');
      }
      if (result.status === 401) return this.set(this.wasPaired ? 'revoked' : ambiguous ? 'unknown' : afterExchange ? 'cookie-missing' : 'unpaired');
      if (result.status === 429) {
        this.retryAt = this.now() + retryDelay(result.retry) * 1000;
        return this.set('throttled');
      }
      this.set(result.status === 403 && result.body === 'forbidden' ? 'forbidden' : 'protocol');
    } catch {
      this.set(ambiguous ? 'unknown' : 'unreachable');
    }
  }
  async check() {
    if (this.busy) return;
    if (!this.secure || !this.origin.startsWith('https://')) return this.set('insecure');
    if (this.now() < this.retryAt) return this.set('throttled');
    this.pending = null;
    this.scope = null;
    this.issue = null;
    this.busy = true;
    this.set('checking');
    await this.probe();
    this.busy = false;
    this.set(this.state);
  }
  prepare(value, workspace = '') {
    if (this.busy) return;
    this.pending = null;
    this.issue = null;
    if (!this.secure || !this.origin.startsWith('https://')) return this.set('insecure');
    if (this.now() < this.retryAt) return this.set('throttled');
    try {
      this.pending = parsePairing(value, workspace, this.origin);
      this.set('confirm');
    } catch (error) { this.set(error.state || 'invalid'); }
  }
  cancel() {
    this.pending = null;
    if (!this.busy && this.state === 'confirm') this.set('unpaired');
  }
  async exchange() {
    if (this.busy || !this.pending || this.state !== 'confirm') return;
    const details = this.pending;
    this.pending = null;
    this.scope = null;
    this.issue = null;
    this.busy = true;
    this.set('submitting');
    try {
      const result = await this.request('/api/pair', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(details),
      });
      details.code = '';
      if (result.status === 200) {
        let data;
        try { data = JSON.parse(result.body); } catch { this.set('protocol'); return; }
        if (!data || typeof data.device_id !== 'string' || !['observe', 'control'].includes(data.scope)) {
          this.set('protocol');
        } else {
          this.scope = data.scope;
          await this.probe(true);
        }
      } else {
        if (result.status === 429) this.retryAt = this.now() + retryDelay(result.retry) * 1000;
        const failure = pairingFailure(result.status, result.body);
        if (this.wasPaired) {
          // A failed replacement exchange does not revoke the existing cookie.
          await this.probe();
          if (this.state === 'paired') this.issue = failure;
        } else this.set(failure);
      }
    } catch {
      // POST may have committed. Only a read is safe after a lost reply.
      await this.probe(true, true);
    } finally {
      details.code = '';
      this.busy = false;
      this.set(this.state);
    }
  }
}

export const cameraMessages = {
  unsupported: 'Camera QR scanning is unavailable in this browser. Paste the pairing link or enter the code below.',
  denied: 'Camera access was denied. Allow it in browser settings, or use manual entry.',
  missing: 'No camera was found. Use manual entry below.',
  busy: 'The camera could not start. Close other camera apps, or use manual entry.',
  failed: 'The QR code could not be read. Try scanning again, or use manual entry.',
};

export class PairingCamera {
  constructor({ media, Detector, video, schedule, unschedule, found, changed }) {
    Object.assign(this, { media, Detector, video, schedule, unschedule, found, changed });
    this.generation = 0;
  }
  stop() {
    this.generation++;
    if (this.timer) this.unschedule(this.timer);
    this.timer = null;
    this.stream?.getTracks().forEach((track) => track.stop());
    this.stream = null;
    this.video.srcObject = null;
  }
  async start() {
    this.stop();
    const generation = this.generation;
    const current = () => generation === this.generation;
    this.changed('starting');
    try {
      if (!this.media?.getUserMedia || !this.Detector ||
          !(await this.Detector.getSupportedFormats()).includes('qr_code')) {
        if (current()) this.changed('unsupported');
        return;
      }
      if (!current()) return;
      const stream = await this.media.getUserMedia({ video: { facingMode: { ideal: 'environment' } }, audio: false });
      if (!current()) { stream.getTracks().forEach((track) => track.stop()); return; }
      this.stream = stream;
      this.video.srcObject = stream;
      await this.video.play();
      if (!current()) return;
      const detector = new this.Detector({ formats: ['qr_code'] });
      this.changed('scanning');
      const scan = async () => {
        if (!current()) return;
        try {
          if (this.video.readyState >= 2) {
            const codes = await detector.detect(this.video);
            if (!current()) return;
            const code = codes.find((code) => code.format === 'qr_code' && code.rawValue);
            if (code) { this.stop(); this.found(code.rawValue); return; }
          }
          if (current()) this.timer = this.schedule(scan, 250);
        } catch { if (current()) { this.stop(); this.changed('failed'); } }
      };
      await scan();
    } catch (error) {
      if (!current()) return;
      this.stop();
      this.changed(({ NotAllowedError: 'denied', SecurityError: 'denied', NotFoundError: 'missing', NotReadableError: 'busy' })[error.name] || 'busy');
    }
  }
}
