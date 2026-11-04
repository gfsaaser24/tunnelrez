// The run book's moving test page: big moving blocks, a frame counter and scrolling text, at most 60 frames per
// second (frame-time gate), the same picture as the page the first real-TV batch used.
"use strict";
const c = document.getElementById("c");
const x = c.getContext("2d");
function fit() {
  c.width = innerWidth;
  c.height = innerHeight;
}
fit();
addEventListener("resize", fit);
const STEP = 1000 / 60;
let n = 0;
let next = 0;
function frame(t) {
  requestAnimationFrame(frame);
  if (t < next - 1) return; // too early for the next 60 Hz slot
  next = Math.max(next + STEP, t); // a steady 60 Hz cadence, no catch-up bursts
  n++;
  const w = c.width;
  const h = c.height;
  x.fillStyle = `hsl(${n % 360},60%,25%)`;
  x.fillRect(0, 0, w, h);
  for (let i = 0; i < 40; i++) {
    x.fillStyle = `hsl(${(i * 37 + n * 3) % 360},80%,55%)`;
    const px = (i * 97 + n * (3 + (i % 7))) % w;
    const py = (i * 53 + n * (2 + (i % 5))) % h;
    x.fillRect(px, py, 160, 120);
  }
  x.fillStyle = "#fff";
  x.font = "bold 96px monospace";
  x.fillText(`frame ${n}  ${t.toFixed(1)}`, 60, 140);
  for (let i = 0; i < 30; i++) x.fillText(`TunnelRez pacing test ${(n + i) % 1000}`, 60 + ((n * 4) % 400), 260 + i * 28);
}
requestAnimationFrame(frame);
