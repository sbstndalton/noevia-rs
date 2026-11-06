(() => {
  const q = selector => document.querySelector(selector);
  return {
    input: q('#input')?.value.replaceAll('synthetic-secret', '{{secret:fixture}}') ?? null,
    trusted: q('#input')?.dataset.trusted ?? null,
    selected: q('#select')?.value ?? null,
    files: q('#file') ? Array.from(q('#file').files).map(f => f.name) : null,
    text: q('#text')?.innerText.replaceAll('synthetic-secret', '{{secret:fixture}}') ?? null,
    pathname: location.pathname
  };
})()
