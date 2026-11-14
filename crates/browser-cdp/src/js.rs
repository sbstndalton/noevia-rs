//! The functions run in the page. Each takes the element as its first argument.

/// core server/browser-executor.cjs `describeInPage`, copied verbatim (its comments stay in the
/// executor's source): the facts of the element that will really act.
pub(crate) const DESCRIBE_IN_PAGE: &str = r#"function describeInPage(target) {
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
}"#;

/// core browser-executor.cjs's submit step, verbatim.
pub(crate) const SUBMIT: &str =
    "(el) => { const f = el.form || el.closest('form') || el; f.requestSubmit ? f.requestSubmit() : f.submit(); }";

pub(crate) const IS_CONNECTED: &str = "(el) => el.isConnected";
pub(crate) const IS_FOCUSED: &str = "(el) => el === document.activeElement";
pub(crate) const FOCUS: &str = "(el) => { el.focus(); }";

/// Scrolled into view: the centre and size of its box in viewport pixels, and what receives the
/// pointer there: `''` the element itself (or one inside it, open shadow roots included),
/// `'outside'` nothing (off the viewport), else a short description of the element in the way.
pub(crate) const ACTION_POINT: &str = "(el) => {
  if (!el.isConnected) throw new Error('Element is not attached to the DOM');
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  const out = { x, y, width: r.width, height: r.height, hit: '' };
  if (!(r.width > 0 && r.height > 0)) return out;
  const root = el.getRootNode();
  let hit = (typeof root.elementFromPoint === 'function' ? root : el.ownerDocument).elementFromPoint(x, y);
  while (hit && hit.shadowRoot) {
    const inner = hit.shadowRoot.elementFromPoint(x, y);
    if (!inner || inner === hit) break;
    hit = inner;
  }
  if (!hit) { out.hit = 'outside'; return out; }
  for (let n = hit; n; n = n.parentNode || n.host) if (n === el) return out;
  const tag = hit.tagName.toLowerCase();
  const id = hit.id ? ' id=\"' + hit.id + '\"' : '';
  const cls = typeof hit.className === 'string' && hit.className ? ' class=\"' + hit.className + '\"' : '';
  out.hit = '<' + tag + id + cls + '>\u{2026}</' + tag + '>';
  return out;
}";

/// Focused and emptied, ready for `Input.insertText` (`'type'`), or, for inputs Playwright sets
/// rather than types into (dates, colours, ranges), set at once (`'set'`).
pub(crate) const FILL: &str = "(el, value) => {
  if (!el.isConnected) throw new Error('Element is not attached to the DOM');
  const tag = el.tagName;
  if (tag === 'INPUT') {
    const type = String(el.type || '').toLowerCase();
    const direct = ['color', 'date', 'time', 'datetime-local', 'month', 'range', 'week'];
    const typed = ['', 'email', 'number', 'password', 'search', 'tel', 'text', 'url'];
    if (!direct.includes(type) && !typed.includes(type)) throw new Error('Input of type \"' + type + '\" cannot be filled');
    if (el.disabled || el.readOnly) throw new Error('Element is not editable');
    el.focus();
    if (direct.includes(type)) {
      el.value = value;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return 'set';
    }
    el.value = '';
  } else if (tag === 'TEXTAREA') {
    if (el.disabled || el.readOnly) throw new Error('Element is not editable');
    el.focus();
    el.value = '';
  } else if (el.isContentEditable) {
    el.focus();
    el.textContent = '';
  } else {
    throw new Error('Element is not an <input>, <textarea> or [contenteditable] element');
  }
  el.dispatchEvent(new Event('input', { bubbles: true }));
  return 'type';
}";

/// The option whose value, else whose label, is `value`; then `input` and `change`.
pub(crate) const SELECT_OPTION: &str = "(el, value) => {
  if (!el.isConnected) throw new Error('Element is not attached to the DOM');
  if (el.tagName !== 'SELECT') throw new Error('Element is not a <select> element');
  if (el.disabled) throw new Error('Element is not enabled');
  const options = Array.from(el.options);
  const option = options.find((o) => o.value === value) || options.find((o) => o.label === value);
  if (!option) throw new Error('No option with value or label \"' + value + '\"');
  for (const o of options) o.selected = o === option;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
}";

/// `document.querySelector(selector)`.
pub(crate) fn query(selector: &str) -> String {
    format!("document.querySelector({})", quoted(selector))
}

/// The `innerText` of the first match, `null` while there is none.
pub(crate) fn inner_text(selector: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector({}); if (!el) return null; if (!(el instanceof HTMLElement)) throw new Error('Node is not an HTMLElement'); return {{ text: el.innerText }}; }})()",
        quoted(selector)
    )
}

fn quoted(text: &str) -> String {
    js_json::wtf16::stringify(&js_json::JValue::Str(text.to_string()))
        .unwrap_or_else(|| "\"\"".into())
}
