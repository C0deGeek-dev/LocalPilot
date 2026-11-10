import { Device } from './device.js';
import { Notifications } from './notifications.js';
import { keyedItems, setText } from './accessibility.js';

export function mountCompanion(document, window, sessions, checkAccess) {
  const el = (id) => document.getElementById(id);
  const make = (tag, text) => { const node = document.createElement(tag); node.textContent = text; return node; };
  const text = (id, value) => setText(el(id), value);
  let approvalInvoker;
  let notificationProjectsKey;
  const sheet = el('approval-sheet');
  const notifications = new Notifications({ browser: window, fetch: window.fetch.bind(window), changed: renderNotifications });
  const device = new Device({ fetch: window.fetch.bind(window), changed: renderDevice, revoked() { sessions.revoke(); device.access(false); notifications.access(false); void checkAccess(); } });
  function renderDevice() {
    text('device-status', device.authorized ? device.message || 'This phone’s access. Manage other devices on your PC with /remote devices.' : 'Pair this phone in Sessions to see its access.');
    el('device-list').replaceChildren();
    if (device.record) {
      const record = device.record;
      const item = make('li', `${record.name} · this device · ${record.scope === 'control' ? 'Control' : 'Observe only'} · ${record.this_workspace_only ? 'This project only' : 'All allowed projects'}`);
      el('device-list').append(item);
    }
    el('refresh-device').disabled = !device.authorized || device.busy;
    el('revoke-device').disabled = !device.record || device.busy;
    if (!device.authorized) el('revoke-confirm').hidden = true;
  }
  function renderNotifications() {
    el('notification-settings').hidden = !notifications.authorized;
    text('notification-status', notifications.supported ? notifications.message : 'Push notifications are unavailable in this browser. Use a supported browser over HTTPS; on iPhone, install the app to the Home Screen.');
    for (const id of ['enable-notifications', 'off-notifications', 'test-notification']) el(id).disabled = notifications.busy || !notifications.workspace || !notifications.supported;
    el('test-notification').disabled ||= !notifications.enabled;
    el('enable-notifications').textContent = notifications.enabled ? 'Re-enable notifications' : 'Enable notifications';
    const selected = el('notification-workspace').value;
    const projectsKey = JSON.stringify(sessions.workspaces);
    if (projectsKey !== notificationProjectsKey) {
      notificationProjectsKey = projectsKey;
      const options = sessions.workspaces.map((workspace) => { const option = make('option', workspace.label); option.value = workspace.id; return option; });
      const blank = make('option', 'Choose a project'); blank.value = '';
      el('notification-workspace').replaceChildren(blank, ...options);
    }
    el('notification-workspace').value = notifications.workspace || selected;
  }
  let attentionKey;
  function renderAttention() {
    const key = JSON.stringify([sessions.key, sessions.phase, sessions.asks, sessions.approval, sessions.canApprove]);
    if (key !== attentionKey) {
      attentionKey = key;
      text('attention-status', !sessions.authorized ? 'Pair this phone in Sessions.' : !sessions.selected ? 'Choose a shared session in Sessions to see its requests.' : `${sessions.workspace.label} / ${sessions.selected.name || 'Unnamed session'} · ${sessions.ready ? 'Connected in app' : 'View may be stale; approval controls locked. Answer waiting requests on the PC'}. Requests cover this attached session only.`);
      keyedItems(el('attention-list'), sessions.asks, (ask) => `${sessions.key}/${ask.ask_id}`, () => make('button', ''), (button, ask) => {
        setText(button, `${ask.tool || 'Request'} · ${ask.risk || 'Review required'}`);
        button.type = 'button';
        button.onclick = () => {
          approvalInvoker = button;
          sessions.openApproval(ask.ask_id); renderAttention();
          if (!sessions.approval) return;
          sheet.showModal(); el('approval-tool').focus();
        };
      });
      if (!sessions.asks.length) el('attention-list').append(make('p', sessions.ready ? 'No pending requests in this session.' : 'Connect to fetch current requests.'));
    }
    const ask = sessions.approval;
    if (!ask) { if (el('approval-sheet').open) el('approval-sheet').close(); return; }
    text('approval-project', `${sessions.workspace.label} / ${sessions.selected.name || 'Unnamed session'}`);
    text('approval-tool', ask.tool || 'Permission request');
    text('approval-detail', ask.detail || 'Details unavailable. Check the request on your PC.');
    text('approval-risk', `Risk: ${ask.risk || 'Not supplied'}`);
    text('approval-expiry', ask.expires_at_ms ? `Deadline: ${new Date(ask.expires_at_ms).toLocaleString()}${ask.expires_at_ms <= Date.now() ? ' · deadline passed; waiting for PC confirmation' : ''}` : 'Deadline not supplied by the PC.');
    const states = { pending: sessions.canApprove ? 'Review the exact request before answering.' : 'Controls locked. Reconnect and reconcile, or answer on your PC.', submitting: 'Submitting… Waiting for the PC acknowledgement.', resolved: ask.allow ? 'Resolved: allowed once by your PC.' : 'Resolved: denied by your PC.', elsewhere: 'Answered on another device or on the PC.', expired: 'Expired. This request can no longer be answered.', cancelled: 'Cancelled by the PC.', unavailable: 'This request is no longer available. Fetch current requests.', unknown: 'Outcome unknown. Check the PC before answering again. Nothing will be retried.', refused: 'The PC refused this answer.' };
    text('approval-status', `${states[ask.state]}${ask.message ? ` ${ask.message}` : ''}`);
    el('allow-approval').disabled = el('deny-approval').disabled = !sessions.canApprove || !ask.detail;
  }
  el('refresh-device').addEventListener('click', () => void device.load());
  el('revoke-device').addEventListener('click', () => { el('revoke-confirm').hidden = false; el('confirm-revoke').focus(); });
  el('cancel-revoke').addEventListener('click', () => { el('revoke-confirm').hidden = true; el('revoke-device').focus(); });
  el('confirm-revoke').addEventListener('click', () => { el('revoke-confirm').hidden = true; void device.revoke(); });
  el('notification-workspace').addEventListener('change', () => notifications.select(el('notification-workspace').value));
  el('enable-notifications').addEventListener('click', () => void notifications.enable());
  el('test-notification').addEventListener('click', () => void notifications.test());
  el('off-notifications').addEventListener('click', () => void notifications.off());
  el('allow-approval').addEventListener('click', () => sessions.answerApproval(true));
  el('deny-approval').addEventListener('click', () => sessions.answerApproval(false));
  el('close-approval').addEventListener('click', () => el('approval-sheet').close());
  sheet.addEventListener('close', () => {
    if (approvalInvoker?.isConnected && !approvalInvoker.closest('[hidden]')) approvalInvoker.focus();
    else if (!el('attention-title').closest('section').hidden) el('attention-title').focus();
  });
  const previousChanged = sessions.changed;
  sessions.changed = (model) => { previousChanged(model); device.access(model.authorized); notifications.access(model.authorized); renderAttention(); renderNotifications(); };
  const refetch = async () => {
    window.location.hash = 'attention';
    const confirmed = await checkAccess();
    if (confirmed && sessions.authorized && sessions.selected) sessions.attach(sessions.selected.id);
  };
  window.navigator.serviceWorker?.addEventListener('message', (event) => { if (event.data?.type === 'notification-open') void refetch(); });
  window.addEventListener('focus', () => { if (window.location.hash === '#attention') void refetch(); });
  document.addEventListener('visibilitychange', () => { if (!document.hidden && window.location.hash === '#attention') void refetch(); });
  // Deadline changes disable buttons even when no socket frame arrives.
  window.setInterval(renderAttention, 1000);
  renderDevice(); renderNotifications(); renderAttention();
  return { device, notifications };
}
