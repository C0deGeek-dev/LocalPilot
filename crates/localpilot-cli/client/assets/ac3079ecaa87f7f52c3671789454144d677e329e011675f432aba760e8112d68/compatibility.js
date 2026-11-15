// Wire versions, rather than product release numbers, decide compatibility.
export const protocol = Object.freeze({ min: 1, max: 1, required_capabilities: ['acks', 'resume'] });

export function compatibilityIssue(record, supported = protocol) {
  if (!Number.isSafeInteger(record.v)) return 'invalid';
  if (record.v > supported.max) return 'new-server';
  if (record.v < supported.min) return 'old-server';
  if (record.event?.type === 'attached' && (!Array.isArray(record.event.capabilities) ||
    !supported.required_capabilities.every((cap) => record.event.capabilities.includes(cap)))) return 'old-server';
  return null;
}

export const compatibilityMessages = {
  'new-server': 'This PC uses a newer protocol. Copy your drafts first. Close every LocalLink browser tab and close LocalLink from Android recent apps, then reopen to update the app. If it still appears, update LocalPilot and its bundled app together.',
  'old-server': 'This app needs a newer protocol or session capability. Update LocalPilot on the PC, then reconnect.',
  invalid: 'The PC returned an incompatible response. Update LocalPilot and its bundled app together.',
};
