// Live landing strip: each server-sent event carries one rendered row
// (`strip_row.html`); it replaces the row with the same id or is appended.
// The SSE `id` is the event cursor, so the browser resumes after a drop.
"use strict";
(function () {
  const strip = document.getElementById("strip");
  const url = strip && strip.dataset.live;
  if (!url) return;
  const source = new EventSource(url);
  source.addEventListener("row", function (e) {
    const holder = document.createElement("template");
    holder.innerHTML = e.data.trim();
    const row = holder.content.firstElementChild;
    if (!row) return;
    const old = document.getElementById(row.id);
    if (old) old.replaceWith(row); else strip.appendChild(row);
  });
})();
