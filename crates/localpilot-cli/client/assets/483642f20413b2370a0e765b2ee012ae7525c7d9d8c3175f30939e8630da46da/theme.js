export function mountTheme(document, window) {
  const select = document.getElementById('theme-preference');
  const valid = new Set(['system', 'light', 'dark']);
  const key = 'locallink-appearance';
  function apply(value) {
    const preference = valid.has(value) ? value : 'system';
    select.value = preference;
    if (preference === 'system') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.setAttribute('data-theme', preference);
    // Browser chrome follows the same palette as native controls and the page.
    for (const meta of document.querySelectorAll('meta[name="theme-color"]')) {
      const dark = preference === 'dark' || (preference === 'system' && meta.media.includes('dark'));
      meta.content = dark ? '#1c2422' : '#f5f3ec';
    }
  }
  try { apply(window.localStorage.getItem(key)); } catch { apply('system'); }
  select.addEventListener('change', () => {
    apply(select.value);
    try {
      if (select.value === 'system') window.localStorage.removeItem(key);
      else window.localStorage.setItem(key, select.value);
    } catch { /* The current page still honors the choice when storage is blocked. */ }
  });
  window.addEventListener('storage', (event) => { if (event.key === key || event.key === null) apply(event.newValue); });
}
