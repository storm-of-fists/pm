// pm_math.hpp — Lightweight math primitives for pm games
//
// Provides:
//   Vec2              — 2D vector with arithmetic operators
//   len, dist, norm   — vector utilities
//   Rng               — fast xorshift32 PRNG with float helpers
//
// No dependencies beyond <cmath> and <cstdint>.

#pragma once
#include <cmath>
#include <cstdint>

namespace pm {

// =============================================================================
// Vec2
// =============================================================================
struct Vec2 {
    float x = 0, y = 0;

    Vec2() = default;
    Vec2(float x, float y) : x(x), y(y) {}

    Vec2 operator+(Vec2 o)  const { return {x + o.x, y + o.y}; }
    Vec2 operator-(Vec2 o)  const { return {x - o.x, y - o.y}; }
    Vec2 operator*(float s) const { return {x * s, y * s}; }
    Vec2 operator/(float s) const { return {x / s, y / s}; }

    Vec2& operator+=(Vec2 o) { x += o.x; y += o.y; return *this; }
    Vec2& operator-=(Vec2 o) { x -= o.x; y -= o.y; return *this; }
    Vec2& operator*=(float s) { x *= s; y *= s; return *this; }
};

inline Vec2 operator*(float s, Vec2 v) { return {s * v.x, s * v.y}; }

inline float len(Vec2 v) {
    return std::sqrt(v.x * v.x + v.y * v.y);
}

inline float dist(Vec2 a, Vec2 b) {
    return len(a - b);
}

inline Vec2 norm(Vec2 v) {
    float l = len(v);
    return l > 0.0001f ? v * (1.f / l) : Vec2{0, 0};
}

// =============================================================================
// Rng — xorshift32 PRNG
// =============================================================================
struct Rng {
    uint32_t state;

    explicit Rng(uint32_t seed = 42) : state(seed ? seed : 1) {}

    uint32_t next() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        return state;
    }

    // Uniform float in [0, 1)
    float rf() {
        return (float)(next() & 0xFFFFFF) / (float)0x1000000;
    }

    // Uniform float in [lo, hi]
    float rfr(float lo, float hi) {
        return lo + rf() * (hi - lo);
    }
};

} // namespace pm