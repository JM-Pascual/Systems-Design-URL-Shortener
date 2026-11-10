// Navigation: arrows, space, PageUp/PageDown, Home/End, or click the right/left half.
// The slide number lives in the URL hash, so a reload or a shared link keeps its place.
const slides = [...document.querySelectorAll('.slide')];
const tiers = ['0', '1', '2', '3', '4.1', '4.2', '4.3', '4.4', '5'];
const progress = document.querySelector('.progress');
const lightbox = document.querySelector('.lightbox');
let current = 0;

slides.forEach((slide, i) => {
  const n = slide.querySelector('.top .n');
  if (n) n.textContent = String(i + 1).padStart(2, '0');
});

// The strip of tiers on every title slide, with the current one highlighted.
document.querySelectorAll('.strip').forEach((strip) => {
  const at = tiers.indexOf(strip.dataset.current);
  strip.innerHTML = tiers
    .map((t, i) => `<span class="${i === at ? 'on' : i < at ? 'done' : ''}">${t}</span>`)
    .join('');
});

function fit() {
  const scale = Math.min(innerWidth / 1280, innerHeight / 720);
  document.documentElement.style.setProperty('--scale', scale);
}

function show(i) {
  current = Math.max(0, Math.min(slides.length - 1, i));
  slides.forEach((s, j) => s.classList.toggle('active', j === current));
  progress.style.width = `${((current + 1) / slides.length) * 100}%`;
  history.replaceState(null, '', `#${current + 1}`);
}

// A slide with .step elements reveals them one at a time before moving on.
function forward() {
  const next = slides[current].querySelector('.step:not(.shown)');
  if (next) next.classList.add('shown');
  else show(current + 1);
}

function back() {
  const shown = slides[current].querySelectorAll('.step.shown');
  if (shown.length) shown[shown.length - 1].classList.remove('shown');
  else show(current - 1);
}

addEventListener('keydown', (e) => {
  if (lightbox.classList.contains('open')) {
    if (e.key === 'Escape') lightbox.classList.remove('open');
    return;
  }
  if (['ArrowRight', 'ArrowDown', 'PageDown', ' ', 'Enter'].includes(e.key)) forward();
  else if (['ArrowLeft', 'ArrowUp', 'PageUp', 'Backspace'].includes(e.key)) back();
  else if (e.key === 'Home') show(0);
  else if (e.key === 'End') show(slides.length - 1);
  else if (e.key === 'f') document.documentElement.requestFullscreen?.();
  else return;
  e.preventDefault();
});

addEventListener('click', (e) => {
  const figure = e.target.closest('.slide.diagram figure:not(.scroll):not(.pan)');
  if (figure) {
    lightbox.innerHTML = '';
    lightbox.append(figure.querySelector('img').cloneNode());
    lightbox.classList.add('open');
    return;
  }
  if (e.target.closest('.lightbox')) return lightbox.classList.remove('open');
  if (e.target.closest('a, figure.scroll, figure.pan')) return;
  if (e.clientX > innerWidth / 2) forward();
  else back();
});

// A mouse wheel pans a wide diagram sideways.
document.querySelectorAll('figure.pan').forEach((f) =>
  f.addEventListener('wheel', (e) => {
    if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
      f.scrollLeft += e.deltaY;
      e.preventDefault();
    }
  }, { passive: false }));

addEventListener('resize', fit);
fit();
hljs.highlightAll();
show(parseInt(location.hash.slice(1), 10) - 1 || 0);
