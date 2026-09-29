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

/// Case folding for the alphabets we meet in contact names: Latin (incl. accents), Greek and all of
/// Cyrillic (Ukrainian І Ї Є Ґ, Belarusian Ў, ...). "Ё" is additionally folded onto "Е" so "Артём" == "Артем".
fn foldCp(cp: u32) u32 {
    const lower: u32 = switch (cp) {
        'A'...'Z' => cp + 32,
        0xC0...0xD6, 0xD8...0xDE => cp + 0x20, // Latin-1 accented capitals (not ×)
        0x0100...0x0137, 0x014A...0x0177 => if (cp % 2 == 0) cp + 1 else cp, // Latin Extended-A pairs
        0x0139...0x0148 => if (cp % 2 == 1) cp + 1 else cp,
        0x0178 => 0xFF,
        0x0179...0x017E => if (cp % 2 == 1) cp + 1 else cp,
        0x0386 => 0x03AC,
        0x0388...0x038A => cp + 0x25,
        0x038C => 0x03CC,
        0x038E, 0x038F => cp + 0x3F,
        0x0391...0x03A1, 0x03A3...0x03AB => cp + 0x20, // Greek
        0x0400...0x040F => cp + 0x50, // Ѐ Ё Ђ Ѓ Є Ѕ І Ї Ј Љ Њ Ћ Ќ Ѝ Ў Џ
        0x0410...0x042F => cp + 0x20, // А..Я
        0x04C0 => 0x04CF, // palochka
        0x04C1...0x04CD => if (cp % 2 == 1) cp + 1 else cp,
        0x0460...0x0481, 0x048A...0x04BF, 0x04D0...0x052F => if (cp % 2 == 0) cp + 1 else cp, // Ґ Ә Ө Ү ...
        else => cp,
    };
    if (lower == 0x0451) return 0x0435; // ё -> е
    if (lower == 0x03C2) return 0x03C3; // Greek final sigma ς -> σ
    return lower;
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

/// Length of the longest common subsequence (two rolling rows of u16).
fn lcsLen(a: []const u32, b: []const u32) usize {
    var prev: [MAX_CP + 1]u16 = @splat(0);
    var cur: [MAX_CP + 1]u16 = @splat(0);
    for (a) |ca| {
        cur[0] = 0;
        for (b, 0..) |cb, j| {
            cur[j + 1] = if (ca == cb) prev[j] + 1 else @max(prev[j + 1], cur[j]);
        }
        prev = cur;
    }
    return prev[b.len];
}

/// Indel similarity in percent, like rapidfuzz `fuzz.ratio`: 100 * 2 * LCS / (len_a + len_b).
fn ratio(a: []const u32, b: []const u32) u32 {
    const total = a.len + b.len;
    if (total == 0) return 100;
    return @intCast((200 * lcsLen(a, b)) / total);
}

/// A partial match must start at a word boundary: "оля" is not a match inside "Школярі".
fn isSeparator(cp: u32) bool {
    return cp < 128 and !((cp >= '0' and cp <= '9') or (cp >= 'a' and cp <= 'z'));
}

fn partialRatio(a: []const u32, b: []const u32) u32 {
    const short = if (a.len <= b.len) a else b;
    const long = if (a.len <= b.len) b else a;
    if (short.len == 0) return if (long.len == 0) 100 else 0;
    var best: u32 = 0;
    for (0..long.len - short.len + 1) |s| {
        if (s > 0 and !isSeparator(long[s - 1])) continue;
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

const MAX_TOK = 64;

/// Whitespace-separated tokens of `src` as slices into it (at most MAX_TOK).
fn tokens(src: []const u32, out: *[MAX_TOK][]const u32) usize {
    var n: usize = 0;
    var i: usize = 0;
    while (i < src.len and n < MAX_TOK) {
        while (i < src.len and isSpace(src[i])) i += 1;
        const s = i;
        while (i < src.len and !isSpace(src[i])) i += 1;
        if (i > s) {
            out[n] = src[s..i];
            n += 1;
        }
    }
    return n;
}

/// Sorts `t[0..n]` and removes duplicates; returns the new length.
fn sortUnique(t: *[MAX_TOK][]const u32, n: usize) usize {
    var i: usize = 1;
    while (i < n) : (i += 1) {
        var j = i;
        while (j > 0 and std.mem.order(u32, t[j], t[j - 1]) == .lt) : (j -= 1) {
            const tmp = t[j];
            t[j] = t[j - 1];
            t[j - 1] = tmp;
        }
    }
    var w: usize = 0;
    for (0..n) |r| {
        if (w == 0 or !std.mem.eql(u32, t[w - 1], t[r])) {
            t[w] = t[r];
            w += 1;
        }
    }
    return w;
}

fn contains(t: []const []const u32, x: []const u32) bool {
    for (t) |y| {
        if (std.mem.eql(u32, x, y)) return true;
    }
    return false;
}

fn appendTokens(dst: *Cps, toks: []const []const u32) void {
    for (toks) |tok| {
        if (dst.len > 0 and dst.len < MAX_CP) {
            dst.buf[dst.len] = ' ';
            dst.len += 1;
        }
        const take = @min(MAX_CP - dst.len, tok.len);
        @memcpy(dst.buf[dst.len..][0..take], tok[0..take]);
        dst.len += take;
    }
}

/// rapidfuzz-style token_set_ratio: compares the common words with each side's leftovers, so a query
/// whose words are a subset of the name ("іванов оля" vs "Оля Іванова Петрівна") still scores high.
/// No shared word -> 0; one side fully contained in the other -> 95 (below an exact match).
fn tokenSetRatio(a: []const u32, b: []const u32) u32 {
    var ta: [MAX_TOK][]const u32 = undefined;
    var tb: [MAX_TOK][]const u32 = undefined;
    const na = sortUnique(&ta, tokens(a, &ta));
    const nb = sortUnique(&tb, tokens(b, &tb));
    var sect: [MAX_TOK][]const u32 = undefined;
    var dab: [MAX_TOK][]const u32 = undefined;
    var dba: [MAX_TOK][]const u32 = undefined;
    var ns: usize = 0;
    var nab: usize = 0;
    var nba: usize = 0;
    for (ta[0..na]) |x| {
        if (contains(tb[0..nb], x)) {
            sect[ns] = x;
            ns += 1;
        } else {
            dab[nab] = x;
            nab += 1;
        }
    }
    for (tb[0..nb]) |x| {
        if (!contains(ta[0..na], x)) {
            dba[nba] = x;
            nba += 1;
        }
    }
    if (ns == 0) return 0;
    if (nab == 0 or nba == 0) return 95;
    var s0 = Cps{};
    appendTokens(&s0, sect[0..ns]);
    var s1 = s0;
    appendTokens(&s1, dab[0..nab]);
    var s2 = s0;
    appendTokens(&s2, dba[0..nba]);
    return @max(ratio(s0.slice(), s1.slice()), @max(ratio(s0.slice(), s2.slice()), ratio(s1.slice(), s2.slice())));
}

export fn tgh_fuzzy_score(a: [*]const u8, a_len: usize, b: [*]const u8, b_len: usize) u32 {
    const ca = decode(a[0..a_len]);
    const cb = decode(b[0..b_len]);
    const full = ratio(ca.slice(), cb.slice());
    const partial = partialRatio(ca.slice(), cb.slice());
    const ta = tokenSorted(ca.slice());
    const tb = tokenSorted(cb.slice());
    const sorted = ratio(ta.slice(), tb.slice());
    const set = tokenSetRatio(ca.slice(), cb.slice());
    return @max(@max(full, partial), @max(sorted, set));
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
    // Ukrainian-specific letters and other alphabets fold too
    try std.testing.expectEqual(@as(u32, 100), fz("ІВАН ЇЖАК ЄВГЕН ҐАНОК", "іван їжак євген ґанок"));
    try std.testing.expectEqual(@as(u32, 100), fz("ÉMILE ŁUKASZ ÇA", "émile łukasz ça"));
    try std.testing.expectEqual(@as(u32, 100), fz("ΑΛΕΞΗΣ", "αλεξης"));
    try std.testing.expect(fz("Olga Ivanova", "Ivanova Olga") >= 90);
    try std.testing.expect(fz("Артем", "Артём") >= 75);
    try std.testing.expect(fz("Оля", "Оля Петренко") >= 90);
    try std.testing.expect(fz("abc", "xyz") < 30);
    // a substring inside a word is not a match ("оля" in "Школярі"), but a word start is
    try std.testing.expect(fz("Оля", "Школярі") < 75);
    try std.testing.expect(fz("Оля", "Starfield | Школярі") < 75);
    try std.testing.expect(fz("Оля", "Мама і Оля") >= 90);
    // words of the query are a subset of the name's words, in any order
    try std.testing.expect(fz("іванов оля", "Оля Іванова Петрівна") >= 60);
    try std.testing.expect(fz("іванов контакт", "Контакт Номер 5 Іванов") >= 90);
    try std.testing.expectEqual(fz("іванов контакт", "Контакт Номер 5 Іванов"), fz("Контакт Номер 5 Іванов", "іванов контакт"));
    try std.testing.expect(fz("Мама Мама", "Мама") >= 90); // duplicate words collapse
    try std.testing.expect(fz("оля петренко", "Оля Іванова") < 90); // one shared word is not a match
    try std.testing.expectEqual(@as(u32, 1), tgh_version());
}
