// pm_util.hpp — PLC-inspired helpers for game logic
//
// Provides:
//   Hysteresis<T>  — value with hold-time persistence (anti-flicker)
//   Cooldown       — fire-once-per-interval timer
//   DelayTimer     — unified on/off delay (TON + TOF in one struct)
//   RisingEdge     — one-tick pulse on false→true transition
//   FallingEdge    — one-tick pulse on true→false transition
//   Latch          — set-reset flip-flop with configurable priority
//   Counter        — count up/down with preset and done flag
//
// All time is float seconds (matches ctx.dt()).
// No external dependencies.

#pragma once

namespace pm {

// =============================================================================
// Hysteresis<T> — value with dead-zone persistence
// =============================================================================
//
// Holds a value and blocks changes for `hold` seconds after each transition.
//   Hysteresis<bool> facing(false, 0.1f);
//   facing.update(dt);
//   facing.set(true);   // only applies if cooldown expired

template<typename T>
struct Hysteresis {
	T     value;
	float hold     = 0.f;
	float cooldown = 0.f;

	Hysteresis() = default;
	Hysteresis(T initial, float hold_sec) : value(initial), hold(hold_sec) {}

	void update(float dt) { if (cooldown > 0.f) cooldown -= dt; }

	void set(T v) {
		if (cooldown > 0.f) return;
		if (!(value == v)) { value = v; cooldown = hold; }
	}

	T get() const { return value; }
	operator T() const { return value; }
};

// =============================================================================
// Cooldown — fire-once-per-interval timer
// =============================================================================
//
// Accumulates time; ready() returns true once per interval.
//   Cooldown cd(0.5f);
//   if (cd.ready(dt)) { /* fires every 0.5s */ }

struct Cooldown {
	float interval = 1.f;
	float elapsed  = 0.f;

	Cooldown() = default;
	explicit Cooldown(float sec) : interval(sec) {}

	bool ready(float dt) {
		elapsed += dt;
		if (elapsed >= interval) { elapsed -= interval; return true; }
		return false;
	}

	void  reset()           { elapsed = 0.f; }
	float remaining() const { return interval - elapsed; }
};

// =============================================================================
// DelayTimer — unified on-delay / off-delay timer
// =============================================================================
//
// Output goes true after input has been true for on_delay seconds.
// Output goes false after input has been false for off_delay seconds.
//
//   DelayTimer dt(0.5f, 0.2f);   // 500ms on-delay, 200ms off-delay
//   dt.update(button_held, ctx.dt());
//   if (dt) { /* delayed activation */ }
//
// Pulse timer: feed !output back as input with on_delay=0.
//   DelayTimer pulse(0.f, 1.f);  // 1s pulse
//   pulse.update(!pulse.output, dt);

struct DelayTimer {
	float on_delay  = 0.f;
	float off_delay = 0.f;
	float elapsed   = 0.f;
	bool  output    = false;

	DelayTimer() = default;
	DelayTimer(float on_sec, float off_sec)
		: on_delay(on_sec), off_delay(off_sec) {}

	void update(bool input, float dt) {
		if (output) {
			if (!input) {
				elapsed += dt;
				if (elapsed >= off_delay) { output = false; elapsed = 0.f; }
			} else {
				elapsed = 0.f;
			}
		} else {
			if (input) {
				elapsed += dt;
				if (elapsed >= on_delay) { output = true; elapsed = 0.f; }
			} else {
				elapsed = 0.f;
			}
		}
	}

	void reset() { output = false; elapsed = 0.f; }
	operator bool() const { return output; }
};

// =============================================================================
// RisingEdge / FallingEdge — one-tick pulse on bool transitions
// =============================================================================
//
//   RisingEdge re;
//   if (re.update(button)) { /* fires once when button goes true */ }

struct RisingEdge {
	bool previous = false;

	bool update(bool input) {
		bool fired = input && !previous;
		previous = input;
		return fired;
	}
};

struct FallingEdge {
	bool previous = false;

	bool update(bool input) {
		bool fired = !input && previous;
		previous = input;
		return fired;
	}
};

// =============================================================================
// Latch — set-reset flip-flop
// =============================================================================
//
// When both set and reset are true simultaneously, reset_dominant controls
// which wins (true = reset wins, false = set wins).
//
//   Latch alarm;                    // reset-dominant by default
//   alarm.update(trigger, clear);
//   if (alarm) { /* latched on */ }

struct Latch {
	bool output         = false;
	bool reset_dominant = true;

	Latch() = default;
	explicit Latch(bool reset_dom) : reset_dominant(reset_dom) {}

	void update(bool set, bool reset) {
		if (set && reset) {
			output = !reset_dominant;
		} else if (set) {
			output = true;
		} else if (reset) {
			output = false;
		}
	}

	operator bool() const { return output; }
};

// =============================================================================
// Counter — count up/down with preset and done flag
// =============================================================================
//
// Compose with RisingEdge for edge-triggered counting:
//   Counter kills(10);
//   if (edge.update(hit)) kills.increment();
//   if (kills.done) { /* reached preset */ }

struct Counter {
	int  count  = 0;
	int  preset = 0;
	bool done   = false;

	Counter() = default;
	explicit Counter(int preset_val) : preset(preset_val) {}

	void increment() {
		if (done) return;
		if (++count >= preset) done = true;
	}

	void decrement() {
		if (done) return;
		if (--count <= 0) { count = 0; done = true; }
	}

	void reset() { count = 0; done = false; }
	void reset(int new_preset) { preset = new_preset; count = 0; done = false; }
};

} // namespace pm