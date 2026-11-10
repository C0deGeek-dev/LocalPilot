// The gateway binds the socket. This client never sends attach, hello or status.
import { compatibilityIssue, protocol } from './compatibility.js';
const terminalReplies = new Set(['accepted', 'queued', 'command_result', 'rejected', 'error']);
const unknownOutcome = 'Outcome unknown. Check the session on your PC before sending again. Nothing will be retried automatically.';

function normalizeAsk(ask) {
  if (!ask || typeof ask !== 'object') throw new Error('ask');
  // RPC zero means no deadline; live events omit it while snapshots include it.
  const expires = ask.expires_at_ms === undefined ? 0 : ask.expires_at_ms;
  if (!Number.isSafeInteger(expires) || expires < 0) throw new Error('ask deadline');
  return { ...ask, expires_at_ms: expires };
}

export class Sessions {
  constructor({ origin, fetch, socket, changed, schedule, unschedule, now = () => Date.now(), timeout = 15000, supported = protocol }) {
    Object.assign(this, { origin, fetch, socket, changed, schedule, unschedule, now, timeout });
    this.supported = supported;
    this.generation = 0;
    this.nextId = 0;
    this.snapshotWait = 0;
    this.scope = null;
    this.authorized = false;
    this.workspaces = [];
    this.projects = new Map();
    this.items = [];
    this.workspace = null;
    this.selected = null;
    this.drafts = new Map();
    this.pending = new Map();
    this.phase = 'unpaired';
    this.notice = '';
    this.actionNotice = false;
    this.outcomeIssue = '';
    this.resetView();
  }
  resetView() {
    this.approval = null;
    this.history = [];
    this.entries = [];
    this.plan = [];
    this.asks = [];
    this.turn = 'unknown';
    this.busy = false;
    this.queued = 0;
    this.synced = null;
    this.capabilities = [];
    this.commands = [];
    this.catalogueId = null;
    this.epoch = null;
    this.seq = -1;
    this.truncated = false;
    this.viewStale = false;
    this.baselineReceived = false;
    this.historyUnconfirmed = false;
    this.unknownAsksConfirmed = false;
    this.compatibility = null;
    this.serverVersion = null;
  }
  emit() { this.changed(this); }
  get key() { return this.selected && `${this.workspace.id}/${this.selected.id}`; }
  projectLabel(workspace = this.workspace) {
    if (!workspace) return 'Project';
    return `${workspace.label} · ${workspace.id}`;
  }
  get draft() { return this.drafts.get(this.key) || ''; }
  setDraft(text) { if (this.key) this.drafts.set(this.key, text); }
  get ready() { return this.phase === 'ready'; }
  get canControl() { return this.ready && this.scope === 'control' && this.turn !== 'unknown' && !this.uncertain; }
  get canSubmit() { return this.canControl && ![...this.pending.values()].some((p) => p.command.type !== 'cancel'); }
  get canStop() { return this.canControl && this.busy && ![...this.pending.values()].some((p) => p.command.type === 'cancel'); }
  access(scope, workspaces) {
    this.authorized = true;
    this.scope = ['control', 'observe'].includes(scope) ? scope : null;
    this.workspaces = workspaces.filter((item) => item && /^[a-f0-9]{16}$/.test(item.id) && typeof item.label === 'string' && typeof item.online === 'boolean');
    // A verified access refresh invalidates in-flight inventory reads, but does
    // not switch the attended session or fetch every project's session list.
    this.projects = new Map(this.workspaces.map((workspace) => {
      const previous = this.projects.get(workspace.id);
      return [workspace.id, { workspace, items: workspace.online ? previous?.items || [] : [],
        phase: !workspace.online ? 'offline' : previous?.phase === 'ready' ? 'ready' : 'idle', sequence: 0 }];
    }));
    if (this.phase === 'unpaired') this.phase = 'choose';
    const current = this.workspaces.find((item) => item.id === this.workspace?.id);
    if (current) this.workspace = current;
    if (this.workspace && !this.workspaces.some((item) => item.id === this.workspace.id && item.online)) {
      this.close('offline');
      this.viewStale = true;
    }
    this.emit();
  }
  revoke() {
    this.authorized = false;
    this.scope = null;
    this.workspaces = [];
    this.projects.clear();
    this.close('unpaired');
    this.items = [];
    this.workspace = null;
    this.selected = null;
    this.drafts.clear();
    this.notice = '';
    this.outcomeIssue = '';
    this.uncertain = false;
    this.resetView();
    this.emit();
  }
  async loadProject(workspaceId) {
    const project = this.projects.get(workspaceId);
    if (!this.authorized || !project?.workspace.online || project.phase === 'loading') return;
    project.phase = 'loading';
    project.items = [];
    const sequence = ++project.sequence;
    const current = () => this.authorized && this.projects.get(workspaceId) === project && project.sequence === sequence;
    this.emit();
    try {
      const response = await this.fetch(`/api/sessions?workspace=${encodeURIComponent(workspaceId)}`, {
        credentials: 'same-origin', mode: 'same-origin', cache: 'no-store', redirect: 'error', signal: AbortSignal.timeout(this.timeout),
      });
      if (!current()) return;
      if (response.status === 401) { this.revoke(); this.accessLost?.(); return; }
      if (response.status === 403) {
        project.phase = 'forbidden';
        if (this.workspace?.id === workspaceId) this.fail('refused');
        this.emit(); this.accessLost?.(); return;
      }
      if (response.status === 429 || response.status === 503) {
        project.phase = response.status === 429 ? 'throttled' : 'offline';
        this.emit(); return;
      }
      if (!response.ok) throw new Error('listing');
      const data = await response.json();
      if (!current()) return;
      if (!Array.isArray(data.sessions) || data.sessions.some((item) => !item ||
        typeof item.id !== 'string' || !/^[a-f0-9-]{36}$/i.test(item.id) || typeof item.live !== 'boolean' ||
        (item.name !== undefined && item.name !== null && typeof item.name !== 'string'))) throw new Error('listing');
      project.items = data.sessions;
      project.phase = 'ready';
    } catch {
      if (!current()) return;
      project.phase = 'error';
    }
    this.emit();
  }
  openProjectSession(workspaceId, sessionId) {
    const project = this.projects.get(workspaceId);
    if (!this.authorized || !project?.workspace.online || project.phase !== 'ready' ||
      !project.items.some((item) => item.id === sessionId && item.live)) return;
    if (this.workspace?.id === workspaceId && this.selected?.id === sessionId && this.connection) return;
    const sameProject = this.workspace?.id === workspaceId;
    if (!sameProject) {
      this.close();
      this.selected = null;
      this.resetView();
    }
    this.workspace = project.workspace;
    this.items = project.items;
    this.attach(sessionId);
  }
  close(phase = 'disconnected') {
    ++this.generation;
    this.unschedule(this.attachTimer);
    for (const pending of this.pending.values()) {
      this.unschedule(pending.timer);
      if (pending.command.type === 'permission_reply' && this.approval?.ask_id === pending.command.ask_id) this.approval.state = 'unknown';
    }
    if (this.pending.size) {
      this.outcomeIssue = `${this.projectLabel()} / ${this.selected?.name || 'session'}: ${unknownOutcome}`;
      this.notice = this.outcomeIssue; this.uncertain = true;
    }
    this.pending.clear();
    const connection = this.connection;
    this.connection = null;
    connection?.close();
    this.phase = phase;
  }
  attach(id) {
    const selected = this.items.find((item) => item.id === id);
    if (!selected || !this.workspace?.online || !this.origin.startsWith('https://')) return;
    const sameSession = this.selected?.id === id;
    this.close('connecting');
    this.selected = selected;
    if (!sameSession) this.resetView();
    else {
      this.viewStale = true;
      this.baselineReceived = false;
      this.historyUnconfirmed = true;
      this.capabilities = [];
      this.commands = [];
      this.catalogueId = null;
      this.compatibility = null;
      this.serverVersion = null;
      this.epoch = null;
      this.seq = -1;
    }
    // A fresh snapshot may restore controls, but the unknown-outcome notice stays.
    this.uncertain = false;
    const generation = this.generation;
    const url = new URL('/api/socket', this.origin);
    url.protocol = 'wss:';
    url.searchParams.set('workspace', this.workspace.id);
    url.searchParams.set('session', id);
    this.emit();
    try {
      const connection = this.socket(url.href);
      this.connection = connection;
      connection.onmessage = ({ data }) => {
        if (generation !== this.generation) return;
        try { this.receive(JSON.parse(data)); }
        catch { this.fail('incompatible'); }
      };
      connection.onclose = connection.onerror = () => {
        if (generation !== this.generation) return;
        this.fail('disconnected');
      };
      this.waitForSnapshot();
    } catch { this.fail('disconnected'); }
  }
  fail(phase) { this.close(this.authorized ? phase : 'unpaired'); this.emit(); }
  waitForSnapshot() {
    this.unschedule(this.attachTimer);
    const generation = this.generation;
    const wait = ++this.snapshotWait;
    this.attachTimer = this.schedule(() => {
      if (generation === this.generation && wait === this.snapshotWait && !this.ready && !this.baselineReceived) this.fail('disconnected');
    }, this.timeout);
  }
  receive(record) {
    const issue = compatibilityIssue(record, this.supported);
    if (issue) { this.compatibility = issue; this.fail('incompatible'); return; }
    if (!record.event || typeof record.event.type !== 'string') throw new Error('protocol');
    const event = record.event;
    if (event.type === 'attached') {
      if (this.phase !== 'connecting' || event.session_id !== this.selected.id || !event.epoch ||
        !Array.isArray(event.capabilities)) throw new Error('attach');
      this.capabilities = event.capabilities;
      this.serverVersion = typeof event.server_version === 'string' ? event.server_version : null;
      this.epoch = event.epoch;
      this.phase = 'syncing';
    } else if (event.type === 'snapshot') {
      if (this.phase !== 'syncing' || typeof event.epoch !== 'string' || !event.epoch ||
        !Number.isSafeInteger(event.seq) || event.seq < 0 || !['running', 'idle', 'unknown'].includes(event.turn) ||
        typeof event.busy !== 'boolean' || !Number.isSafeInteger(event.queued) || event.queued < 0 ||
        !Array.isArray(event.history) || event.history.some((item) => typeof item.role !== 'string' || typeof item.text !== 'string') ||
        !Array.isArray(event.asks) || (event.plan !== undefined && event.plan !== null && (!Array.isArray(event.plan) ||
          event.plan.some((step) => typeof step.title !== 'string' || typeof step.status !== 'string'))) ||
        (event.turn_text !== undefined && event.turn_text !== null && typeof event.turn_text !== 'string')) throw new Error('snapshot');
      const asks = event.asks.map(normalizeAsk);
      this.seq = event.seq;
      // A newer mirror install can win while this connection handles a resync.
      // The complete verified snapshot owns its epoch, not the earlier notice.
      this.epoch = event.epoch;
      this.baselineReceived = true;
      this.turn = event.turn;
      this.busy = event.busy;
      this.queued = event.queued;
      if (event.turn === 'unknown') {
        this.viewStale = true;
        this.historyUnconfirmed = !event.history.length;
        if (event.history.length) this.history = event.history;
        // Empty mirror rebuilds cannot prove a previously shown ask vanished.
        this.unknownAsksConfirmed = asks.length > 0;
        if (this.unknownAsksConfirmed) { this.asks = asks; this.reconcileApproval(); }
        this.unschedule(this.attachTimer);
        this.emit(); return;
      }
      this.viewStale = false;
      this.historyUnconfirmed = false;
      this.clearActionNotice();
      this.history = event.history;
      this.entries = event.turn === 'running' && event.turn_text ? [{ type: 'assistant', text: event.turn_text }] : [];
      this.plan = event.plan || [];
      this.asks = asks;
      this.reconcileApproval();
      this.truncated = event.history_truncated || event.turn_text_truncated;
      this.phase = 'ready';
      this.unschedule(this.attachTimer);
      if (this.capabilities.includes('commands') && !this.catalogueId) this.readCommands();
    } else if (event.type === 'resync_required') {
      this.epoch = event.epoch;
      this.phase = 'syncing';
      this.viewStale = true;
      this.baselineReceived = false;
      this.waitForSnapshot();
    } else if (event.type === 'session_closed' || event.type === 'closed') {
      this.fail('closed'); return;
    } else if (record.id && this.pending.has(record.id) && terminalReplies.has(event.type)) {
      this.settle(record.id, event);
    } else if (event.type === 'commands' && record.id === this.catalogueId && this.ready) {
      if (!Array.isArray(event.items) || event.items.some((item) => typeof item.name !== 'string' ||
        !/^[a-z][a-z0-9_-]*$/.test(item.name) || typeof item.description !== 'string' ||
        !['none', 'optional', 'required'].includes(item.args) || item.scope !== 'control')) throw new Error('commands');
      this.commands = event.items;
    } else if (!record.id && (this.ready || (this.phase === 'syncing' && this.baselineReceived && ['turn_started', 'stopped'].includes(event.type))) && record.seq !== undefined && record.seq !== null) {
      if (!Number.isSafeInteger(record.seq) || record.seq <= this.seq) return;
      // Intentional feed filtering leaves gaps; a numeric gap alone is not loss.
      this.seq = record.seq;
      if (this.phase === 'syncing') {
        this.phase = 'ready'; this.viewStale = this.historyUnconfirmed; this.unschedule(this.attachTimer);
        // A non-empty baseline carries actual registry asks; an empty mirror
        // cannot authorise stale asks retained from before its interruption.
        if (!this.unknownAsksConfirmed) this.asks = [];
        this.reconcileApproval();
      }
      this.project(event);
      if (this.ready && this.capabilities.includes('commands') && !this.catalogueId) this.readCommands();
    } else if ((event.type === 'error' || event.type === 'rejected') && !this.ready) {
      this.fail('refused'); return;
    }
    if (this.ready) this.synced = this.now();
    this.emit();
  }
  readCommands() {
    this.commands = [];
    this.catalogueId = `read-${++this.nextId}`;
    this.connection.send(JSON.stringify({ v: 1, id: this.catalogueId, command: { type: 'commands' } }));
  }
  project(event) {
    switch (event.type) {
      case 'turn_started': this.turn = 'running'; this.busy = true; this.plan = []; this.entries.push({ type: 'assistant', text: '' }); break;
      case 'text_delta': {
        let entry = this.entries.at(-1);
        if (entry?.type !== 'assistant') { entry = { type: 'assistant', text: '' }; this.entries.push(entry); }
        entry.text += event.text;
        break;
      }
      case 'tool_started': this.entries.push({ type: 'tool', id: event.id, name: event.name, output: '', running: true }); break;
      case 'tool_finished': {
        const tool = this.entries.findLast((entry) => entry.type === 'tool' && entry.id === event.id);
        if (tool) Object.assign(tool, { running: false, output: event.output, error: event.is_error });
        else this.entries.push({ type: 'tool', id: event.id, name: event.name, output: event.output, error: event.is_error });
        break;
      }
      case 'plan': this.plan = event.steps; break;
      case 'stopped': this.turn = 'idle'; this.busy = false; this.clearActionNotice(); break;
      case 'permission_ask': this.asks = [...this.asks.filter((ask) => ask.ask_id !== event.ask_id), normalizeAsk(event)]; break;
      case 'permission_resolved':
        this.asks = this.asks.filter((ask) => ask.ask_id !== event.ask_id);
        if (this.approval?.ask_id === event.ask_id) {
          this.approval.resolution = event;
          if (this.approval.state !== 'submitting' && this.approval.state !== 'unknown') this.approval.state = event.outcome === 'expired' ? 'expired' : event.outcome === 'cancelled' ? 'cancelled' : this.approval.state === 'resolved' ? 'resolved' : 'elsewhere';
        }
        break;
      case 'asks_resync':
        if (!Array.isArray(event.asks)) throw new Error('asks');
        this.asks = event.asks.map(normalizeAsk); this.reconcileApproval(); break;
      case 'warning': this.notice = event.message; break;
      case 'quota_paused': this.notice = `Paused by the provider. Reset: ${event.reset}`; break;
    }
  }
  submit(text) {
    if (!this.canSubmit || !text.trim()) return;
    if (text.length > 65536) { this.notice = 'Keep input within 65,536 characters.'; this.emit(); return; }
    const value = text.trim();
    if (value.startsWith('/')) {
      const [, name, args = ''] = /^\/([^\s]+)\s*([\s\S]*)$/.exec(value) || [];
      const command = this.commands?.find((item) => item.name === name);
      if (!command || !this.capabilities.includes('commands')) { this.notice = 'Choose an available slash command. Slash text is never sent to the model.'; this.emit(); return; }
      if ((command.args === 'required' && !args.trim()) || (command.args === 'none' && args.trim())) {
        this.notice = command.args === 'required' ? `/${name} needs an argument.` : `/${name} takes no arguments.`;
        this.emit(); return;
      }
      if (this.busy && name !== 'effort') { this.notice = 'Wait for the turn to finish before running this command.'; this.emit(); return; }
      this.mutate({ type: 'run_command', name, args }, text);
    } else this.mutate({ type: 'prompt', text, disposition: this.busy ? 'follow_up' : 'immediate' }, text);
  }
  stop() { if (this.canStop) this.mutate({ type: 'cancel' }); }
  openApproval(id) {
    if ([...this.pending.values()].some((pending) => pending.command.type === 'permission_reply')) return;
    const ask = this.asks.find((ask) => ask.ask_id === id);
    if (!ask) return;
    if (this.approval?.ask_id === id && this.approval.key === this.key && !['unavailable', 'refused'].includes(this.approval.state)) return;
    this.approval = { ...ask, key: this.key, epoch: this.epoch, state: 'pending' };
    this.emit();
  }
  reconcileApproval() {
    const approval = this.approval;
    if (!approval || ['submitting', 'unknown', 'resolved', 'elsewhere', 'expired', 'cancelled', 'unavailable'].includes(approval.state)) return;
    const current = this.asks.find((ask) => ask.ask_id === approval.ask_id);
    if (approval.key !== this.key || !current || ['tool', 'detail', 'risk', 'expires_at_ms'].some((field) => current[field] !== approval[field])) approval.state = 'unavailable';
    else approval.epoch = this.epoch;
  }
  get canApprove() {
    const ask = this.approval;
    return !!ask && typeof ask.detail === 'string' && !!ask.detail && typeof ask.tool === 'string' && typeof ask.risk === 'string' &&
      Number.isSafeInteger(ask.expires_at_ms) && ask.expires_at_ms >= 0 && ask.state === 'pending' && this.canControl && ask.key === this.key && ask.epoch === this.epoch &&
      this.asks.some((item) => item.ask_id === ask.ask_id) && (!ask.expires_at_ms || ask.expires_at_ms > this.now()) &&
      ![...this.pending.values()].some((p) => p.command.type === 'permission_reply');
  }
  answerApproval(allow) {
    if (!this.canApprove || typeof allow !== 'boolean') return;
    this.approval.state = 'submitting';
    this.mutate({ type: 'permission_reply', ask_id: this.approval.ask_id, allow });
  }
  mutate(command, draft) {
    const id = `action-${++this.nextId}`;
    const pending = { command, draft, key: this.key };
    this.pending.set(id, pending);
    this.notice = command.type === 'cancel' ? 'Requesting Stop… Waiting for your PC.' : 'Submitting… Waiting for your PC.';
    this.actionNotice = false;
    pending.timer = this.schedule(() => { if (this.pending.has(id)) this.fail('disconnected'); }, this.timeout);
    try { this.connection.send(JSON.stringify({ v: 1, id, command })); }
    catch { this.fail('disconnected'); return; }
    this.emit();
  }
  settle(id, event) {
    const pending = this.pending.get(id);
    this.unschedule(pending.timer);
    this.pending.delete(id);
    if (pending.command.type === 'permission_reply') {
      const approval = this.approval;
      if (!approval || approval.ask_id !== pending.command.ask_id || approval.key !== pending.key) return;
      if (event.type === 'accepted') {
        approval.state = 'resolved';
        approval.allow = pending.command.allow;
        this.asks = this.asks.filter((ask) => ask.ask_id !== approval.ask_id);
      } else {
        approval.state = event.code === 'already_resolved' ? 'elsewhere' : event.code === 'expired' ? 'expired' :
          event.code === 'unknown_ask' ? 'unavailable' : ['outcome_unknown', 'in_progress'].includes(event.code) ? 'unknown' : 'refused';
        approval.message = event.message;
        if (approval.state === 'unknown') {
          this.uncertain = true;
          this.outcomeIssue = `${this.projectLabel()} / ${this.selected.name || 'session'}: Approval outcome unknown. Check the PC before answering again. Nothing will be retried.`;
        }
      }
      this.notice = '';
      return;
    }
    if (event.type === 'rejected' || event.type === 'error') {
      this.actionNotice = false;
      if (event.code === 'outcome_unknown' || event.code === 'in_progress') {
        this.outcomeIssue = `${this.projectLabel()} / ${this.selected.name || 'session'}: ${unknownOutcome}`;
        this.notice = this.outcomeIssue; this.uncertain = true;
      } else this.notice = `Not accepted: ${event.message}`;
      return;
    }
    if (pending.draft !== undefined && this.drafts.get(pending.key) === pending.draft) this.drafts.delete(pending.key);
    if (pending.command.type === 'prompt') {
      this.entries.push({ type: 'user', text: pending.command.text, queued: event.type === 'queued' });
    }
    this.actionNotice = true;
    this.notice = event.type === 'command_result' ? `/${event.name}: ${event.message || 'Completed.'}` :
      event.type === 'queued' && this.busy ? 'Follow-up queued by your PC.' : pending.command.type === 'cancel' ?
        this.busy ? 'Stop accepted. Waiting for the turn to end.' : 'Stop accepted by your PC.' : 'Input accepted by your PC.';
  }
  clearActionNotice() { if (this.actionNotice) { this.notice = ''; this.actionNotice = false; } }
}
