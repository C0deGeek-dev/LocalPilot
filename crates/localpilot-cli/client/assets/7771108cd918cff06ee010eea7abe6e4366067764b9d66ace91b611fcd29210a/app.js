// Navigation is local. A network connection alone never enables session controls.
const destinations = new Set(['sessions', 'attention', 'devices']);

export function destinationFromHash(hash) {
  const destination = hash.slice(1);
  return destinations.has(destination) ? destination : 'sessions';
}

export function showDestination(document, hash) {
  const destination = destinationFromHash(hash);
  for (const panel of document.querySelectorAll('[data-panel]')) {
    panel.hidden = panel.dataset.panel !== destination;
  }
  for (const link of document.querySelectorAll('[data-destination]')) {
    if (link.dataset.destination === destination) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
}

if (typeof document !== 'undefined') {
  showDestination(document, window.location.hash);
  window.addEventListener('hashchange', () => showDestination(document, window.location.hash));
}
