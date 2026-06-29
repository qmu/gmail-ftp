// qfs dashboard — shell + preview→commit approval cards (tickets t51, t52).
//
// Vanilla JS, NO framework / bundler / CDN (the embedded binary is the deliverable; `qfs serve`
// must stay offline-clean). The shell composes a statement/path in the browser and posts it to the
// thin JSON bridge the binary serves over the SAME engine (and the SAME gate + guard) the CLI/MCP
// use:
//
//   POST /api/describe  { path }              -> the cred-free describe report (pure)
//   POST /api/run       { statement }         -> the dry-run plan preview (ZERO effects)
//   POST /api/commit    { statement, ack }    -> apply through the policy gate + irreversible guard
//
// t52 adds the approval cards: the preview renders the plan's effects (reversible / irreversible
// badges) as a card; Approve posts to /api/commit. A reversible in-policy plan applies; an
// out-of-policy plan is refused with the decision; an IRREVERSIBLE plan is NOT auto-applied — it
// raises a distinct second confirmation that posts `ack=true` (the same explicit acknowledgement the
// CLI's --commit-irreversible drives). The bridge never returns credential material — the card shows
// effect/metadata only (the secret-free dry-run summary + `<VERB> <driver>:<path>` labels).

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

// ---- t52: the preview → commit approval card --------------------------------------------------

/** Render an `Affected` estimate (`{exact}` / `{at_most}` / "unknown") as honest text. */
function affectedText(a) {
  if (a === "unknown" || a === null || a === undefined) return "?";
  if (typeof a === "object") {
    if ("exact" in a) return String(a.exact);
    if ("at_most" in a) return "<=" + a.at_most;
  }
  return String(a);
}

/** Render a target (`{driver, path}`) as `driver:/path` (no secrets — identity + location only). */
function targetText(t) {
  if (t && typeof t === "object") return (t.driver || "?") + ":" + (t.path || "?");
  return String(t);
}

/** Clear the approval-card region. */
function clearCard() {
  const card = document.getElementById("approval-card");
  card.innerHTML = "";
  card.classList.remove("show");
}

/** Render the commit OUTCOME (applied / policy-refused / needs-ack / error) under the card. */
function renderCommitResult(value, kind) {
  const out = document.getElementById("commit-out");
  out.classList.remove("ok", "err", "warn");
  out.classList.add(kind);
  out.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

/** POST the commit and render its structured outcome (the bridge already ran the gate + guard). */
async function postCommit(statement, ack) {
  renderCommitResult("committing…", "warn");
  let resp;
  try {
    resp = await fetch("/api/commit", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ statement: statement, ack: ack }),
    });
  } catch (e) {
    renderCommitResult("network error: " + (e.message || e), "err");
    return;
  }
  let payload;
  try {
    payload = await resp.json();
  } catch (_e) {
    renderCommitResult("the bridge returned a non-JSON response (HTTP " + resp.status + ")", "err");
    return;
  }
  if (payload && payload.applied === true) {
    clearCard();
    renderCommitResult(payload, "ok");
  } else if (payload && payload.refused) {
    // A legible refusal (policy_denied / needs_human_approval) — NOT an error; the card stays.
    renderCommitResult(payload, payload.refused === "needs_human_approval" ? "warn" : "err");
  } else {
    const err = (payload && payload.error) || {};
    const code = err.code ? err.code + ": " : "";
    renderCommitResult(code + (err.message || "commit failed (HTTP " + resp.status + ")"), "err");
  }
}

/** Build the approval card from a preview summary and wire Approve / Cancel (+ the irreversible ack). */
function renderApprovalCard(statement, preview) {
  const card = document.getElementById("approval-card");
  card.innerHTML = "";
  card.classList.add("show");

  const rows = (preview && preview.rows) || [];
  const irreversible = (preview && preview.irreversible) || [];
  const isPure = !preview || preview.is_pure || rows.length === 0;

  const title = document.createElement("h3");
  title.textContent = isPure ? "preview — reads only, 0 effects" : "preview — approve to commit";
  card.appendChild(title);

  if (isPure) {
    // A read-only / zero-effect plan: nothing to commit (matches the CLI's "pure query" wording).
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = "this statement reads only — there is nothing to apply.";
    card.appendChild(p);
    return;
  }

  const list = document.createElement("ul");
  list.className = "effect-rows";
  rows.forEach((r) => {
    const li = document.createElement("li");
    const badge = document.createElement("span");
    badge.className = "badge " + (r.irreversible ? "irreversible" : "reversible");
    badge.textContent = r.irreversible ? "irreversible" : "reversible";
    li.appendChild(badge);
    const text = document.createElement("span");
    text.className = "effect-text";
    text.textContent =
      " " + r.verb + " → " + targetText(r.target) + "  [affected " + affectedText(r.affected) + "]";
    li.appendChild(text);
    list.appendChild(li);
  });
  card.appendChild(list);

  const total = document.createElement("p");
  total.className = "muted total";
  total.textContent = "total affected: " + affectedText(preview.total_affected);
  card.appendChild(total);

  const hasIrreversible = irreversible.length > 0;
  const actions = document.createElement("div");
  actions.className = "card-actions";

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "secondary";
  cancel.textContent = "Cancel";
  cancel.addEventListener("click", () => {
    clearCard();
    renderCommitResult("cancelled — nothing was applied.", "warn");
  });

  const approve = document.createElement("button");
  approve.type = "button";
  approve.textContent = hasIrreversible ? "Approve…" : "Approve & commit";
  approve.addEventListener("click", () => {
    if (hasIrreversible) {
      // A distinct, explicit SECOND confirmation for irreversible effects — never auto-applied.
      renderIrreversibleConfirm(card, statement, irreversible.length);
    } else {
      postCommit(statement, false); // reversible in-policy → auto-commit (no ack needed).
    }
  });

  actions.appendChild(approve);
  actions.appendChild(cancel);
  card.appendChild(actions);
}

/** The one-time irreversible approval step: an explicit ack that posts `ack=true`. */
function renderIrreversibleConfirm(card, statement, count) {
  const warn = document.createElement("div");
  warn.className = "irreversible-confirm";
  const msg = document.createElement("p");
  msg.innerHTML =
    "<strong>" +
    count +
    " irreversible effect(s)</strong> (send / merge / delete) cannot be undone. " +
    "Confirm to acknowledge and commit.";
  warn.appendChild(msg);

  const confirm = document.createElement("button");
  confirm.type = "button";
  confirm.className = "danger";
  confirm.textContent = "Confirm irreversible commit";
  confirm.addEventListener("click", () => postCommit(statement, true)); // the explicit ack.
  warn.appendChild(confirm);
  card.appendChild(warn);
}

/** Preview the composed statement, then render the approval card from the dry-run summary. */
function wirePreviewCommit() {
  const form = document.getElementById("run-form");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    clearCard();
    renderCommitResult("", "ok");
    document.getElementById("commit-out").textContent = "";
    const statement = form.elements.statement.value.trim();
    render("run-out", "previewing…", false);
    try {
      const result = await postJson("/api/run", { statement: statement, mode: "preview" });
      render("run-out", result, false);
      renderApprovalCard(statement, result.preview);
    } catch (e) {
      render("run-out", e.message || String(e), true);
    }
  });
}

wirePreviewCommit();

// t53 admin views: each /sys quick-link fills the describe + preview forms with the chosen admin
// path and runs BOTH through the SAME bridge — no new endpoint, no admin capability the CLI lacks.
// Administration is also "everything is a path": the admin view is just describe/preview over a
// /sys/* path.
function wireAdminLinks() {
  const buttons = document.querySelectorAll(".sys-link");
  buttons.forEach((btn) => {
    btn.addEventListener("click", () => {
      const path = btn.getAttribute("data-path");
      const describePath = document.getElementById("describe-path");
      const statement = document.getElementById("run-statement");
      if (describePath) {
        describePath.value = path;
        document.getElementById("describe-form").requestSubmit();
      }
      if (statement) {
        statement.value = "FROM " + path + " |> SELECT *";
        document.getElementById("run-form").requestSubmit();
      }
    });
  });
}

wireAdminLinks();
