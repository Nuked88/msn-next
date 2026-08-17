// Synthesized MSN-style effects. No audio assets shipped; Web Audio only.
// Drop-in real samples later if wanted; keep this as the fallback.
let ctx: AudioContext | null = null

function audio(): AudioContext | null {
  try {
    ctx ??= new AudioContext()
    if (ctx.state === 'suspended') void ctx.resume()
    return ctx
  } catch {
    return null
  }
}

type Tone = { freq: number; start: number; dur: number; type?: OscillatorType; gain?: number }

function play(tones: Tone[]) {
  const c = audio()
  if (!c) return
  const now = c.currentTime
  for (const t of tones) {
    const osc = c.createOscillator()
    const g = c.createGain()
    osc.type = t.type ?? 'sine'
    osc.frequency.setValueAtTime(t.freq, now + t.start)
    const peak = t.gain ?? 0.06
    g.gain.setValueAtTime(0.0001, now + t.start)
    g.gain.exponentialRampToValueAtTime(peak, now + t.start + 0.01)
    g.gain.exponentialRampToValueAtTime(0.0001, now + t.start + t.dur)
    osc.connect(g).connect(c.destination)
    osc.start(now + t.start)
    osc.stop(now + t.start + t.dur + 0.02)
  }
}

export const sounds = {
  messageIn: () => play([{ freq: 880, start: 0, dur: 0.12 }, { freq: 1175, start: 0.08, dur: 0.14 }]),
  messageOut: () => play([{ freq: 660, start: 0, dur: 0.1 }]),
  signIn: () => play([
    { freq: 523, start: 0, dur: 0.12 },
    { freq: 659, start: 0.1, dur: 0.12 },
    { freq: 784, start: 0.2, dur: 0.18 },
  ]),
  signOut: () => play([{ freq: 784, start: 0, dur: 0.12 }, { freq: 523, start: 0.1, dur: 0.18 }]),
  nudge: () => play([
    { freq: 880, start: 0, dur: 0.09, type: 'square', gain: 0.05 },
    { freq: 660, start: 0.09, dur: 0.12, type: 'square', gain: 0.05 },
  ]),
}
