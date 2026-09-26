// Live landing strip (ADR 0030). The server relays the event stream as
// SSE: `reset` carries every row (sent on each connect, so a reconnect
// never shows stale rows); each `row` carries one rendered row, which
// replaces the row with the same id or is appended. The SSE id is the
// event cursor, so the browser resumes after a drop.
"use strict";
(function () {
  const strip = document.getElementById("strip");
  const url = strip && strip.dataset.live;
  if (!url) return;
  const source = new EventSource(url);
  source.addEventListener("reset", function (e) {
    strip.innerHTML = e.data;
  });
  source.addEventListener("row", function (e) {
    const holder = document.createElement("template");
    holder.innerHTML = e.data.trim();
    const row = holder.content.firstElementChild;
    if (!row) return;
    const old = document.getElementById(row.id);
    if (old) old.replaceWith(row); else strip.appendChild(row);
  });
})();
