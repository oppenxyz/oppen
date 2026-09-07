import { expect, it } from 'bun:test';
import { effectScope } from 'vue';

it('pauses decoration for saved preference, visibility and reduced motion without stopping other timers', async () => {
  const previous = Object.fromEntries(['window', 'document', 'localStorage', 'setInterval', 'clearInterval'].map(key => [key, globalThis[key]]));
  const timers = new Map();
  const storage = new Map([['oppen.decorative-motion', 'paused']]);
  let nextTimer = 0;
  let visibilityChange;
  let reducedChange;
  const scope = effectScope();
  try {
    globalThis.setInterval = fn => { const id = ++nextTimer; timers.set(id, fn); return id; };
    globalThis.clearInterval = id => timers.delete(id);
    globalThis.localStorage = { getItem: key => storage.get(key), setItem: (key, value) => storage.set(key, value) };
    globalThis.window = { matchMedia: () => ({ matches: false, addEventListener: (_event, fn) => { reducedChange = fn; } }) };
    globalThis.document = { hidden: false, addEventListener: (_event, fn) => { visibilityChange = fn; } };
    let dataTicks = 0;
    setInterval(() => dataTicks++);
    const { useClock, setMotionPaused, motionPaused, motionStill } = await import('./clock');
    const tick = scope.run(useClock);
    const advance = () => [...timers.values()].forEach(fn => fn());
    expect(motionPaused.value).toBe(true);
    advance(); expect(tick.value).toBe(0); expect(dataTicks).toBe(1);
    setMotionPaused(false);
    expect(storage.get('oppen.decorative-motion')).toBe('playing');
    advance(); expect(tick.value).toBe(1);
    document.hidden = true; visibilityChange();
    advance(); expect(tick.value).toBe(1); expect(dataTicks).toBe(3);
    document.hidden = false; visibilityChange();
    advance(); expect(tick.value).toBe(2);
    reducedChange({ matches: true });
    advance(); expect(motionStill.value).toBe(true); expect(tick.value).toBe(0);
    reducedChange({ matches: false });
    advance(); expect(tick.value).toBe(1);
    setMotionPaused(true);
    expect(storage.get('oppen.decorative-motion')).toBe('paused');
    advance(); expect(tick.value).toBe(1); expect(dataTicks).toBe(7);
    scope.stop(); expect(timers.size).toBe(1);
  } finally {
    scope.stop();
    for (const [key, value] of Object.entries(previous)) globalThis[key] = value;
  }
});
