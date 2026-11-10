// Reconcile rendered conversation nodes so streaming never replaces the reader's
// focused code block, expanded activity group, or earlier message.
export function reconcile(parent, nodes) {
  const previous = new Map([...parent.children].map((node) => [node.dataset.key, node]));
  const keys = new Set(nodes.map((node) => node.dataset.key));
  for (const child of [...parent.children]) if (!keys.has(child.dataset.key)) child.remove();
  for (const [index, fresh] of nodes.entries()) {
    const current = previous.get(fresh.dataset.key);
    let node = fresh;
    if (current?.tagName === fresh.tagName) {
      node = current;
      patch(node, fresh);
    }
    place(parent, node, index);
  }
  while (parent.children.length > nodes.length) parent.lastElementChild.remove();
}

function place(parent, node, index) {
  const before = parent.children[index] || null;
  if (before === node) return;
  if (parent.moveBefore && node.parentElement === parent) parent.moveBefore(node, before);
  else {
    const focused = node.contains(node.ownerDocument.activeElement) ? node.ownerDocument.activeElement : null;
    parent.insertBefore(node, before);
    focused?.focus({ preventScroll: true });
  }
}

function patch(node, fresh) {
  for (const attribute of [...node.attributes]) {
    if (!fresh.hasAttribute(attribute.name) && attribute.name !== 'open') node.removeAttribute(attribute.name);
  }
  for (const attribute of fresh.attributes) {
    if (attribute.name !== 'open' && node.getAttribute(attribute.name) !== attribute.value) node.setAttribute(attribute.name, attribute.value);
  }
  // Keep text-only elements and their text nodes stable between token updates.
  if (!fresh.children.length) {
    if (node.textContent !== fresh.textContent) {
      if (!node.children.length && node.firstChild?.nodeType === 3) node.firstChild.data = fresh.textContent;
      else node.textContent = fresh.textContent;
    }
    return;
  }
  // A streamed opening fence starts as plain text and later becomes a code
  // child. Remove obsolete text without replacing stable element children.
  for (const child of [...node.childNodes]) if (child.nodeType !== 1) child.remove();
  const children = [...fresh.children];
  for (const [index, child] of children.entries()) {
    const old = node.children[index];
    if (old?.tagName === child.tagName) patch(old, child);
    else if (old) old.replaceWith(child);
    else node.append(child);
  }
  while (node.children.length > children.length) node.lastElementChild.remove();
}

export function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

export function keyedItems(parent, items, keyFor, create, update) {
  const existing = new Map([...parent.children].map((node) => [node.dataset.key, node]));
  const nodes = items.map((item) => {
    const key = String(keyFor(item));
    const node = existing.get(key) || create(item);
    node.dataset.key = key;
    update(node, item);
    return node;
  });
  const retained = new Set(nodes);
  for (const child of [...parent.children]) if (!retained.has(child)) child.remove();
  for (const [index, node] of nodes.entries()) {
    place(parent, node, index);
  }
  while (parent.children.length > nodes.length) parent.lastElementChild.remove();
}

// Announce completed groups, never generated token text. A fresh attachment
// establishes a baseline rather than announcing an old reply as newly finished.
export function replyAnnouncer(node) {
  let key;
  let running = false;
  return (model) => {
    if (model.key !== key || !model.ready) {
      key = model.key;
      running = model.ready && model.busy;
      setText(node, '');
      return;
    }
    if (model.busy && !running) setText(node, '');
    if (running && !model.busy && model.turn === 'idle') setText(node, 'Reply finished. Read the conversation for the response and tool activity.');
    running = model.busy;
  };
}
