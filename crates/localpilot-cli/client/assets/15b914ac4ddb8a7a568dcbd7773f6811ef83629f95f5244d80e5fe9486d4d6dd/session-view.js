import { Sessions } from './session.js';

const phaseMessages = {
  unpaired: 'Pair this phone to see your sessions.', choose: 'Choose a shared terminal session.',
  loading: 'Looking for shared sessions…', 'list-error': 'Could not list sessions. Check your PC and refresh.',
  offline: 'This project is offline. Start LocalPilot on your PC, then refresh.',
  connecting: 'Connecting to the terminal…', syncing: 'Synchronising the session. Controls are locked.',
  disconnected: 'Connection lost. The view may be out of date. Reconnect to fetch a fresh snapshot.',
  incompatible: 'This session uses an incompatible response. Update LocalPilot and LocalLink together.',
  refused: 'The PC refused this session. Check your access and whether the terminal is still sharing.',
  closed: 'Session closed. Phone access ends when the shared terminal closes. Check the session on your PC.',
};

export function mountSessions(document, window) {
  const el = (id) => document.getElementById(id);
  const make = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined && text !== null) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
  let frame;
  let timelineKey;
  let commandsKey;
  let listKey;
  let workspaceKey;
  const model = new Sessions({
    origin: window.location.origin, fetch: window.fetch.bind(window), socket: (url) => new window.WebSocket(url),
    schedule: window.setTimeout.bind(window), unschedule: window.clearTimeout.bind(window),
    changed() { if (!frame) frame = window.requestAnimationFrame(() => { frame = null; render(); }); },
  });
  function setText(id, text) { if (el(id).textContent !== text) el(id).textContent = text; }
  function messageBody(text) {
    const nodes = [];
    const fences = /^```[^\n]*\n([\s\S]*?)^```[^\n]*$/gm;
    let start = 0;
    for (const match of text.matchAll(fences)) {
      if (match.index > start) nodes.push(make('pre', text.slice(start, match.index), 'message-text'));
      const code = make('pre', null, 'code-block');
      code.tabIndex = 0;
      code.setAttribute('aria-label', 'Code');
      code.append(make('code', match[1])); nodes.push(code);
      start = match.index + match[0].length;
    }
    if (start < text.length) nodes.push(make('pre', text.slice(start), 'message-text'));
    return nodes;
  }
  function renderTimeline() {
    const key = JSON.stringify([model.history, model.entries]);
    if (key === timelineKey) return;
    timelineKey = key;
    const timeline = el('timeline');
    const open = new Set([...timeline.querySelectorAll('details[open]')].map((node) => node.dataset.activity));
    const nodes = [];
    for (const message of model.history) {
      const article = make('article', null, 'message');
      article.append(make('h3', message.role === 'assistant' ? 'LocalPilot' : message.role === 'user' ? 'You' : message.role), ...messageBody(message.text));
      nodes.push(article);
    }
    let group;
    model.entries.forEach((entry, index) => {
      if (entry.type !== 'tool') {
        group = null;
        if (!entry.text) return;
        const article = make('article', null, `message ${entry.type}`);
        article.append(make('h3', entry.type === 'assistant' ? 'LocalPilot' : entry.queued ? 'Sent from this phone · follow-up' : 'Sent from this phone'), ...messageBody(entry.text));
        nodes.push(article);
        return;
      }
      if (!group) {
        group = make('details', null, 'activity-group');
        group.dataset.activity = String(index);
        group.open = open.has(String(index));
        group.append(make('summary', 'Tool activity'));
        nodes.push(group);
      }
      const tool = make('section', null, 'tool');
      tool.append(make('h3', `${entry.name} · ${entry.running ? 'Running' : entry.error ? 'Failed' : 'Finished'}`));
      if (entry.output) tool.append(make('pre', entry.output, 'tool-output'));
      group.append(tool);
    });
    if (!nodes.length) nodes.push(make('p', 'No conversation text has been shared yet.', 'help'));
    timeline.replaceChildren(...nodes);
  }
  function render() {
    const paired = model.authorized;
    const viewing = paired && !!model.selected;
    el('main').classList.toggle('viewing-session', viewing);
    el('pair-card').hidden = viewing;
    for (const id of ['sessions-title', 'sessions-intro', 'sessions-eyebrow']) el(id).hidden = viewing;
    el('session-browser').hidden = !paired;
    el('session-view').hidden = !model.selected || !paired;
    const nextWorkspaceKey = JSON.stringify([model.workspaces, model.workspace?.id]);
    if (nextWorkspaceKey !== workspaceKey) {
      workspaceKey = nextWorkspaceKey;
      const selectedWorkspace = el('session-workspace').value;
      const options = model.workspaces.map((item) => {
        const option = make('option', `${item.label}${item.online ? '' : ' · offline'}`);
        option.value = item.id;
        return option;
      });
      const blank = make('option', 'Choose a project'); blank.value = '';
      el('session-workspace').replaceChildren(blank, ...options);
      el('session-workspace').value = model.workspace?.id || selectedWorkspace;
    }
    el('refresh-sessions').disabled = !el('session-workspace').value || model.phase === 'loading';
    setText('sessions-status', phaseMessages[model.phase] || 'Shared terminal connected.');
    const nextListKey = JSON.stringify([model.items, model.selected?.id, model.phase === 'choose']);
    if (nextListKey !== listKey) {
      listKey = nextListKey;
      const list = model.items.map((item) => {
        const button = make('button', `${item.name || 'Unnamed session'} · ${item.live ? 'Shared now' : 'No longer shared'}`, 'secondary session-choice');
        button.disabled = !item.live;
        button.setAttribute('aria-pressed', String(model.selected?.id === item.id));
        button.addEventListener('click', () => { model.attach(item.id); el('session-browser').open = false; });
        return button;
      });
      if (!list.length && model.phase === 'choose' && model.workspace) list.push(make('p', 'No shared sessions. Open LocalPilot in this project on your PC and enable phone access.', 'help'));
      el('session-list').replaceChildren(...list);
    }
    if (!model.selected) return;
    setText('session-name', model.selected.name || 'Unnamed session');
    setText('session-project', model.workspace.label);
    setText('session-pc', `PC address: ${window.location.host}`);
    setText('stop-project', `Stop in ${model.workspace.label}`);
    const status = model.ready ? model.turn === 'unknown' ? 'Turn state unknown · controls locked' : model.busy ? 'Working' : 'Ready' : phaseMessages[model.phase];
    el('timeline').classList.toggle('stale', model.viewStale || !model.ready);
    el('timeline').setAttribute('aria-busy', String(model.viewStale || !model.ready));
    setText('session-state', status);
    setText('last-sync', model.synced ? `Last sync ${new Date(model.synced).toLocaleTimeString()}` : 'Waiting for first snapshot');
    setText('session-access', model.scope === 'control' ? 'Control access' : model.scope === 'observe' ? 'Observe only' : 'Access scope unconfirmed · controls locked');
    setText('session-notice', [...new Set([model.outcomeIssue, model.notice].filter(Boolean))].join(' '));
    el('reconnect-session').hidden = !['disconnected', 'refused', 'incompatible'].includes(model.phase);
    el('pending-attention').hidden = !model.asks.length;
    setText('pending-attention', `${model.asks.length} request${model.asks.length === 1 ? '' : 's'} need attention. Open Attention to review.`);
    setText('history-limit', model.truncated ? 'Only the most recent conversation text is available.' : '');
    el('session-plan').hidden = !model.plan.length;
    const planStatus = { pending: 'Pending', in_progress: 'In progress', completed: 'Completed', done: 'Done', skipped: 'Skipped' };
    el('plan-steps').replaceChildren(...model.plan.map((step) => make('li', `${step.title} · ${planStatus[step.status] || 'Status unavailable'}`)));
    renderTimeline();
    if (el('session-draft').value !== model.draft) el('session-draft').value = model.draft;
    el('session-draft').disabled = model.scope !== 'control';
    el('send-input').disabled = !model.canSubmit || !model.draft.trim();
    setText('send-input', model.draft.trim().startsWith('/') ? 'Run command' : model.busy ? 'Follow up' : 'Send');
    el('stop-turn').disabled = !model.canStop;
    const commands = model.ready && model.scope === 'control' ? model.commands || [] : [];
    el('command-picker').hidden = !commands.length;
    const nextCommandsKey = JSON.stringify([commands, model.canSubmit, model.busy]);
    if (nextCommandsKey === commandsKey) return;
    commandsKey = nextCommandsKey;
    el('command-list').replaceChildren(...commands.map((command) => {
      const button = make('button', null, 'secondary command-choice');
      button.append(make('strong', `/${command.name}${command.args === 'required' ? ' …' : ''}`), make('span', command.description));
      button.disabled = !model.canSubmit || (model.busy && command.name !== 'effort');
      button.addEventListener('click', () => {
        model.setDraft(`/${command.name}${command.args === 'none' ? '' : ' '}`);
        el('command-picker').open = false;
        render(); el('session-draft').focus();
      });
      return button;
    }));
  }
  el('session-workspace').addEventListener('change', () => void model.list(el('session-workspace').value));
  el('refresh-sessions').addEventListener('click', () => void model.list(el('session-workspace').value));
  el('reconnect-session').addEventListener('click', () => model.attach(model.selected.id));
  el('session-draft').addEventListener('input', () => { model.setDraft(el('session-draft').value); render(); });
  el('session-composer').addEventListener('submit', (event) => { event.preventDefault(); model.submit(el('session-draft').value); });
  el('stop-turn').addEventListener('click', () => model.stop());
  el('session-check-access').addEventListener('click', () => el('check-connection').click());
  window.addEventListener('offline', () => model.fail('disconnected'));
  window.addEventListener('pagehide', () => model.close('disconnected'));
  window.addEventListener('pageshow', () => model.emit());
  render();
  return model;
}
