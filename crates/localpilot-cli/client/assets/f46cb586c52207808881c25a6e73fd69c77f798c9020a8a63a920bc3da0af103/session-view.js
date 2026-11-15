import { Sessions } from './session.js';
import { compatibilityMessages } from './compatibility.js';
import { reconcile, replyAnnouncer, keyedItems } from './accessibility.js';

const phaseMessages = {
  unpaired: 'Pair this phone to see your sessions.', choose: 'Choose a shared terminal session.',
  loading: 'Looking for shared sessions…', 'list-error': 'Could not list sessions. Check your PC and refresh.',
  offline: 'This project is offline. Start LocalPilot on your PC, then refresh.',
  connecting: 'Connecting to the terminal…', syncing: 'Waiting for the PC to report this session’s state. Controls are locked.',
  disconnected: 'Connection lost. The view may be out of date. Reconnect to fetch a fresh snapshot.',
  incompatible: 'This session uses an incompatible response. Update LocalPilot and LocalLink together.',
  refused: 'The PC refused this session. Check your access and whether the terminal is still sharing.',
  closed: 'Session closed. On your PC, resume this session to keep its history, or start a new one, then enable phone access.',
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
  let historyKeys = new Map();
  let nextHistoryKey = 0;
  let scrollKey;
  let wasAuthorized = false;
  let firstSnapshot = false;
  let followLatest = true;
  let forceLatest = false;
  const wideLayout = window.matchMedia('(min-width: 760px)');
  const scroller = el('conversation-scroll');
  const nearLatest = () => scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop < 80;
  function jumpLatest() {
    scroller.scrollTop = scroller.scrollHeight;
    followLatest = true;
    el('jump-latest').hidden = true;
  }
  scroller.addEventListener('scroll', () => {
    followLatest = nearLatest();
    el('jump-latest').hidden = followLatest;
  }, { passive: true });
  el('jump-latest').addEventListener('click', jumpLatest);
  // Phones: the session list opens as a sheet over the chat; closing it returns focus to the chat header.
  el('open-sessions').addEventListener('click', () => { el('session-browser').open = true; el('session-browser-title').focus(); });
  el('session-browser').addEventListener('toggle', () => {
    if (!el('session-browser').open && model.selected && !wideLayout.matches && el('session-browser').contains(document.activeElement)) el('open-sessions').focus();
  });
  el('session-browser').addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && model.selected && !wideLayout.matches && el('session-browser').open) { event.preventDefault(); el('session-browser').open = false; el('open-sessions').focus(); }
  });
  // Grow the composer with its draft (CSS caps the height) and keep the latest-messages button above it.
  const draft = el('session-draft');
  function fitDraft() {
    draft.style.height = 'auto';
    if (draft.value) draft.style.height = `${draft.scrollHeight + draft.offsetHeight - draft.clientHeight}px`;
  }
  draft.addEventListener('input', fitDraft);
  if (window.ResizeObserver) new window.ResizeObserver(([entry]) => {
    el('session-view').style.setProperty('--composer-height', `${entry.target.offsetHeight}px`);
  }).observe(document.querySelector('.composer-panel'));
  // Touch keyboards take half the screen; hide the bottom navigation while composing.
  const touch = window.matchMedia('(pointer: coarse)');
  // Restore it only after a tap on Send or Stop has completed, so the layout never shifts under the finger.
  const panel = document.querySelector('.composer-panel');
  let restoreNav = null;
  const composing = (on) => { window.clearTimeout(restoreNav); document.documentElement.classList.toggle('composing', on); };
  panel.addEventListener('focusin', (event) => {
    if (event.target === draft) { if (touch.matches) composing(true); }
    else restoreNav = window.setTimeout(() => composing(false), 400);
  });
  panel.addEventListener('focusout', (event) => { if (!panel.contains(event.relatedTarget)) { window.clearTimeout(restoreNav); restoreNav = window.setTimeout(() => composing(false), 400); } });
  wideLayout.addEventListener('change', () => { el('session-browser').open = wideLayout.matches || !model.selected; });
  const announceReply = replyAnnouncer(el('reply-announcement'));
  const model = new Sessions({
    origin: window.location.origin, fetch: window.fetch.bind(window), socket: (url) => new window.WebSocket(url),
    schedule: window.setTimeout.bind(window), unschedule: window.clearTimeout.bind(window),
    changed(state) {
      announceReply(state);
      if (!frame) frame = window.requestAnimationFrame(() => { frame = null; render(); });
    },
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
      code.setAttribute('role', 'group');
      code.setAttribute('aria-label', 'Code');
      code.append(make('code', match[1])); nodes.push(code);
      start = match.index + match[0].length;
    }
    if (start < text.length) nodes.push(make('pre', text.slice(start), 'message-text'));
    return nodes;
  }
  function renderTimeline() {
    const key = JSON.stringify([model.key, model.history, model.entries]);
    if (key === timelineKey) return;
    timelineKey = key;
    const timeline = el('timeline');
    const open = new Set([...timeline.querySelectorAll('details[open]')].map((node) => node.dataset.activity));
    const nodes = [];
    const occurrences = new Map();
    const nextKeys = new Map();
    for (const message of model.history) {
      const content = JSON.stringify([model.key, message.role, message.text]);
      const occurrence = occurrences.get(content) || 0;
      occurrences.set(content, occurrence + 1);
      const identity = JSON.stringify([content, occurrence]);
      const messageKey = historyKeys.get(identity) ?? ++nextHistoryKey;
      nextKeys.set(identity, messageKey);
      const article = make('article', null, `message ${message.role === 'user' ? 'user' : 'assistant'}`);
      article.dataset.key = `${model.key}/history/${messageKey}`;
      article.append(make('h3', message.role === 'assistant' ? 'LocalPilot' : message.role === 'user' ? 'You' : message.role), ...messageBody(message.text));
      nodes.push(article);
    }
    historyKeys = nextKeys;
    let group;
    model.entries.forEach((entry, index) => {
      if (entry.type !== 'tool') {
        group = null;
        if (!entry.text) return;
        const article = make('article', null, `message ${entry.type}`);
        article.dataset.key = `${model.key}/entry/${index}`;
        article.append(make('h3', entry.type === 'assistant' ? 'LocalPilot' : 'You'), ...messageBody(entry.text));
        if (entry.type === 'user') {
          const delivery = { sending: 'Sending…', accepted: 'Received by your PC', queued: 'Queued for the next turn', rejected: 'Not sent · draft kept below', unknown: 'Delivery unconfirmed · check your PC before resending' };
          article.dataset.delivery = entry.delivery || 'accepted';
          article.append(make('p', delivery[entry.delivery] || (entry.queued ? delivery.queued : delivery.accepted), 'delivery-status'));
        }
        nodes.push(article);
        return;
      }
      if (!group) {
        group = make('details', null, 'activity-group');
        group.dataset.activity = String(index);
        group.dataset.key = `${model.key}/entry/${index}`;
        group.open = open.has(String(index));
        group.append(make('summary', 'Tool activity'));
        nodes.push(group);
      }
      const tool = make('section', null, 'tool');
      tool.append(make('h3', `${entry.name} · ${entry.running ? 'Running' : entry.error ? 'Failed' : 'Finished'}`));
      if (entry.output) {
        const output = make('pre', entry.output, 'tool-output');
        output.tabIndex = 0;
        output.setAttribute('role', 'group');
        output.setAttribute('aria-label', `${entry.name} output`);
        tool.append(output);
      }
      group.append(tool);
    });
    if (!nodes.length) {
      const empty = make('p', 'No conversation text has been shared yet.', 'help');
      empty.dataset.key = 'empty'; nodes.push(empty);
    }
    reconcile(timeline, nodes);
  }
  function renderProjects() {
    const focused = document.activeElement;
    const focusedGroup = focused?.closest('.project-group');
    keyedItems(el('session-list'), [...model.projects.values()], (project) => project.workspace.id,
      () => {
        const group = make('details', null, 'project-group');
        const summary = make('summary');
        summary.append(make('span', '', 'project-label'), make('span', '', 'help project-id'), make('span', '', 'help project-state'));
        const content = make('div', null, 'project-content');
        const loading = make('div', null, 'session-skeleton');
        loading.setAttribute('aria-hidden', 'true');
        loading.append(make('i'), make('i'));
        const status = make('p', '', 'help project-status'); status.setAttribute('role', 'status');
        content.append(status, loading, make('div', null, 'session-list'), make('button', 'Refresh sessions', 'secondary'));
        group.append(summary, content);
        group.addEventListener('toggle', () => {
          const project = model.projects.get(group.dataset.key);
          if (group.open && project?.phase === 'idle') void model.loadProject(group.dataset.key);
        });
        return group;
      }, (group, project) => {
        const { workspace, phase, items } = project;
        const set = (selector, value) => { const node = group.querySelector(selector); if (node.textContent !== value) node.textContent = value; };
        set('.project-label', workspace.label);
        set('.project-id', `Project ID: ${workspace.id}`);
        set('.project-state', `${workspace.online && phase !== 'offline' ? 'Online' : 'Offline'}${model.workspace?.id === workspace.id && model.selected ? ' · Current session' : ''}`);
        const messages = {
          idle: 'Open this project to load shared sessions.', loading: 'Loading shared sessions…',
          offline: 'This project is offline. Start LocalPilot on your PC, then refresh projects.',
          throttled: 'Too many requests. Wait a moment, then try again.',
          forbidden: 'Access to this project needs checking.',
          error: 'Could not load shared sessions. Check your connection and try again.',
          ready: items.length ? '' : 'No shared sessions. Open LocalPilot in this project on your PC and enable phone access.',
        };
        set('.project-status', messages[phase]);
        group.querySelector('.session-skeleton').hidden = phase !== 'loading';
        group.querySelector('.session-list').setAttribute('aria-busy', String(phase === 'loading'));
        const refresh = group.querySelector('.project-content > button');
        refresh.hidden = phase === 'offline';
        refresh.disabled = phase === 'loading' || !!model.inventoryBusy;
        refresh.textContent = phase === 'idle' ? 'Load sessions' : phase === 'forbidden' ? 'Check access' : ['throttled', 'error'].includes(phase) ? 'Try again' : 'Refresh sessions';
        refresh.onclick = () => phase === 'forbidden' ? el('check-connection').click() : void model.loadProject(workspace.id);
        keyedItems(group.querySelector('.session-list'), items, (item) => item.id,
          () => { const button = make('button', '', 'secondary session-choice'); button.append(make('strong'), make('span')); return button; }, (button, item) => {
            const current = model.workspace?.id === workspace.id && model.selected?.id === item.id;
            const name = item.name || 'Unnamed session';
            const duplicate = items.some((other) => other.id !== item.id && (other.name || 'Unnamed session') === name);
            const label = `${name}${duplicate ? ` · ${item.id}` : ''} · ${item.live ? 'Shared now' : 'No longer shared'}${current ? ' · Current session' : ''}`;
            if (button.children[0].textContent !== name) button.children[0].textContent = name;
            const metadata = label.slice(name.length).replace(/^ · /, '');
            if (button.children[1].textContent !== metadata) button.children[1].textContent = metadata;
            button.disabled = !item.live || phase !== 'ready' || !workspace.online;
            button.setAttribute('aria-pressed', String(current));
            button.onclick = () => {
              model.openProjectSession(workspace.id, item.id);
              if (model.workspace?.id !== workspace.id || model.selected?.id !== item.id) return;
              forceLatest = true; render(); el('session-browser').open = wideLayout.matches; el('session-name').focus({ preventScroll: true });
            };
          });
      });
    if (focusedGroup && !focused?.isConnected) {
      if (focusedGroup.isConnected) focusedGroup.querySelector('summary').focus();
      else el('session-browser-title').focus();
    }
  }
  function render() {
    const paired = model.authorized;
    const viewing = paired && !!model.selected;
    el('sessions-title').closest('section').setAttribute('aria-labelledby', viewing ? 'session-name' : paired ? 'workspace-title' : 'sessions-title');
    el('main').classList.toggle('viewing-session', viewing);
    el('main').classList.toggle('has-projects', paired);
    el('session-empty').hidden = !paired || viewing;
    if (paired && !wasAuthorized) el('session-browser').open = true;
    wasAuthorized = paired;
    if (scrollKey !== model.key) { scrollKey = model.key; firstSnapshot = false; followLatest = true; }
    el('pair-card').hidden = paired;
    for (const id of ['sessions-title', 'sessions-intro', 'sessions-eyebrow']) el(id).hidden = paired;
    el('session-browser').hidden = !paired;
    el('session-view').hidden = !model.selected || !paired;
    setText('session-browser-title', viewing ? 'Switch session' : 'Choose a shared session');
    el('refresh-projects').disabled = !!model.inventoryBusy;
    setText('projects-status', model.inventoryIssue || (!model.workspaces.length && paired ? 'No projects are available to this phone. Check LocalPilot and this device’s access on your PC.' : ''));
    const phaseMessage = model.phase === 'incompatible' ? compatibilityMessages[model.compatibility || 'invalid'] : phaseMessages[model.phase];
    setText('sessions-status', phaseMessage || 'Shared terminal connected.');
    renderProjects();
    if (!model.selected) return;
    setText('session-name', model.selected.name || 'Unnamed session');
    setText('session-project', model.projectLabel());
    setText('session-pc', `PC address: ${window.location.host}`);
    setText('stop-project', `Stop in ${model.projectLabel()} / ${model.selected.name || 'Unnamed session'}`);
    const status = model.ready ? model.turn === 'unknown' ? 'Turn state unknown · controls locked' : model.busy ? 'Working' : 'Ready' : phaseMessage;
    el('timeline').classList.toggle('stale', model.viewStale || !model.ready);
    el('timeline').setAttribute('aria-busy', String(model.viewStale || !model.ready));
    setText('session-state', status);
    el('session-state').closest('header').classList.toggle('loading-state', !model.ready);
    setText('last-sync', model.synced ? `Last sync ${new Date(model.synced).toLocaleTimeString()}` : 'Waiting for first snapshot');
    setText('session-access', model.scope === 'control' ? 'Control access' : model.scope === 'observe' ? 'Observe only' : 'Access scope unconfirmed · controls locked');
    const notice = model.actionNotice && ['Input accepted by your PC.', 'Follow-up queued by your PC.'].includes(model.notice) ? '' : model.notice;
    setText('session-notice', [...new Set([model.outcomeIssue, notice].filter(Boolean))].join(' '));
    el('reconnect-session').hidden = !(['disconnected', 'refused', 'incompatible'].includes(model.phase) || (model.phase === 'syncing' && model.baselineReceived));
    el('back-to-sessions').hidden = model.phase !== 'closed';
    el('pending-attention').hidden = !model.asks.length;
    setText('pending-attention', `${model.asks.length} request${model.asks.length === 1 ? '' : 's'} need attention. Open Attention to review.`);
    setText('history-limit', model.historyUnconfirmed ? 'Conversation may be incomplete. Waiting for a full snapshot from the PC.' : model.truncated ? 'Only the most recent conversation text is available.' : '');
    el('session-plan').hidden = !model.plan.length;
    const planStatus = { pending: 'Pending', in_progress: 'In progress', completed: 'Completed', done: 'Done', skipped: 'Skipped' };
    el('plan-steps').replaceChildren(...model.plan.map((step) => make('li', `${step.title} · ${planStatus[step.status] || 'Status unavailable'}`)));
    const loading = ['connecting', 'syncing'].includes(model.phase) && !model.synced && !model.history.length && !model.entries.length;
    el('session-loading').hidden = !loading;
    el('timeline').hidden = loading;
    renderTimeline();
    const sending = [...model.pending.values()].some((pending) => pending.command.type === 'prompt');
    const runningTool = model.entries.findLast((entry) => entry.type === 'tool' && entry.running);
    const activity = !model.ready || model.viewStale ? '' : model.asks.length ? 'Waiting for your approval' : model.busy ?
      runningTool ? `Running ${runningTool.name}…` : model.entries.at(-1)?.type === 'assistant' && model.entries.at(-1).text ? 'Responding…' : 'Thinking…' :
      sending ? 'Sending message…' : model.awaitingReply ? 'Waiting for LocalPilot…' : '';
    el('turn-activity').hidden = !activity;
    el('turn-activity').classList.toggle('waiting-approval', !!model.asks.length);
    setText('turn-activity-text', activity);
    if (loading) scroller.scrollTop = 0;
    else if (forceLatest || (!firstSnapshot && model.ready && !model.viewStale) || followLatest) jumpLatest();
    if (model.ready && !model.viewStale) firstSnapshot = true;
    forceLatest = false;
    el('jump-latest').hidden = followLatest || loading;
    el('session-composer').setAttribute('aria-busy', String(sending));
    setText('composer-help', !model.ready ? 'Waiting for the session. Sending is locked.' : model.scope === 'observe' ? 'Observe only · send messages from your PC.' : sending ? 'Waiting for your PC to confirm delivery.' : model.busy ? 'Follow-ups are queued after the current turn.' : window.matchMedia('(pointer: coarse)').matches ? 'Send a message to this session.' : 'Enter to send · Shift+Enter for a new line.');
    if (el('session-draft').value !== model.draft) { el('session-draft').value = model.draft; fitDraft(); }
    el('session-draft').disabled = model.scope !== 'control';
    el('send-input').disabled = !model.canSubmit || !model.draft.trim();
    setText('send-input', model.draft.trim().startsWith('/') ? 'Run command' : model.busy ? 'Follow up' : 'Send');
    el('stop-turn').disabled = !model.canStop;
    const commands = model.ready && model.scope === 'control' ? model.commands || [] : [];
    el('command-picker').hidden = !commands.length;
    const nextCommandsKey = JSON.stringify([commands, model.canSubmit, model.busy]);
    if (nextCommandsKey === commandsKey) return;
    commandsKey = nextCommandsKey;
    keyedItems(el('command-list'), commands, (command) => command.name, () => {
      const button = make('button', '', 'secondary command-choice');
      button.append(make('strong'), make('span'));
      return button;
    }, (button, command) => {
      const label = `/${command.name}${command.args === 'required' ? ' …' : ''}`;
      if (button.children[0].textContent !== label) button.children[0].textContent = label;
      if (button.children[1].textContent !== command.description) button.children[1].textContent = command.description;
      button.disabled = !model.canSubmit || (model.busy && command.name !== 'effort');
      button.onclick = () => {
        model.setDraft(`/${command.name}${command.args === 'none' ? '' : ' '}`);
        el('command-picker').open = false;
        render(); el('session-draft').focus();
      };
    });
  }
  el('refresh-projects').addEventListener('click', () => el('check-connection').click());
  el('reconnect-session').addEventListener('click', () => model.attach(model.selected.id));
  el('back-to-sessions').addEventListener('click', () => { el('session-browser').open = true; el('session-browser-title').focus(); });
  el('session-draft').addEventListener('input', () => { model.setDraft(el('session-draft').value); render(); });
  el('session-composer').addEventListener('submit', (event) => { event.preventDefault(); forceLatest = true; model.submit(el('session-draft').value); });
  el('session-draft').addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !window.matchMedia('(pointer: coarse)').matches) {
      event.preventDefault();
      if (model.canSubmit) el('session-composer').requestSubmit();
    }
  });
  window.addEventListener('hashchange', () => {
    if (window.location.hash === '#sessions' && model.selected) window.requestAnimationFrame(() => { if (followLatest) jumpLatest(); });
  });
  el('stop-turn').addEventListener('click', () => model.stop());
  el('session-check-access').addEventListener('click', () => el('check-connection').click());
  window.addEventListener('offline', () => model.fail('disconnected'));
  window.addEventListener('pagehide', () => model.close('disconnected'));
  window.addEventListener('pageshow', () => model.emit());
  render();
  return model;
}
