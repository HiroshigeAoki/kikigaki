'use strict';
// Dev-only data toggle. Not part of the design under test; ?chrome=0 hides it for screenshots.
(() => {
  const params = new URLSearchParams(location.search);
  if (params.get('chrome') === '0') return;
  const { DATA, names } = window.__HARNESS__;
  const bar = document.createElement('div');
  bar.style.cssText = 'position:fixed;left:50%;bottom:8px;transform:translateX(-50%);display:flex;gap:2px;padding:2px;background:#ddd;border-radius:8px;font:12px system-ui;z-index:9999;box-shadow:0 1px 4px rgba(0,0,0,.2)';
  const go = (name) => { params.set('data', name); location.search = params.toString(); };
  for (const [name, label] of [['demo', 'Demo'], ['worst', 'Worst case'], ['empty', 'Empty'], ['one', 'One']]) {
    const b = document.createElement('button');
    b.textContent = label;
    b.style.cssText = `border:0;border-radius:6px;padding:3px 8px;font:inherit;cursor:pointer;background:${DATA === name ? '#fff' : 'transparent'}`;
    b.onclick = () => go(name);
    bar.append(b);
  }
  const sel = document.createElement('select');
  sel.style.font = 'inherit';
  for (const name of names) sel.append(new Option(name, name, false, name === DATA));
  sel.onchange = () => go(sel.value);
  bar.append(sel);
  document.body.append(bar);
})();
