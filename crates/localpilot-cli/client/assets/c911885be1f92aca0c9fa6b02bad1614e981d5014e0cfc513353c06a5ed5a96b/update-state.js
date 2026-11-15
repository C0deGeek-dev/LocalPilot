// One-use, per-tab handoff during an app update. Never stores session responses,
// pairing codes, cookies or commands, and never replays input after reload.
const key = 'locallink-update-drafts';
const sessionKey = /^[a-f0-9]{16}\/[a-f0-9-]{36}$/i;

export function saveUpdateState(browser, sessions) {
  const drafts = [...sessions.drafts].filter(([id, text]) => sessionKey.test(id) && typeof text === 'string' && text.length <= 65536).slice(-32);
  const active = sessionKey.test(sessions.key || '') ? sessions.key : null;
  const uncertain = sessions.pending.size > 0 || !!sessions.outcomeIssue;
  try {
    browser.sessionStorage.setItem(key, JSON.stringify({ drafts, active, uncertain }));
    return true;
  } catch {
    // A draft must never be silently discarded when tab storage is blocked.
    return !drafts.some(([, text]) => text.trim()) && !uncertain;
  }
}

export function takeUpdateState(browser) {
  try {
    const raw = browser.sessionStorage.getItem(key);
    browser.sessionStorage.removeItem(key);
    if (!raw) return null;
    const state = JSON.parse(raw);
    if (!Array.isArray(state.drafts) || state.drafts.length > 32 ||
        state.drafts.some((item) => !Array.isArray(item) || item.length !== 2 || !sessionKey.test(item[0]) || typeof item[1] !== 'string' || item[1].length > 65536) ||
        (state.active !== null && !sessionKey.test(state.active)) || typeof state.uncertain !== 'boolean') return null;
    return state;
  } catch { return null; }
}
