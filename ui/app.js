/* Chronicle — the window.
 *
 * Deliberately small and framework-free. Every string shown here was formatted
 * in Rust; this file arranges elements and forwards keystrokes, and holds no
 * opinion about dates, durations or fidelity.
 */

const invoke = window.__TAURI__.core.invoke;

const state = {
  cards: [],
  selected: null,
  detail: null,
  unchecked: new Set(), // keys the user has deliberately turned off
  query: "",
  busy: false,
  recording: false,
};

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
};

// ── loading ───────────────────────────────────────────────────────────

async function loadSessions(preserveChecks = false) {
  try {
    state.cards = state.query
      ? await invoke("search_sessions", { query: state.query })
      : await invoke("list_sessions", { limit: 60 });
  } catch (e) {
    state.cards = [];
    console.error(e);
  }
  if (!state.cards.some((c) => c.id === state.selected)) {
    state.selected = state.cards.length ? state.cards[0].id : null;
  }
  renderSidebar();
  await loadDetail(preserveChecks);
}

async function loadDetail(preserveChecks = false) {
  if (state.selected === null) {
    state.detail = null;
    renderDetail();
    return;
  }
  try {
    state.detail = await invoke("get_session", { id: state.selected });
  } catch (e) {
    state.detail = null;
    console.error(e);
  }
  // A background refresh must not undo the boxes the user just unticked.
  if (!preserveChecks) state.unchecked.clear();
  renderDetail();
}

/// Pick up sessions the recorder has written since the window opened, without
/// disturbing the selection, the scroll position, or the restore checkboxes.
async function refreshQuiet() {
  if (state.busy || document.querySelector(".scrim:not([hidden])")) return;
  const before = JSON.stringify(state.cards.map((c) => [c.id, c.title, c.duration, c.live]));
  const wasSelected = state.selected;

  let cards;
  try {
    cards = state.query
      ? await invoke("search_sessions", { query: state.query })
      : await invoke("list_sessions", { limit: 60 });
  } catch (e) {
    return;
  }
  if (JSON.stringify(cards.map((c) => [c.id, c.title, c.duration, c.live])) === before) return;

  state.cards = cards;
  if (!cards.some((c) => c.id === state.selected)) {
    state.selected = cards.length ? cards[0].id : null;
  }
  renderSidebar();
  // Only re-read the detail when it can actually have changed.
  const live = cards.find((c) => c.id === state.selected)?.live;
  if (state.selected !== wasSelected || live) await loadDetail(true);
}

async function loadStatus() {
  try {
    const s = await invoke("get_status");
    const was = state.recording;
    state.recording = s.recording;
    $("recdot").className = "dot" + (s.recording ? " on" : "");
    $("recstate").textContent = s.recording
      ? "Recording" + (s.currentSession ? " · " + s.currentSession : "")
      : "Not recording";
    $("dbline").textContent = s.database;
    $("countline").textContent =
      `${s.sessions} sessions · ${s.artifacts} artifacts · ${s.sizeMb.toFixed(1)} MB · ${s.retentionDays}-day retention`;
    // The empty state says different things depending on this, so redraw it.
    if (was !== s.recording && !state.detail) renderDetail();
  } catch (e) {
    $("recstate").textContent = "unavailable";
  }
}

// ── sidebar ───────────────────────────────────────────────────────────

function renderSidebar() {
  const nav = $("sidebar");
  nav.replaceChildren();

  if (!state.cards.length) {
    const p = el("div", "dayhead", state.query ? "Nothing matched" : "No sessions yet");
    nav.append(p);
    return;
  }

  let lastDay = null;
  for (const c of state.cards) {
    if (c.dayKey !== lastDay) {
      nav.append(el("div", "dayhead", c.dayHeading));
      lastDay = c.dayKey;
    }

    const row = el("button", "srow" + (c.id === state.selected ? " on" : ""));
    row.type = "button";
    row.dataset.id = c.id;

    const title = el("span", "st");
    title.append(el("span", "name", c.title));
    if (c.interrupted) title.append(el("span", "badge crit", "Interrupted"));
    else if (c.live) title.append(el("span", "badge good", "Recording"));
    if (c.pinned) title.append(el("span", "badge", "Pinned"));

    const meta = el("span", "sm", `${c.timeRange} · ${c.duration}`);

    const tiles = el("span", "tiles");
    for (const t of c.tiles) tiles.append(el("span", `tile cat-${t.category}`, t.text));

    row.append(title, meta, tiles);
    row.addEventListener("click", () => select(c.id));
    nav.append(row);
  }
}

function select(id) {
  if (state.selected === id) return;
  state.selected = id;
  renderSidebar();
  loadDetail();
  const row = document.querySelector(`.srow[data-id="${id}"]`);
  if (row) row.scrollIntoView({ block: "nearest" });
}

function move(delta) {
  if (!state.cards.length) return;
  const i = state.cards.findIndex((c) => c.id === state.selected);
  const next = Math.min(state.cards.length - 1, Math.max(0, (i < 0 ? 0 : i) + delta));
  select(state.cards[next].id);
}

// ── detail ────────────────────────────────────────────────────────────

function renderDetail() {
  const root = $("detail");
  root.replaceChildren();
  const d = state.detail;

  if (!d) {
    const box = el("div", "empty");

    if (state.cards.length) {
      box.append(el("h2", null, "Nothing selected"));
      box.append(el("p", null, "Pick a session on the left to see what you had open."));
    } else if (state.recording) {
      box.append(el("h2", null, "Nothing to show yet"));
      box.append(el("p", null,
        "Chronicle is watching. A session appears here once you have spent about four minutes with three different things open — a glance at one window does not count as work."));
    } else {
      box.append(el("h2", null, "Chronicle is not recording"));
      box.append(el("p", null,
        "Nothing is being observed, so nothing new will appear here. Start the recorder and carry on working."));
      const c = el("p");
      c.append(document.createTextNode("Run "), el("code", null, "chronicled run"));
      box.append(c);
    }
    root.append(box);
    return;
  }

  // Header
  const head = el("div", "dhead");
  const h1 = el("h1");
  h1.append(el("span", "title", d.title));
  if (d.pinned) h1.append(el("span", "badge", "Pinned"));
  head.append(h1);

  const sub = el("div", "dsub");
  const bits = [d.dayHeading, d.timeRange, `${d.duration} active`];
  bits.forEach((b, i) => {
    if (i) sub.append(el("span", "sep", "|"));
    sub.append(el("span", null, b));
  });
  if (d.endNote) {
    sub.append(el("span", "sep", "|"));
    sub.append(el("span", d.interrupted ? "crit" : "", d.endNote));
  }
  head.append(sub);
  root.append(head);

  // The shape of the session
  if (d.bands.length) {
    const tl = el("div", "tl");
    const bar = el("div", "tlbar");
    for (const b of d.bands) {
      const seg = el("span", `cat-${b.category}`);
      seg.style.flex = String(b.flex);
      bar.append(seg);
    }
    const rule = el("div", "tlrule");
    for (const r of d.ruler) rule.append(el("span", null, r));
    tl.append(bar, rule);
    root.append(tl);
  }

  // Context, grouped the way you would describe it out loud
  const ctx = el("div", "ctx");
  for (const g of d.groups) {
    const grp = el("div", "grp");
    grp.append(el("div", "grphead", g.label));

    for (const item of g.items) {
      const row = el("div", "item" + (item.actionable ? "" : " off"));

      const box = document.createElement("input");
      box.type = "checkbox";
      box.checked = item.actionable && !state.unchecked.has(item.key);
      box.disabled = !item.actionable;
      box.title = item.actionable ? "Include in restore" : "Chronicle cannot restore this one";
      box.addEventListener("change", () => {
        if (box.checked) state.unchecked.delete(item.key);
        else state.unchecked.add(item.key);
        updateActionBar();
      });

      row.append(box, el("span", `ico cat-${item.category}`, item.monogram));

      const txt = el("div", "itxt");
      txt.append(el("b", null, item.name));
      if (item.detail) txt.append(el("span", "path", item.detail));
      for (const line of item.lines.slice(0, 4)) txt.append(el("span", "meta", line));
      if (item.lines.length > 4) {
        txt.append(el("span", "meta", `+ ${item.lines.length - 4} more`));
      }
      if (item.note) txt.append(el("span", "note", item.note));
      row.append(txt);

      row.append(el("span", `fid ${item.fidelity}`, item.fidelityLabel));
      grp.append(row);
    }
    ctx.append(grp);
  }
  root.append(ctx);

  // Action bar
  const bar = el("div", "actionbar");
  bar.append(el("span", "abmeta"));
  const forget = el("button", "btn", "Forget last hour");
  forget.addEventListener("click", () => openScrim("forgetScrim"));
  const restore = el("button", "btn primary");
  restore.id = "restoreBtn";
  restore.append(document.createTextNode("Restore Workspace"), el("span", "kbd", "↵"));
  restore.addEventListener("click", doRestore);
  bar.append(forget, restore);
  root.append(bar);

  updateActionBar();
}

function selectedKeys() {
  if (!state.detail) return [];
  const keys = [];
  for (const g of state.detail.groups) {
    for (const i of g.items) {
      if (i.actionable && !state.unchecked.has(i.key)) keys.push(i.key);
    }
  }
  return keys;
}

function updateActionBar() {
  const d = state.detail;
  if (!d) return;
  const chosen = selectedKeys().length;
  const needs = d.total - d.ready;
  const meta = document.querySelector(".abmeta");
  if (meta) {
    meta.replaceChildren();
    meta.append(el("b", null, `${chosen} of ${d.total}`));
    meta.append(document.createTextNode(needs ? ` selected · ${needs} need you` : " selected"));
  }
  const btn = $("restoreBtn");
  if (btn) btn.disabled = chosen === 0 || state.busy;
}

// ── restore ───────────────────────────────────────────────────────────

async function doRestore() {
  if (!state.detail || state.busy) return;
  const keys = selectedKeys();
  if (!keys.length) return;

  state.busy = true;
  updateActionBar();
  try {
    const r = await invoke("restore_session", { id: state.detail.id, keys });
    showReceipt(r);
  } catch (e) {
    showReceipt({ restored: 0, total: keys.length, seconds: 0, rows: [{ label: "Restore failed", ok: false, message: String(e), fidelityLabel: "" }] });
  } finally {
    state.busy = false;
    updateActionBar();
  }
}

function showReceipt(r) {
  $("receiptTiming").textContent = `${r.restored} of ${r.total} · ${r.seconds.toFixed(1)}s`;
  const body = $("receiptBody");
  body.replaceChildren();
  for (const row of r.rows) {
    const line = el("div", "rline");
    line.append(el("span", "tick " + (row.ok ? "ok" : "no"), row.ok ? "✓" : "!"));
    line.append(el("span", null, row.label));
    line.append(el("span", "msg", row.ok ? row.fidelityLabel.toLowerCase() : row.message));
    body.append(line);
  }
  $("receiptNote").textContent =
    r.restored < r.total ? "Anything not restored is listed above with the reason." : "Undo is not implemented yet.";
  openScrim("receiptScrim");
}

// ── sources ───────────────────────────────────────────────────────────

// Sources lists the apps that are on this machine, not Chronicle's whole
// catalogue. A settings screen full of software the user does not have buries
// the one row they came to change.
let sourcesShowAll = false;

async function openSources() {
  const list = await invoke("list_sources", { all: sourcesShowAll });
  const body = $("sourcesBody");
  body.replaceChildren();

  $("sourcesAll").textContent = sourcesShowAll
    ? "Show installed only"
    : "Show all known apps";
  const installedCount = list.filter((s) => s.installed).length;
  $("sourcesNote").textContent = sourcesShowAll
    ? "Everything Chronicle recognises, installed here or not."
    : `${installedCount} apps on this machine. Password managers and sign-in prompts can never be enabled.`;

  // Installed first, grouped by category. The rest are the hard-denied rules
  // for software that is not here — worth being able to see, not worth putting
  // between the user and the app they opened this panel to change.
  const here = list.filter((s) => s.installed);
  const elsewhere = list.filter((s) => !s.installed);

  let lastCat = null;
  for (const s of here.concat(elsewhere)) {
    const heading = s.installed ? s.categoryLabel : "Not installed here";
    if (heading !== lastCat) {
      body.append(el("div", "seghead", heading));
      lastCat = heading;
    }
    const row = el("div", "togrow");
    const name = el("div", "name");
    name.append(el("b", null, s.displayName));
    if (s.autoDenied && s.policy === "ignore") {
      name.append(el("span", null, "never recorded, whatever this says"));
    } else if (s.autoDenied && s.configured) {
      name.append(el("span", null, "you turned this on; the default is off"));
    } else if (s.autoDenied) {
      name.append(el("span", null, "off by default"));
    }

    row.append(name);

    const seg = el("div", "seg");
    for (const [value, label] of [["full", "Full"], ["titles_off", "Titles off"], ["ignore", "Ignore"]]) {
      const b = el("button", null, label);
      b.type = "button";
      if (s.policy === value) b.className = value === "ignore" ? "off" : "on";
      b.addEventListener("click", async () => {
        await invoke("set_source_policy", {
          appId: s.appId,
          policy: value,
          aliases: s.aliases ?? [],
        });
        openSources();
      });
      seg.append(b);
    }
    row.append(seg);
    body.append(row);
  }
  openScrim("sourcesScrim");
}

$("sourcesAll").addEventListener("click", () => {
  sourcesShowAll = !sourcesShowAll;
  openSources();
});

// ── scrims ────────────────────────────────────────────────────────────

function openScrim(id) { $(id).hidden = false; }
function closeScrims() {
  for (const s of document.querySelectorAll(".scrim")) s.hidden = true;
}

document.addEventListener("click", (e) => {
  const t = e.target.closest("[data-close]");
  if (t) closeScrims();
  if (e.target.classList.contains("scrim")) closeScrims();
});

$("sourcesBtn").addEventListener("click", openSources);
$("forgetConfirm").addEventListener("click", async () => {
  await invoke("forget_hours", { hours: 1 });
  closeScrims();
  await loadSessions();
  await loadStatus();
});

// ── keyboard ──────────────────────────────────────────────────────────

let searchTimer = null;
$("q").addEventListener("input", (e) => {
  state.query = e.target.value.trim();
  clearTimeout(searchTimer);
  searchTimer = setTimeout(loadSessions, 140);
});

document.addEventListener("keydown", async (e) => {
  const typing = e.target.tagName === "INPUT";

  if (e.key === "Escape") {
    if (!document.querySelector(".scrim:not([hidden])")) {
      $("q").value = "";
      state.query = "";
      loadSessions();
    }
    closeScrims();
    $("q").blur();
    return;
  }

  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
    e.preventDefault();
    $("q").focus();
    $("q").select();
    return;
  }

  if (document.querySelector(".scrim:not([hidden])")) return;

  if (e.key === "ArrowDown") { e.preventDefault(); move(1); return; }
  if (e.key === "ArrowUp") { e.preventDefault(); move(-1); return; }

  if (e.key === "Enter" && !typing) { e.preventDefault(); doRestore(); return; }
  if (e.key === "Enter" && typing && state.cards.length) { e.preventDefault(); $("q").blur(); return; }

  if (typing) return;

  if (e.key === "F2" && state.detail) {
    const name = prompt("Rename this session", state.detail.title);
    if (name && name.trim()) {
      await invoke("rename_session", { id: state.detail.id, title: name.trim() });
      await loadSessions();
    }
    return;
  }

  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "p" && state.detail) {
    e.preventDefault();
    await invoke("set_pinned", { id: state.detail.id, pinned: !state.detail.pinned });
    await loadSessions();
  }
});

// ── go ────────────────────────────────────────────────────────────────

loadStatus();
loadSessions();
setInterval(() => { loadStatus(); refreshQuiet(); }, 15000);
