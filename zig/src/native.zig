//! C-ABI helpers linked into the Rust server:
//!  - tgh_cosine_topk: SIMD cosine top-k over a row-major f32 matrix
//!  - tgh_fuzzy_score: 0..100 name similarity (Latin + Cyrillic, case-insensitive)
const std = @import("std");

const MAX_K = 64;
const MAX_CP = 256;
const LANES = 8;
const F8 = @Vector(LANES, f32);

export fn tgh_version() u32 {
    return 1;
}

fn dotAndNorm(a: [*]const f32, b: [*]const f32, dim: usize, nb: *f32) f32 {
    var acc: F8 = @splat(0);
    var accn: F8 = @splat(0);
    var i: usize = 0;
    while (i + LANES <= dim) : (i += LANES) {
        const va: F8 = a[i..][0..LANES].*;
        const vb: F8 = b[i..][0..LANES].*;
        acc += va * vb;
        accn += vb * vb;
    }
    var dot = @reduce(.Add, acc);
    var n = @reduce(.Add, accn);
    while (i < dim) : (i += 1) {
        dot += a[i] * b[i];
        n += b[i] * b[i];
    }
    nb.* = n;
    return dot;
}

/// Returns the number of results written, best first.
export fn tgh_cosine_topk(
    query: [*]const f32,
    dim: usize,
    matrix: [*]const f32,
    rows: usize,
    k_in: usize,
    out_idx: [*]u32,
    out_score: [*]f32,
) usize {
    const k = @min(@min(k_in, MAX_K), rows);
    if (k == 0 or dim == 0) return 0;

    var qn: f32 = 0;
    _ = dotAndNorm(query, query, dim, &qn);
    const qnorm = @sqrt(qn);

    var scores: [MAX_K]f32 = undefined;
    var idxs: [MAX_K]u32 = undefined;
    var count: usize = 0;

    var r: usize = 0;
    while (r < rows) : (r += 1) {
        var rn: f32 = 0;
        const dot = dotAndNorm(query, matrix + r * dim, dim, &rn);
        const denom = qnorm * @sqrt(rn);
        const score: f32 = if (denom > 0) dot / denom else 0;

        if (count == k and score <= scores[k - 1]) continue;
        var pos: usize = if (count < k) count else k - 1;
        while (pos > 0 and scores[pos - 1] < score) : (pos -= 1) {
            scores[pos] = scores[pos - 1];
            idxs[pos] = idxs[pos - 1];
        }
        scores[pos] = score;
        idxs[pos] = @intCast(r);
        if (count < k) count += 1;
    }
    for (0..count) |i| {
        out_idx[i] = idxs[i];
        out_score[i] = scores[i];
    }
    return count;
}

// ---------------------------------------------------------------- fuzzy ----

const Cps = struct {
    buf: [MAX_CP]u32 = undefined,
    len: usize = 0,

    fn slice(self: *const Cps) []const u32 {
        return self.buf[0..self.len];
    }
};

fn foldCp(cp: u32) u32 {
    if (cp >= 'A' and cp <= 'Z') return cp + 32;
    if (cp >= 0x0410 and cp <= 0x042F) return cp + 0x20;
    if (cp == 0x0401) return 0x0451;
    if (cp == 0x0451) return 0x0435; // ё ~ е, so "Артём" == "Артем"
    return cp;
}

fn decode(s: []const u8) Cps {
    var out = Cps{};
    var it = std.unicode.Utf8Iterator{ .bytes = s, .i = 0 };
    while (out.len < MAX_CP) {
        const cp = it.nextCodepoint() orelse break;
        out.buf[out.len] = foldCp(cp);
        out.len += 1;
    }
    return out;
}

fn levenshtein(a: []const u32, b: []const u32) usize {
    var prev: [MAX_CP + 1]u16 = undefined;
    var cur: [MAX_CP + 1]u16 = undefined;
    for (0..b.len + 1) |j| prev[j] = @intCast(j);
    for (a, 0..) |ca, i| {
        cur[0] = @intCast(i + 1);
        for (b, 0..) |cb, j| {
            const sub = prev[j] + @as(u16, if (ca == cb) 0 else 1);
            cur[j + 1] = @min(sub, @min(prev[j + 1] + 1, cur[j] + 1));
        }
        prev = cur;
    }
    return prev[b.len];
}

fn ratio(a: []const u32, b: []const u32) u32 {
    const m = @max(a.len, b.len);
    if (m == 0) return 100;
    const d = levenshtein(a, b);
    return @intCast(((m - d) * 100) / m);
}

fn partialRatio(a: []const u32, b: []const u32) u32 {
    const short = if (a.len <= b.len) a else b;
    const long = if (a.len <= b.len) b else a;
    if (short.len == 0) return if (long.len == 0) 100 else 0;
    var best: u32 = 0;
    for (0..long.len - short.len + 1) |s| {
        best = @max(best, ratio(short, long[s .. s + short.len]));
    }
    return best;
}

fn isSpace(cp: u32) bool {
    return cp == ' ' or cp == '\t' or cp == '\n' or cp == '\r';
}

/// Sort whitespace-separated tokens and rejoin with single spaces.
fn tokenSorted(src: []const u32) Cps {
    var starts: [64]u16 = undefined;
    var lens: [64]u16 = undefined;
    var n: usize = 0;
    var i: usize = 0;
    while (i < src.len and n < 64) {
        while (i < src.len and isSpace(src[i])) i += 1;
        const s = i;
        while (i < src.len and !isSpace(src[i])) i += 1;
        if (i > s) {
            starts[n] = @intCast(s);
            lens[n] = @intCast(i - s);
            n += 1;
        }
    }
    // insertion sort of token indices
    var order: [64]u8 = undefined;
    for (0..n) |t| {
        var p = t;
        order[p] = @intCast(t);
        while (p > 0) : (p -= 1) {
            const x = src[starts[order[p]]..][0..lens[order[p]]];
            const y = src[starts[order[p - 1]]..][0..lens[order[p - 1]]];
            if (std.mem.order(u32, x, y) != .lt) break;
            const tmp = order[p];
            order[p] = order[p - 1];
            order[p - 1] = tmp;
        }
    }
    var out = Cps{};
    for (0..n) |t| {
        const tok = src[starts[order[t]]..][0..lens[order[t]]];
        if (t > 0 and out.len < MAX_CP) {
            out.buf[out.len] = ' ';
            out.len += 1;
        }
        const room = MAX_CP - out.len;
        const take = @min(room, tok.len);
        @memcpy(out.buf[out.len..][0..take], tok[0..take]);
        out.len += take;
    }
    return out;
}

export fn tgh_fuzzy_score(a: [*]const u8, a_len: usize, b: [*]const u8, b_len: usize) u32 {
    const ca = decode(a[0..a_len]);
    const cb = decode(b[0..b_len]);
    const full = ratio(ca.slice(), cb.slice());
    const partial = partialRatio(ca.slice(), cb.slice());
    const ta = tokenSorted(ca.slice());
    const tb = tokenSorted(cb.slice());
    const sorted = ratio(ta.slice(), tb.slice());
    return @max(full, @max(partial, sorted));
}

// ---------------------------------------------------------------- tests ----

fn fz(a: []const u8, b: []const u8) u32 {
    return tgh_fuzzy_score(a.ptr, a.len, b.ptr, b.len);
}

test "cosine top-k" {
    const m = [_]f32{ 1, 0, 0, 0, 0, 1, 0, 0, 1, 1, 0, 0 };
    const q = [_]f32{ 0, 1, 0, 0 };
    var idx: [2]u32 = undefined;
    var sc: [2]f32 = undefined;
    const n = tgh_cosine_topk(&q, 4, &m, 3, 2, &idx, &sc);
    try std.testing.expectEqual(@as(usize, 2), n);
    try std.testing.expectEqual(@as(u32, 1), idx[0]);
    try std.testing.expectApproxEqAbs(@as(f32, 1.0), sc[0], 1e-6);
    try std.testing.expectEqual(@as(u32, 2), idx[1]);
}

test "cosine top-k wide dim uses simd + tail" {
    var m: [3 * 19]f32 = undefined;
    for (&m, 0..) |*v, i| v.* = @floatFromInt(i % 7);
    var q: [19]f32 = undefined;
    for (&q, 0..) |*v, i| v.* = @floatFromInt((i + 3) % 7);
    var idx: [3]u32 = undefined;
    var sc: [3]f32 = undefined;
    try std.testing.expectEqual(@as(usize, 3), tgh_cosine_topk(&q, 19, &m, 3, 8, &idx, &sc));
    try std.testing.expect(sc[0] >= sc[1] and sc[1] >= sc[2]);
}

test "fuzzy" {
    try std.testing.expectEqual(@as(u32, 100), fz("Олександр", "олександр"));
    try std.testing.expect(fz("Olga Ivanova", "Ivanova Olga") >= 90);
    try std.testing.expect(fz("Артем", "Артём") >= 75);
    try std.testing.expect(fz("Оля", "Оля Петренко") >= 90);
    try std.testing.expect(fz("abc", "xyz") < 30);
    try std.testing.expectEqual(@as(u32, 1), tgh_version());
}
