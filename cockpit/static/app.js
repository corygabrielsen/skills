// ── State ──────────────────────────────────────────────────
const state = {
  runs: [], // RunSummary[] from /api/runs?status=all
  currentRunId: null,
  currentRun: null, // RunSnapshot
  selectedIteration: null,
  selectedBlob: null,
  blobText: null,
  stream: null,
  backoff: 1000,
  listTimer: null,
};

// ── DOM refs ───────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const topbar = $("topbar");
const topbarState = $("topbar-state");
const topbarDetail = $("topbar-detail");
const listEl = $("run-list");
const centerEl = $("center-pane");
const rightEl = $("right-pane");
const banner = $("banner");

// ── Helpers ────────────────────────────────────────────────
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k === "html") node.innerHTML = v;
    else if (k.startsWith("on") && typeof v === "function")
      node.addEventListener(k.slice(2), v);
    else if (v != null) node.setAttribute(k, v);
  }
  for (const c of children) {
    if (c == null) continue;
    if (typeof c === "string") node.appendChild(document.createTextNode(c));
    else node.appendChild(c);
  }
  return node;
}
function ageMs(d) {
  return Date.now() - new Date(d).getTime();
}
function shortAge(d) {
  const ms = ageMs(d);
  if (ms < 60_000) return `${Math.floor(ms / 1000)}s`;
  if (ms < 3600_000) return `${Math.floor(ms / 60_000)}m`;
  if (ms < 86400_000) return `${Math.floor(ms / 3600_000)}h`;
  return `${Math.floor(ms / 86400_000)}d`;
}
function setBanner(msg) {
  if (!msg) {
    banner.classList.remove("show");
    banner.textContent = "";
  } else {
    banner.textContent = msg;
    banner.classList.add("show");
  }
}
function targetLabel(snap) {
  if (snap.domain_view && snap.domain_view.domain === "pr") {
    return `${snap.domain_view.slug}#${snap.domain_view.pr}`;
  }
  return snap.run_id.slice(0, 24) + "…";
}
function shortRun(id) {
  // Run ids carry timestamp + entropy + pid. The PR target is
  // already the better label; for non-PR runs we keep the id.
  return id.length > 24 ? id.slice(0, 24) + "…" : id;
}

// ── API ────────────────────────────────────────────────────
async function listRuns() {
  const resp = await fetch("/api/runs?status=all&limit=100");
  if (!resp.ok) throw new Error(`/api/runs: ${resp.status}`);
  return resp.json();
}
async function getRun(id) {
  const resp = await fetch(`/api/runs/${encodeURIComponent(id)}`);
  if (resp.status === 404) throw new Error("unknown run");
  if (!resp.ok) throw new Error(`/api/runs/${id}: ${resp.status}`);
  return resp.json();
}
async function getBlobText(runId, sha) {
  const resp = await fetch(
    `/api/runs/${encodeURIComponent(runId)}/blobs/${sha}`,
  );
  if (!resp.ok) throw new Error(`/api/runs/${runId}/blobs/${sha}: ${resp.status}`);
  const ct = resp.headers.get("content-type") || "";
  const isText =
    ct.startsWith("text/") ||
    ct.startsWith("application/json") ||
    ct.startsWith("text/markdown");
  return { isText, text: isText ? await resp.text() : null, ct };
}

// ── Stream ─────────────────────────────────────────────────
function attachStream(runId) {
  if (state.stream) {
    state.stream.close();
    state.stream = null;
  }
  const es = new EventSource(`/api/runs/${encodeURIComponent(runId)}/events`);
  state.stream = es;
  es.addEventListener("open", () => {
    state.backoff = 1000;
    topbar.classList.remove("disconnected");
    topbar.classList.add("connected");
    topbarState.textContent = "live";
    setBanner("");
  });
  es.addEventListener("projected", (ev) => {
    let pe;
    try {
      pe = JSON.parse(ev.data);
    } catch {
      return;
    }
    applyProjected(pe);
    renderCenter();
    renderRight();
  });
  es.addEventListener("error", () => {
    es.close();
    state.stream = null;
    topbar.classList.add("disconnected");
    topbar.classList.remove("connected");
    topbarState.textContent = "reconnecting";
    setBanner(`Reconnecting in ${Math.round(state.backoff / 1000)}s…`);
    setTimeout(() => {
      if (state.currentRunId === runId) attachStream(runId);
    }, state.backoff);
    state.backoff = Math.min(state.backoff * 2, 30_000);
  });
}

function applyProjected(pe) {
  if (!pe || !pe.kind) return;
  switch (pe.kind) {
    case "snapshot":
      // Full snapshot replacement — preserves selectedIteration
      // if the iteration still exists in the new snapshot.
      state.currentRun = pe;
      // Also refresh the run-list entry's status/age fields
      // optimistically; the 10s list poll will reconcile.
      updateRunSummaryFromSnapshot(pe);
      renderLeft();
      break;
    case "iteration_update":
      if (!state.currentRun) return;
      upsertIteration(state.currentRun, pe);
      break;
    case "status_change":
      if (!state.currentRun) return;
      state.currentRun.status = pe.status;
      state.currentRun.outcome = pe.outcome;
      break;
    case "axis_update":
      // PR domain only; ignore on Other.
      if (
        !state.currentRun ||
        !state.currentRun.domain_view ||
        state.currentRun.domain_view.domain !== "pr"
      )
        return;
      state.currentRun.domain_view.axes[pe.axis] = pe.status;
      break;
    case "handoff_emitted":
      // Reflected in the next snapshot; no-op here.
      break;
    case "branch_divergence":
      if (
        state.currentRun &&
        state.currentRun.domain_view &&
        state.currentRun.domain_view.domain === "pr"
      ) {
        state.currentRun.domain_view.branch_divergence = pe;
      }
      break;
    default:
      // Forward-compat: unknown delta kinds are skipped.
      break;
  }
}
function upsertIteration(snap, pe) {
  // pe is the IterationSnapshot for iteration_update.
  const ix = snap.iterations.findIndex(
    (i) => i.iteration === pe.iteration,
  );
  if (ix >= 0) snap.iterations[ix] = pe;
  else snap.iterations.push(pe);
  snap.iterations.sort((a, b) => a.iteration - b.iteration);
}
function updateRunSummaryFromSnapshot(snap) {
  const ix = state.runs.findIndex((r) => r.run_id === snap.run_id);
  if (ix < 0) return;
  state.runs[ix].status = snap.status;
  state.runs[ix].latest_event_at = snap.latest_event_at;
  if (snap.outcome) {
    state.runs[ix].outcome_kind = snap.outcome.kind;
    state.runs[ix].exit_code = snap.outcome.exit_code;
  }
}

// ── Routing ────────────────────────────────────────────────
function parseRoute() {
  const hash = window.location.hash || "#/";
  const m = hash.match(/^#\/runs\/([^/]+)(?:\/iter\/(\d+))?$/);
  if (m) return { runId: decodeURIComponent(m[1]), iter: m[2] ? Number(m[2]) : null };
  return { runId: null, iter: null };
}
function navigate(path) {
  window.location.hash = path;
}
window.addEventListener("hashchange", routeChanged);

async function routeChanged() {
  const { runId, iter } = parseRoute();
  if (runId !== state.currentRunId) {
    state.currentRunId = runId;
    state.currentRun = null;
    state.selectedIteration = null;
    state.selectedBlob = null;
    state.blobText = null;
    if (state.stream) {
      state.stream.close();
      state.stream = null;
    }
    if (runId) {
      try {
        state.currentRun = await getRun(runId);
      } catch (e) {
        state.currentRun = null;
        setBanner(String(e));
      }
      attachStream(runId);
    }
  }
  state.selectedIteration = iter;
  if (iter == null) {
    state.selectedBlob = null;
    state.blobText = null;
  }
  renderAll();
}

// ── Selection ──────────────────────────────────────────────
function selectRun(runId) {
  navigate(`#/runs/${encodeURIComponent(runId)}`);
}
function selectIteration(iter) {
  const runId = state.currentRunId;
  if (!runId) return;
  navigate(`#/runs/${encodeURIComponent(runId)}/iter/${iter}`);
}
async function selectBlob(sha) {
  const runId = state.currentRunId;
  if (!runId) return;
  state.selectedBlob = sha;
  state.blobText = null;
  try {
    const { isText, text } = await getBlobText(runId, sha);
    state.blobText = isText
      ? text
      : "(binary blob; download via the link above)";
  } catch (e) {
    state.blobText = `error: ${e}`;
  }
  renderRight();
}

// ── Render ─────────────────────────────────────────────────
function renderAll() {
  renderTopbar();
  renderLeft();
  renderCenter();
  renderRight();
}
function renderTopbar() {
  if (state.currentRun) {
    topbarDetail.textContent = `${state.currentRun.iterations.length} iter · ${state.currentRun.status}`;
  } else {
    topbarDetail.textContent = `${state.runs.length} runs`;
  }
}
function renderLeft() {
  listEl.innerHTML = "";
  const active = state.runs.filter((r) => r.status === "active");
  const recent = state.runs.filter((r) => r.status !== "active");
  listEl.appendChild(renderGroup("Active", active));
  listEl.appendChild(renderGroup("Recent", recent.slice(0, 20)));
}
function renderGroup(label, items) {
  const wrap = el("div");
  wrap.appendChild(el("div", { class: "group-header" }, `${label} ${items.length}`));
  if (items.length === 0) {
    wrap.appendChild(el("div", { class: "pane-empty" }, "—"));
    return wrap;
  }
  for (const r of items) {
    const card = el(
      "a",
      {
        class: `run-card${r.run_id === state.currentRunId ? " selected" : ""}`,
        href: `#/runs/${encodeURIComponent(r.run_id)}`,
        onclick: (e) => {
          e.preventDefault();
          selectRun(r.run_id);
        },
      },
      el("div", { class: "target" }, summaryTarget(r)),
      el(
        "div",
        { class: "meta" },
        el("span", { class: `status-badge ${r.status}` }, r.status),
        el("span", null, shortAge(r.latest_event_at)),
      ),
    );
    wrap.appendChild(card);
  }
  return wrap;
}
function summaryTarget(r) {
  if (r.domain === "pr" && r.target && r.target.slug && r.target.pr) {
    return `pr ${r.target.slug}#${r.target.pr}`;
  }
  return shortRun(r.run_id);
}

function renderCenter() {
  centerEl.innerHTML = "";
  if (!state.currentRun) {
    centerEl.appendChild(el("div", { class: "pane-empty" }, "Select a run to view details."));
    return;
  }
  const snap = state.currentRun;
  centerEl.appendChild(el("h2", null, targetLabel(snap)));
  const started = new Date(snap.started_at).toLocaleString();
  centerEl.appendChild(
    el(
      "div",
      { class: "subtitle" },
      `${snap.domain} · started ${started} · `,
      el("span", { class: `status-badge ${snap.status}` }, snap.status),
    ),
  );

  // Iterations
  const iterSection = el("section", { class: "section" }, el("h3", null, "Iterations"));
  if (snap.iterations.length === 0) {
    iterSection.appendChild(el("div", { class: "pane-empty" }, "(none yet)"));
  } else {
    for (const i of snap.iterations) iterSection.appendChild(renderIterationRow(i));
  }
  centerEl.appendChild(iterSection);

  // Axes (PR domain only)
  if (snap.domain_view && snap.domain_view.domain === "pr") {
    const axes = snap.domain_view.axes || {};
    const rows = Object.entries(axes).filter(([, v]) => v != null);
    if (rows.length > 0) {
      const sec = el("section", { class: "section" }, el("h3", null, "Axes"));
      const tbl = el("table", { class: "axis-table" });
      const tbody = el("tbody");
      for (const [name, a] of rows) {
        tbody.appendChild(
          el(
            "tr",
            null,
            el("td", null, name),
            el("td", null, a.state_summary || "—"),
            el("td", null, a.current_candidate || ""),
          ),
        );
      }
      tbl.appendChild(tbody);
      sec.appendChild(tbl);
      centerEl.appendChild(sec);
    }
  }

  // Outcome
  if (snap.outcome) {
    const sec = el("section", { class: "section" }, el("h3", null, "Outcome"));
    sec.appendChild(
      el(
        "div",
        { class: "detail-row" },
        el("span", { class: "k" }, "kind"),
        el("span", null, snap.outcome.kind),
      ),
    );
    sec.appendChild(
      el(
        "div",
        { class: "detail-row" },
        el("span", { class: "k" }, "exit_code"),
        el("span", null, String(snap.outcome.exit_code)),
      ),
    );
    if (snap.outcome.headline) {
      sec.appendChild(
        el(
          "div",
          { class: "detail-row" },
          el("span", { class: "k" }, "headline"),
          el("span", null, snap.outcome.headline),
        ),
      );
    }
    centerEl.appendChild(sec);
  }
}
function renderIterationRow(i) {
  const inFlight = !i.executed && !i.handoff && i.waited_ms == null && i.decision_kind == null;
  const cls = [
    "iter-row",
    state.selectedIteration === i.iteration ? "selected" : "",
    inFlight ? "in-flight" : "",
    i.success === true ? "success" : "",
    i.success === false ? "fail" : "",
  ]
    .filter(Boolean)
    .join(" ");
  let icon = "·";
  if (i.executed && i.success === true) icon = "✓";
  else if (i.executed && i.success === false) icon = "✗";
  else if (i.handoff) icon = "⇄";
  else if (i.waited_ms != null) icon = "⏸";
  else if (inFlight) icon = "▶";
  const summary = el("span", { class: "summary" });
  summary.appendChild(document.createTextNode("Observe"));
  if (i.decision_kind) {
    summary.appendChild(el("span", { class: "arrow" }, " → "));
    summary.appendChild(document.createTextNode(i.decision_kind));
  }
  if (i.action_kind) {
    summary.appendChild(el("span", { class: "arrow" }, " · "));
    summary.appendChild(document.createTextNode(i.action_kind));
  }
  return el(
    "div",
    {
      class: cls,
      onclick: () => selectIteration(i.iteration),
    },
    el("span", { class: "num" }, String(i.iteration)),
    el("span", { class: "icon" }, icon),
    summary,
  );
}

function renderRight() {
  rightEl.innerHTML = "";
  if (!state.currentRun || state.selectedIteration == null) {
    rightEl.appendChild(
      el("div", { class: "pane-empty" }, "Select an iteration to inspect."),
    );
    return;
  }
  const i = state.currentRun.iterations.find(
    (x) => x.iteration === state.selectedIteration,
  );
  if (!i) {
    rightEl.appendChild(
      el("div", { class: "pane-empty" }, "Iteration not yet observed."),
    );
    return;
  }
  rightEl.appendChild(el("h3", null, `Iteration ${i.iteration}`));
  rightEl.appendChild(detailRow("decision", i.decision_kind || "—"));
  rightEl.appendChild(detailRow("action", i.action_kind || "—"));
  rightEl.appendChild(detailRow("executed", i.executed ? "yes" : "no"));
  if (i.success != null) {
    rightEl.appendChild(detailRow("success", i.success ? "yes" : "no"));
  }
  if (i.waited_ms != null) {
    rightEl.appendChild(detailRow("waited", `${i.waited_ms} ms`));
  }
  if (i.handoff) {
    rightEl.appendChild(detailRow("handoff", `${i.handoff.variant}: ${i.handoff.action_kind}`));
  }
  if (i.blob_refs && i.blob_refs.length > 0) {
    rightEl.appendChild(el("h3", { style: "margin-top:14px" }, "Blobs"));
    const ul = el("ul", { class: "blob-list" });
    for (const b of i.blob_refs) {
      const link = el(
        "a",
        {
          href: `/api/runs/${encodeURIComponent(state.currentRunId)}/blobs/${b.sha}`,
          target: "_blank",
          title: b.sha,
        },
        `${b.kind}.${b.ext}`,
      );
      const inline = el(
        "button",
        {
          style: "margin-left:8px; padding:1px 6px; font-size:11px",
          onclick: () => selectBlob(b.sha),
        },
        "view",
      );
      ul.appendChild(
        el(
          "li",
          null,
          link,
          el("span", { class: "blob-size" }, `${b.size} B`),
          inline,
        ),
      );
    }
    rightEl.appendChild(ul);
  }
  if (state.selectedBlob && state.blobText != null) {
    const r = el("div", { class: "blob-render" }, state.blobText);
    rightEl.appendChild(r);
  }
}
function detailRow(k, v) {
  return el("div", { class: "detail-row" }, el("span", { class: "k" }, k), el("span", null, v));
}

// ── Boot ───────────────────────────────────────────────────
async function refreshRunList() {
  try {
    const runs = await listRuns();
    state.runs = runs;
    renderLeft();
    renderTopbar();
  } catch (e) {
    setBanner(`run list: ${e}`);
  }
}
async function init() {
  await refreshRunList();
  await routeChanged();
  state.listTimer = setInterval(refreshRunList, 10_000);
}
init().catch((e) => setBanner(String(e)));
