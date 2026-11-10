export class Device {
  constructor({ fetch, changed, revoked }) { Object.assign(this, { fetch, changed, revoked }); this.generation = 0; this.authorized = false; this.busy = false; }
  access(authorized) {
    if (this.authorized === authorized) return;
    this.authorized = authorized; ++this.generation; this.busy = false; this.record = null; this.message = '';
    if (authorized) void this.load();
    else this.changed(this);
  }
  async request(path, method = 'GET') {
    return this.fetch(path, { method, credentials: 'same-origin', mode: 'same-origin', cache: 'no-store', redirect: 'error', signal: AbortSignal.timeout(15000) });
  }
  async load() {
    if (!this.authorized || this.busy) return;
    const generation = this.generation;
    this.busy = true; this.changed(this);
    try {
      const response = await this.request('/api/device');
      if (generation !== this.generation) return;
      if (response.status === 401) { this.revoked(); return; }
      if (!response.ok) throw new Error();
      const record = await response.json();
      if (generation !== this.generation) return;
      if (!/^[a-f0-9]{16}$/.test(record.id) || typeof record.name !== 'string' || !['observe', 'control'].includes(record.scope)) throw new Error();
      this.record = record; this.message = '';
    } catch { if (generation === this.generation) this.message = 'Could not read this device. Check your PC and refresh.'; }
    finally { if (generation === this.generation) { this.busy = false; this.changed(this); } }
  }
  async revoke() {
    if (!this.authorized || this.busy || !this.record) return;
    const generation = this.generation;
    this.busy = true; this.message = 'Revoking this device…'; this.changed(this);
    try {
      const response = await this.request('/api/device/revoke', 'POST');
      if (generation !== this.generation) return;
      if (response.status !== 401 && (!response.ok || (await response.json()).revoked !== true)) throw new Error();
      if (generation === this.generation) this.revoked();
    } catch {
      // A lost acknowledgement is reconciled by a read, never another POST.
      try {
        const response = await this.request('/api/device');
        if (generation !== this.generation) return;
        if (response.status === 401) { this.revoked(); return; }
      } catch { /* Keep the uncertain outcome visible. */ }
      if (generation === this.generation) this.message = 'Revocation outcome unknown. Check access or revoke this phone on your PC. Nothing was retried.';
    } finally { if (generation === this.generation) { this.busy = false; this.changed(this); } }
  }
}
