function describeInPage(target) {
  const text = (v) => String(v || '').replace(/\s+/g, ' ').trim().slice(0, 200);
  let el = target || document.body;
  const activatable = 'button, a[href], input, select, textarea, summary, label, [role=button], [role=link], [role=menuitem], [role=tab], [role=switch], [role=checkbox], [role=radio], [role=option]';
  el = (el.closest && el.closest(activatable)) || el;
  if (el.tagName === 'LABEL' && el.control) el = el.control;
  const labelled = el.getAttribute('aria-labelledby');
  const byId = labelled ? labelled.split(/\s+/).map((id) => document.getElementById(id)?.textContent || '').join(' ') : '';
  const label = el.labels && el.labels.length ? Array.from(el.labels).map((l) => l.textContent).join(' ') : '';
  const form = el.form || (el.closest && el.closest('form'));
  return {
    tag: el.tagName.toLowerCase(),
    // A button's DOM type is its real state (an invalid attribute is 'submit'); #1218.
    type: el.tagName === 'BUTTON' ? el.type : (el.getAttribute('type') || ''),
    role: el.getAttribute('role') || '',
    name: text(el.getAttribute('aria-label') || byId || label || el.getAttribute('title') || el.getAttribute('alt') || ''),
    text: text(el.innerText || el.textContent || ''),
    value: el.tagName === 'INPUT' && /^(submit|button|reset)$/i.test(el.type) ? text(el.value) : '',
    inForm: !!form,
    formMethod: form ? (form.getAttribute('method') || 'get').toLowerCase() : '',
    formAction: form ? form.action : '',
    frameOrigin: location.origin,
  };
}
