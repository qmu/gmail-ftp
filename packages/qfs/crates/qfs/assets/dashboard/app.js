// qfs dashboard — shell behaviour (ticket t51).
//
// Vanilla JS, NO framework / bundler / CDN (the embedded binary is the deliverable; `qfs serve`
// must stay offline-clean). The shell only composes a statement/path in the browser and posts it to
// the thin JSON bridge the binary serves over the SAME engine the CLI/MCP use:
//
//   POST /api/describe  { path }       -> the cred-free describe report (pure)
//   POST /api/run       { statement }  -> the dry-run plan preview (ZERO effects)
//
// There is no commit path here BY DESIGN: this slice is preview/read only (commit is t52's gated
// card). The bridge never returns credential material — describe is pure and preview is a secret-
// free plan summary — so nothing sensitive ever reaches this script.

"use strict";

/** POST a JSON body to `path` and return the parsed JSON (or throw with a legible message). */
async function postJson(path, body) {
  const resp = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  let payload;
  try {
    payload = await resp.json();
  } catch (_e) {
    throw new Error("the bridge returned a non-JSON response (HTTP " + resp.status + ")");
  }
  if (!resp.ok) {
    const err = payload && payload.error ? payload.error : {};
    const code = err.code ? err.code + ": " : "";
    throw new Error(code + (err.message || "request failed (HTTP " + resp.status + ")"));
  }
  return payload;
}

/** Render a result (or error) into the named <pre> output, toggling the ok/err border. */
function render(outId, value, isError) {
  const out = document.getElementById(outId);
  out.classList.remove("ok", "err");
  out.classList.add(isError ? "err" : "ok");
  out.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

function wire(formId, outId, build, path) {
  const form = document.getElementById(formId);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    render(outId, "running…", false);
    try {
      const result = await postJson(path, build(form));
      render(outId, result, false);
    } catch (e) {
      render(outId, e.message || String(e), true);
    }
  });
}

wire(
  "describe-form",
  "describe-out",
  (form) => ({ path: form.elements.path.value.trim() }),
  "/api/describe"
);

wire(
  "run-form",
  "run-out",
  (form) => ({ statement: form.elements.statement.value.trim(), mode: "preview" }),
  "/api/run"
);
