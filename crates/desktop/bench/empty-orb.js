// A stand-in orb that only keeps a requestAnimationFrame loop going and draws
// nothing: orb-bench.mjs runs it to measure what headless Edge costs by itself,
// so that can be taken off the real orb's numbers.
(function (global) {
  'use strict';
  global.createOrb = function () {
    let raf = global.requestAnimationFrame(function frame() {
      raf = global.requestAnimationFrame(frame);
    });
    return {
      setMode() {}, setLevel() {}, setAccent() {}, lean() {}, setActive() {},
      destroy() { global.cancelAnimationFrame(raf); },
    };
  };
})(typeof window !== 'undefined' ? window : globalThis);
