// Flight-recorder transport: the scrubber asks the server for the strip
// after N events (`frames?at=N&speed=S`); the server renders it and says in
// `Hord-Next-Delay` how long to wait before the next event at that speed.
"use strict";
(function () {
  const form = document.getElementById("transport");
  const strip = document.getElementById("strip");
  if (!form || !strip) return;
  const total = Number(form.dataset.total);
  const at = form.elements.at, where = form.elements.where, play = form.elements.play;
  let timer = null, request = 0;

  async function show(n) {
    const mine = ++request;
    const url = form.dataset.frames + "?at=" + n + "&speed=" + form.elements.speed.value;
    const res = await fetch(url);
    if (!res.ok || mine !== request) return null;
    strip.innerHTML = await res.text();
    at.value = n;
    where.value = n + " / " + total;
    return Number(res.headers.get("Hord-Next-Delay") || 0);
  }
  function stop() {
    clearTimeout(timer); timer = null; play.textContent = "play";
  }
  async function step() {
    const n = Number(at.value) + 1;
    if (n > total) { stop(); return; }
    const delay = await show(n);
    if (timer === null || delay === null) return;
    timer = setTimeout(step, delay);
  }
  play.addEventListener("click", function () {
    if (timer !== null) { stop(); return; }
    if (Number(at.value) >= total) at.value = 0;
    play.textContent = "pause";
    timer = setTimeout(step, 0);
  });
  at.addEventListener("input", function () { stop(); show(Number(at.value)); });
})();
