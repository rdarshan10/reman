// ---------- install boxes (all stay in sync) ----------
const INSTALL = {
  win: { cmd: "irm https://raw.githubusercontent.com/rdarshan10/reman/master/install.ps1 | iex", note: "Run in PowerShell. Windows 10 and 11, x64." },
  mac: { cmd: "curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh", note: "Apple Silicon Macs (M1 and later)." },
  linux: { cmd: "curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh", note: "x64 Linux, with bash, zsh or fish." },
};
function guessOS() {
  const p = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  if (/mac/i.test(p)) return "mac";
  if (/linux/i.test(p)) return "linux";
  return "win";
}
function setOS(os) {
  document.querySelectorAll(".install").forEach((box) => {
    box.querySelectorAll("[data-os]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.os === os)));
    box.querySelector("[data-cmd]").textContent = INSTALL[os].cmd;
    box.querySelector("[data-note]").textContent = INSTALL[os].note;
  });
}
document.querySelectorAll("[data-os]").forEach((b) => b.addEventListener("click", () => setOS(b.dataset.os)));
document.querySelectorAll("[data-copy]").forEach((b) =>
  b.addEventListener("click", async () => {
    const text = b.parentElement.querySelector("[data-cmd]").textContent;
    try { await navigator.clipboard.writeText(text); } catch (e) { /* clipboard blocked: the text is still selectable */ }
    b.textContent = "Copied";
    b.classList.add("done");
    setTimeout(() => { b.textContent = "Copy"; b.classList.remove("done"); }, 1600);
  })
);
setOS(guessOS());

const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

// ---------- Fig. 1: three short replays of the real finder ----------
(function () {
if (!document.getElementById("demo")) return;
// sections are listed nearest-the-prompt first, rows best-first, as the finder ranks them
const SCENES = [
  {
    label: "by meaning",
    caption: "Describe it in words. The command that worked comes first.",
    count: "1 close match",
    query: "tear down containers",
    filters: "this folder first ←→  ·  by you F3  ·  any outcome F2",
    sections: [
      { title: "in this folder", rows: [
        ["ok", "✓", "docker compose down -v", "14×   2d", true],
      ] },
    ],
    card: ["docker compose down -v", "docker: Stop and remove containers, networks, images, and volumes created by `docker compose up`", ["ok", "✓ worked every time (14 runs)"], "last 2 days ago · by you", "matched: close in meaning"],
  },
  {
    label: "after a failure",
    caption: "Right after a failure, what worked last time is waiting.",
    count: "18 of 212",
    query: "",
    filters: "in this folder ←→  ·  by you F3  ·  any outcome F2",
    sections: [
      { title: "last command failed: git push", rows: [
        ["ok", "→", "git push --set-upstream origin feat/search", "worked last time", true],
      ] },
      { title: "recent in this folder", rows: [
        ["bad", "✗", "git push", " 4×  now"],
        ["ok", "✓", 'git commit -m "wip"', "10×   1m"],
        ["ok", "✓", "git add -A", "12×   1m"],
      ] },
    ],
    card: ["git push --set-upstream origin feat/search", "Last time this failed, you ran this next and it worked (3x).", ["ok", "✓ worked every time (3 runs)"], "last 2 days ago · by you", "Alt+F inserts it at the prompt"],
  },
  {
    label: "flows",
    caption: "Walk through a flow you repeat: the next step waits on ↑.",
    count: "4 steps",
    query: "",
    filters: "a flow you ran 4x · 4 steps",
    sections: [
      { title: "flow · 2 of 4 done · ^X stops it", rows: [
        ["info", "⇢", "npx prisma migrate dev", "step 3 of 4", true],
        ["info", "⇢", "npm run dev", "step 4 of 4"],
      ] },
    ],
    card: ["npx prisma migrate dev", "Step 3 of 4. ↵ puts it on your prompt; `npm run dev` then waits on ↑.", ["ok", "✓ worked every time (8 runs)"], "last 2 days ago · by you", "in ~/work/web"],
  },
  {
    label: "blanks",
    caption: "Thirty-eight commit messages, one command with a blank.",
    count: "3 results",
    query: "git commit",
    filters: "this folder first ←→  ·  by you F3  ·  variants folded ^G",
    sections: [
      { title: "in this folder", rows: [
        ["ok", "✓", 'git commit -m "‹message›"', "38×   1h", true],
        ["ok", "✓", "git commit --amend --no-edit", " 6×   3d"],
      ] },
      { title: "everywhere", rows: [
        ["ok", "✓", "git commit -am \"‹message›\"", "12×   2w"],
      ] },
    ],
    card: ['git commit -m "‹message›"', '38 variants · recent: "fix login", "wip", "bump deps"', ["ok", "✓ worked every time (38 runs)"], "usually 1s · last 1 hour ago · by you", "Enter: the cursor lands in the blank"],
  },
  {
    label: "by time",
    caption: "Say when: only what ran then is left.",
    count: "2 results",
    query: "deploy last week",
    filters: "this folder first ←→  ·  any outcome F2  ·  ran last week",
    sections: [
      { title: "in this folder", rows: [
        ["ok", "✓", "./deploy.ps1 prod", " 1×  Fri", true],
        ["ok", "✓", "./deploy.ps1 staging", " 2×  Wed"],
      ] },
    ],
    card: ["./deploy.ps1 prod", "Deploy to production", ["ok", "✓ worked every time (4 runs)"], "usually 6m 20s · last 6 days ago · by you", "matched: shares your words · ran last week"],
  },
];
const demo = document.getElementById("demo");
const caption = document.getElementById("caption");
const scenes = document.getElementById("scenes");
let timer = null, token = 0;

SCENES.forEach((s, i) => {
  const b = document.createElement("button");
  b.textContent = s.label;
  b.setAttribute("aria-label", "Example " + (i + 1) + ": " + s.caption);
  b.addEventListener("click", () => play(i));
  scenes.appendChild(b);
});

function rowHTML([cls, glyph, cmd, meta, sel], q, fade) {
  let text = esc(cmd);
  q.split(/\s+/).filter((w) => w.length > 2 && !/^(last|week|yesterday|today)$/.test(w)).forEach((w) => {
    text = text.replace(new RegExp("(" + w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "ig"), '<span class="hl">$1</span>');
  });
  // a command with a blank: the blank in the accent colour
  text = text.replace(/‹([^›]*)›/g, '<span class="blank">‹$1›</span>');
  const metaCls = glyph === "→" ? "ok" : cls === "info" ? "info" : "";
  return `<div class="t-row${fade ? " fade" : ""}${sel ? " t-sel" : ""}"><span class="${cls}">${glyph} </span><span class="t-cmd">${text}</span><span class="t-meta ${metaCls}">${esc(meta)}</span></div>`;
}

function render(s, typed, showResults, fade) {
  // bottom-up, like the real finder: the first section and best row sit nearest the prompt
  const lines = [];
  if (showResults) {
    [...s.sections].reverse().forEach((sec, i) => {
      if (i > 0) lines.push('<div class="t-row"> </div>');
      lines.push(`<div class="t-row"><span class="t-head">   ${esc(sec.title)} </span><span class="t-rule">${"─".repeat(90)}</span></div>`);
      [...sec.rows].reverse().forEach((r) => lines.push(rowHTML(r, typed, fade)));
    });
  }
  const pad = Math.max(0, 7 - lines.length);
  const list = Array(pad).fill('<div class="t-row"> </div>').join("") + lines.join("");
  const f = fade ? " fade" : "";
  const card = showResults
    ? `<div class="c-title${f}">${esc(s.card[0]).replace(/‹([^›]*)›/g, '<span class="blank">‹$1›</span>')}</div><div class="c-why${f}">${esc(s.card[1])}</div><div class="${f}"><span class="${s.card[2][0]}">${esc(s.card[2][1])}</span></div><div class="mut${f}">${esc(s.card[3])}</div><div class="mut${f}">${esc(s.card[4])}</div>`
    : "";
  const q = typed
    ? `<span>${esc(typed)}</span><span class="caret"></span>`
    : `<span class="placeholder">describe it in words, or type part of the command</span><span class="caret"></span>`;
  demo.innerHTML =
    `<div class="t-split"><div>${list}</div><div class="t-card">${card}</div></div>` +
    `<div class="t-filters"><span>${esc(s.filters)}</span><span>${showResults ? esc(s.count) : "…"}</span></div>` +
    `<div class="t-query"><span class="p">›</span>${q}<span class="tabs-mini"><b>Recall</b>  Fixes  Flows</span></div>`;
}

function play(i) {
  const my = ++token;
  clearTimeout(timer);
  const s = SCENES[i];
  caption.textContent = s.caption;
  [...scenes.children].forEach((d, j) => d.setAttribute("aria-current", String(j === i)));
  if (reduced || !s.query) {
    render(s, s.query, true, true);
    timer = setTimeout(() => play((i + 1) % SCENES.length), reduced ? 9000 : 5200);
    return;
  }
  let n = 0;
  render(s, "", false);
  const type = () => {
    if (my !== token) return;
    n++;
    render(s, s.query.slice(0, n), n >= s.query.length, n === s.query.length);
    if (n < s.query.length) timer = setTimeout(type, 70 + Math.random() * 60);
    else timer = setTimeout(() => play((i + 1) % SCENES.length), 5200);
  };
  timer = setTimeout(type, 700);
}
play(0);
})();

// ---------- the name: what reman stands for ----------
(function () {
  const el = document.getElementById("acro");
  if (!el) return;
  // one part per letter of R·E·M·A·N; the part's first capital is the letter
  const LINES = [
    ["Remembers", "Exactly,", "Makes", "Absolutely", "Nothing up"],
    ["Repeats", "Every", "Mistake?", "Absolutely", "Not."],
    ["Retrieves", "Exactly", "what you Meant,", "Approximately", "Named"],
    ["Real", "Execution", "Memory for", "Agents,", "Natively"],
  ];
  let k = 0, auto = null;
  const letter = (w) => {
    const i = w.search(/[A-Z]/);
    return esc(w.slice(0, i)) + `<span class="i">${w[i]}</span>` + esc(w.slice(i + 1));
  };
  const show = () => {
    el.innerHTML = '<span class="aw">' + LINES[k].map(letter).join(" ") + "</span>";
    el.setAttribute("aria-label", "reman: " + LINES[k].join(" ") + ". Click for another.");
  };
  show();
  el.addEventListener("click", () => { clearInterval(auto); k = (k + 1) % LINES.length; show(); });
  if (!reduced) auto = setInterval(() => { k = (k + 1) % LINES.length; show(); }, 5000);
})();

// ---------- Fig. 2: search by meaning, one example at a time ----------
(function () {
  const body = document.getElementById("mean-body");
  if (!body) return;
  const EX = [
    ["tear down containers", "docker compose down -v", "Stop and remove a project's containers"],
    ["undo my last commit", "git reset --soft HEAD~1", "Move back one commit, keeping the changes"],
    ["free up docker disk space", "docker system prune -af --volumes", "Remove unused images, containers and volumes"],
    ["kill whatever is on port 3000", "npx kill-port 3000", "Stop the process holding a port"],
    ["biggest folders here", "du -sh * | sort -rh | head -20", "Disk usage per folder, largest first"],
    ["when does the certificate expire", "openssl x509 -in cert.pem -noout -dates", "A certificate's validity dates"],
    ["clear the dns cache", "ipconfig /flushdns", "Flush the DNS resolver cache"],
    ["rebuild images without cache", "docker compose build --no-cache", "Build images from scratch"],
    ["restart the api in kubernetes", "kubectl rollout restart deploy/api -n api", "Restart a deployment's pods"],
    ["dcoker ps", "docker ps", "A typo: two letters swapped still finds it"],
  ];
  const SMALL = new Set(["the", "my", "on", "in", "is", "up", "here", "when", "does", "a", "of", "to", "whatever", "last"]);
  const words = (s) => s.toLowerCase().split(/[^a-z0-9]+/).filter((w) => w && !SMALL.has(w));
  const chips = document.getElementById("mean-chips"), count = document.getElementById("mean-n");
  let k = 0, timer = null, token = 0, visible = false;

  function draw(q, i, showHit) {
    const [query, cmd, what] = EX[i];
    const qw = new Set(words(query));
    const shared = [...new Set(words(cmd).filter((w) => qw.has(w)))];
    let hit = "";
    if (showHit) {
      // the words both share, underlined in the command
      const marked = esc(cmd).replace(/[A-Za-z0-9]+/g, (w) => (shared.includes(w.toLowerCase()) ? `<span class="shared">${w}</span>` : w));
      const share = shared.length
        ? shared.map((w) => `<span class="acc">${esc(w)}</span>`).join(", ") + (i === EX.length - 1 ? "" : ", the rest by meaning")
        : '<span class="none">none.</span> Found by meaning alone.';
      hit = `<div class="m-hit fade"><span class="ok">✓</span><span class="cmd">${marked}</span><span class="close">close match</span></div>` +
        `<dl class="m-facts fade"><dt>shared words</dt><dd>${share}</dd><dt>what it does</dt><dd>${esc(what)}</dd></dl>`;
    } else {
      // while it types: the shape of the answer, so the figure keeps its height
      hit = `<div class="m-hit m-ghost"><span class="sk" style="width:58%"></span></div>` +
        `<dl class="m-facts"><dt>shared words</dt><dd><span class="sk" style="width:34%"></span></dd><dt>what it does</dt><dd><span class="sk" style="width:66%"></span></dd></dl>`;
    }
    body.innerHTML = `<div class="m-query"><span class="p">›</span><span>${esc(q)}</span><span class="caret"></span></div>${hit}`;
    count.textContent = `${i + 1} of ${EX.length}`;
    chips.querySelectorAll("button").forEach((b, j) => b.setAttribute("aria-pressed", String(j === i)));
  }

  function play(i) {
    const my = ++token;
    clearTimeout(timer);
    k = i;
    const q = EX[i][0];
    if (reduced) { draw(q, i, true); return; }
    let n = 0;
    const type = () => {
      if (my !== token) return;
      n++;
      draw(q.slice(0, n), i, n >= q.length);
      if (n < q.length) timer = setTimeout(type, 55 + Math.random() * 45);
      else timer = setTimeout(() => { if (visible) play((i + 1) % EX.length); }, 4800);
    };
    draw("", i, false);
    timer = setTimeout(type, 400);
  }

  EX.forEach(([q], i) => {
    const b = document.createElement("button");
    b.textContent = q;
    b.addEventListener("click", () => play(i));
    chips.appendChild(b);
  });
  // plays while on screen; starts once you scroll to it
  draw(EX[0][0], 0, true);
  if (!reduced && "IntersectionObserver" in window) {
    new IntersectionObserver((es) => {
      const was = visible;
      visible = es[0].isIntersecting;
      if (visible && !was) play(k);
      if (!visible) { clearTimeout(timer); token++; }
    }, { threshold: 0.4 }).observe(body);
  }
})();

// ---------- an agent's loop, as it happens: plays while on screen, still with reduced motion ----------
(function () {
  const box = document.getElementById("chat");
  if (!box) return;
  const msgs = [...box.querySelectorAll(".msg")];
  const typing = document.getElementById("chat-typing");
  const later = (ms) => new Promise((r) => setTimeout(r, ms));
  const still = () => { typing.style.display = "none"; msgs.forEach((m) => m.classList.add("on")); };
  if (reduced || !("IntersectionObserver" in window)) { still(); return; }
  let gen = 0, on = false;
  async function play(g) {
    while (g === gen) {
      msgs.forEach((m) => m.classList.remove("on"));
      typing.style.display = "";
      for (const [i, wait] of [[0, 700], [1, 1500], [2, 1100]]) { await later(wait); if (g !== gen) return; msgs[i].classList.add("on"); }
      await later(1400); if (g !== gen) return;
      typing.style.display = "none";
      msgs[3].classList.add("on");
      await later(5200);
    }
  }
  new IntersectionObserver((es) => {
    const v = es[0].isIntersecting;
    if (v && !on) play(++gen);
    if (!v && on) gen++;
    on = v;
  }, { threshold: 0.3 }).observe(box);
})();

// ---------- Fig. 3: a command with a blank ----------
(function () {
  const body = document.getElementById("blank-body");
  if (!body) return;
  const VALUES = ["fix login", "wip", "bump deps"];
  const B = (k) => `<span class="blank">‹${k}›</span>`;
  function finder() {
    body.innerHTML =
      `<div class="t-row"><span class="t-head">   in this folder </span><span class="t-rule">${"─".repeat(60)}</span></div>` +
      `<div class="t-row t-sel"><span class="ok">✓ </span><span class="t-cmd">git commit -m "${B("message")}"</span><span class="t-meta">38×   1h</span></div>` +
      `<div class="t-row"><span class="ok">✓ </span><span class="t-cmd">kubectl logs ${B("name")} -n prod</span><span class="t-meta">12×   3d</span></div>` +
      `<div class="t-row"><span class="ok">✓ </span><span class="t-cmd">cd ${B("folder")}</span><span class="t-meta">90×   2m</span></div>` +
      `<div class="t-row"> </div>` +
      `<div class="t-row"><span class="info">38 variants</span><span class="mut"> · recent: "fix login", "wip", "bump deps"</span></div>` +
      `<div class="t-query"><span class="p">›</span><span>git commit</span><span class="caret"></span></div>` +
      `<div class="t-actions"><button class="fillbtn" id="blank-enter">Enter ↵</button><span class="mut">puts it on your prompt</span></div>`;
    document.getElementById("blank-enter").addEventListener("click", prompt);
  }
  function prompt() {
    body.innerHTML =
      `<div class="t-row"><span class="acc">PS&gt; </span><span>git commit -m "</span><span class="fill"><input id="blank-in" aria-label="Commit message: type into the blank" placeholder="type here" spellcheck="false" maxlength="40"></span><span>"</span></div>` +
      `<div class="t-row"> </div>` +
      `<div class="t-row"><span class="mut">went in the blank lately:&nbsp;</span><span class="vals">${VALUES.map((v) => `<button data-v="${v}">${v}</button>`).join("")}</span></div>` +
      `<div class="t-actions"><span class="mut">Enter runs it · Esc starts over</span></div>`;
    const inp = document.getElementById("blank-in");
    const size = () => { inp.style.width = (inp.value ? inp.value.length + 1 : 10) + "ch"; };
    size();
    inp.addEventListener("input", size);
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); ran(inp.value.trim() || "wip"); }
      if (e.key === "Escape") { e.preventDefault(); finder(); }
    });
    body.querySelectorAll("[data-v]").forEach((b) => b.addEventListener("click", () => { inp.value = b.dataset.v; size(); inp.focus(); }));
    inp.focus({ preventScroll: true });
  }
  function ran(msg) {
    body.innerHTML =
      `<div class="t-row"><span class="acc">PS&gt; </span><span>git commit -m "${esc(msg)}"</span></div>` +
      `<div class="t-row"><span>[main 3f2a91c] ${esc(msg)}</span></div>` +
      `<div class="t-row"><span class="mut"> 2 files changed, 14 insertions(+), 3 deletions(-)</span></div>` +
      `<div class="t-row"><span class="acc">PS&gt; </span><span class="caret"></span></div>` +
      `<div class="t-actions"><button class="fillbtn" id="blank-again">Again</button></div>`;
    document.getElementById("blank-again").addEventListener("click", finder);
  }
  finder();
})();

// ---------- Fig. 4: time is a search word ----------
(function () {
  const strip = document.getElementById("tw-strip");
  if (!strip) return;
  const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed", "Thu"];
  const TODAY = 10;
  // [day, command, is a deploy]
  const RUNS = [
    [0, "./deploy.ps1 staging", 1], [0, "npm test", 0], [1, "npm run build", 0], [2, "./deploy.ps1 staging", 1], [2, "npm test", 0],
    [3, "git pull", 0], [4, "./deploy.ps1 prod", 1], [4, "npm run build", 0], [7, "git pull", 0], [7, "npm test", 0],
    [8, "kubectl rollout restart deploy/api -n api", 1], [9, "./deploy.ps1 staging --skip-tests", 1], [9, "npm test", 0], [10, "npm run dev", 0],
  ];
  const SEARCHES = [
    { text: "deploy", when: "", days: null, label: "any time" },
    { text: "deploy", when: "yesterday", days: [9, 9], label: "ran yesterday" },
    { text: "deploy", when: "since monday", days: [7, 10], label: "ran since monday" },
    { text: "deploy", when: "last week", days: [0, 6], label: "ran last week" },
  ];
  const dayName = (d) => (d === TODAY ? "today" : d === TODAY - 1 ? "yest." : DAYS[d]);
  strip.innerHTML = DAYS.map((d, i) =>
    `<div class="day${i === TODAY ? " today" : ""}" data-i="${i}"><span class="d">${i === TODAY ? "today" : d}</span>` +
    RUNS.filter((r) => r[0] === i).map((r) => `<span class="dot${r[2] ? " dep" : ""}"></span>`).join("") + "</div>"
  ).join("");
  const chips = document.getElementById("tw-chips"), list = document.getElementById("tw-list"), chip = document.getElementById("tw-chip");
  function pick(k) {
    const s = SEARCHES[k];
    chips.querySelectorAll("button").forEach((b, i) => b.setAttribute("aria-pressed", String(i === k)));
    strip.querySelectorAll(".day").forEach((el) => {
      const i = +el.dataset.i;
      el.classList.toggle("on", !!s.days && i >= s.days[0] && i <= s.days[1]);
    });
    const seen = new Map();
    RUNS.filter((r) => r[2] && (!s.days || (r[0] >= s.days[0] && r[0] <= s.days[1]))).forEach((r) => {
      const e = seen.get(r[1]) || { n: 0, day: r[0] };
      e.n++; e.day = Math.max(e.day, r[0]);
      seen.set(r[1], e);
    });
    const rows = [...seen.entries()].sort((a, b) => b[1].day - a[1].day);
    list.innerHTML = rows.map(([cmd, e], i) =>
      `<div class="t-row${i === 0 ? " t-sel" : ""}"><span class="ok">✓ </span><span class="t-cmd">${esc(cmd).replace(/deploy/, '<span class="hl">deploy</span>')}</span><span class="t-meta">${String(e.n).padStart(2)}×  ${dayName(e.day)}</span></div>`
    ).join("") +
      `<div class="t-query"><span class="p">›</span><span>${esc(s.text)}${s.when ? ' <span class="acc">' + esc(s.when) + "</span>" : ""}</span><span class="caret"></span></div>`;
    chip.textContent = s.label;
  }
  SEARCHES.forEach((s, i) => {
    const b = document.createElement("button");
    b.textContent = s.when ? `${s.text} ${s.when}` : s.text;
    b.addEventListener("click", () => pick(i));
    chips.appendChild(b);
  });
  pick(3);
})();

// ---------- Fig. 10: Alt+N ----------
(function () {
  const term = document.getElementById("altn");
  if (!term) return;
  const body = document.getElementById("altn-body"), btn = document.getElementById("altn-btn"), hint = document.getElementById("altn-hint");
  const IDEAS = [
    ["alembic upgrade head", "the next step of the flow you started (3 of 4)"],
    ["npm run dev", "after `docker compose up -d` (12x)"],
    ["docker compose logs -f api", "often run here (9x)"],
  ];
  const ran = `<div class="t-row"><span class="acc">PS&gt; </span><span>docker compose up -d</span></div><div class="t-row"><span class="mut"> ✔ Container api-db-1  Started</span></div>`;
  let i = -1;
  function draw() {
    if (i < 0) {
      body.innerHTML = ran + `<div class="t-row"><span class="acc">PS&gt; </span><span class="caret"></span></div>`;
      hint.textContent = "an empty prompt: press it a few times";
      return;
    }
    const [cmd, why] = IDEAS[i];
    body.innerHTML = ran +
      `<div class="t-row"><span class="mut">  reman: ${esc(why)}   (Alt+N again: ${IDEAS.length - 1} more)</span></div>` +
      `<div class="t-row fade"><span class="acc">PS&gt; </span><span style="color:#fff;font-weight:600">${esc(cmd)}</span><span class="caret" style="margin-left:2px"></span></div>`;
    hint.textContent = `idea ${i + 1} of ${IDEAS.length}, on your prompt, not run · Esc clears`;
  }
  const press = () => { i = (i + 1) % IDEAS.length; draw(); };
  btn.addEventListener("click", press);
  term.addEventListener("keydown", (e) => {
    if (e.altKey && (e.code === "KeyN" || e.key === "n" || e.key === "N")) { e.preventDefault(); press(); }
    else if (e.key === "Escape") { i = -1; draw(); }
  });
  draw();
})();
