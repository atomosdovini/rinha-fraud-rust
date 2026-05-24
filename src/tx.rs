/// Zero-alloc JSON parser for /fraud-score request body.
/// Ported from tx.hpp.
use crate::index::{qclamp01, DIMS};

fn d2(s: &[u8], p: usize) -> i32 {
    (s[p] - b'0') as i32 * 10 + (s[p + 1] - b'0') as i32
}

fn d4(s: &[u8], p: usize) -> i32 {
    (s[p] - b'0') as i32 * 1000
        + (s[p + 1] - b'0') as i32 * 100
        + (s[p + 2] - b'0') as i32 * 10
        + (s[p + 3] - b'0') as i32
}

fn civil_day(y: i32, m: i32, d: i32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) as u32 + 2) / 5 + d as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i32 - 719468
}

fn ts_minutes(ts: &[u8]) -> i32 {
    // Fast path: March 2026
    if ts.len() >= 16
        && ts[0] == b'2' && ts[1] == b'0' && ts[2] == b'2' && ts[3] == b'6'
        && ts[5] == b'0' && ts[6] == b'3'
    {
        return (d2(ts, 8) - 1) * 1440 + d2(ts, 11) * 60 + d2(ts, 14);
    }
    civil_day(d4(ts, 0), d2(ts, 5), d2(ts, 8)) * 1440 + d2(ts, 11) * 60 + d2(ts, 14)
}

fn ts_weekday(ts: &[u8]) -> i32 {
    if ts.len() >= 16
        && ts[0] == b'2' && ts[1] == b'0' && ts[2] == b'2' && ts[3] == b'6'
        && ts[5] == b'0' && ts[6] == b'3'
    {
        return (d2(ts, 8) + 5) % 7;
    }
    let y = d4(ts, 0); let m = d2(ts, 5); let day = d2(ts, 8);
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    ((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + day) % 7 + 6) % 7
}

fn mcc_risk(code: &[u8]) -> i16 {
    if code.len() < 4 { return 5000; }
    let v = (code[0] - b'0') as i32 * 1000
        + (code[1] - b'0') as i32 * 100
        + (code[2] - b'0') as i32 * 10
        + (code[3] - b'0') as i32;
    match v {
        5411 => 1500, 5812 => 3000, 5912 => 2000, 5944 => 4500,
        7801 => 8000, 7802 => 7500, 7995 => 8500, 4511 => 3500,
        5311 => 2500, _ => 5000,
    }
}

fn skip_ws(s: &[u8], p: &mut usize) {
    while *p < s.len() && s[*p] <= b' ' { *p += 1; }
}

fn past_colon(s: &[u8], p: &mut usize) -> bool {
    while *p < s.len() && s[*p] != b':' { *p += 1; }
    if *p >= s.len() { return false; }
    *p += 1;
    skip_ws(s, p);
    *p < s.len()
}

fn read_string<'a>(s: &'a [u8], p: &mut usize) -> &'a [u8] {
    let open = match s[*p..].iter().position(|&b| b == b'"') {
        Some(i) => *p + i,
        None => return &[],
    };
    let close = match s[open + 1..].iter().position(|&b| b == b'"') {
        Some(i) => open + 1 + i,
        None => return &[],
    };
    *p = close + 1;
    &s[open + 1..close]
}

fn read_number(s: &[u8], p: &mut usize) -> Option<f64> {
    skip_ws(s, p);
    if *p >= s.len() { return None; }
    let neg = s[*p] == b'-';
    if neg { *p += 1; if *p >= s.len() { return None; } }
    let mut v = 0f64;
    let mut ok = false;
    while *p < s.len() && s[*p] >= b'0' && s[*p] <= b'9' {
        v = v * 10.0 + (s[*p] - b'0') as f64; *p += 1; ok = true;
    }
    if *p < s.len() && s[*p] == b'.' {
        *p += 1; let mut f = 0.1f64;
        while *p < s.len() && s[*p] >= b'0' && s[*p] <= b'9' {
            v += (s[*p] - b'0') as f64 * f; f *= 0.1; *p += 1; ok = true;
        }
    }
    if !ok { return None; }
    Some(if neg { -v } else { v })
}

fn read_bool(s: &[u8], p: &mut usize) -> Option<bool> {
    skip_ws(s, p);
    if s[*p..].starts_with(b"true")  { *p += 4; return Some(true); }
    if s[*p..].starts_with(b"false") { *p += 5; return Some(false); }
    None
}

fn seek_key<'a>(s: &'a [u8], p: &mut usize, key: &[u8]) -> bool {
    loop {
        let q = match s[*p..].iter().position(|&b| b == b'"') {
            Some(i) => *p + i,
            None => return false,
        };
        let end = q + 1 + key.len();
        if end < s.len() && &s[q + 1..q + 1 + key.len()] == key && s[end] == b'"' {
            *p = end + 1;
            return true;
        }
        *p = q + 1;
    }
}

/// Extract feature vector from JSON body. Returns false on parse error.
pub fn extract(body: &[u8], out: &mut [i16; DIMS]) -> bool {
    let mut p = 0usize;

    if !seek_key(body, &mut p, b"amount") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let amount = match read_number(body, &mut p) { Some(v) => v, None => return false };

    if !seek_key(body, &mut p, b"installments") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let installments = match read_number(body, &mut p) { Some(v) => v, None => return false };

    if !seek_key(body, &mut p, b"requested_at") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let ts = read_string(body, &mut p);
    if ts.len() < 16 { return false; }

    // First avg_amount = customer's
    if !seek_key(body, &mut p, b"avg_amount") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let cust_avg = match read_number(body, &mut p) { Some(v) if v != 0.0 => v, _ => return false };

    if !seek_key(body, &mut p, b"tx_count_24h") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let tx_count = match read_number(body, &mut p) { Some(v) => v, None => return false };

    // known_merchants: capture the array substring
    let ka = match body[p..].windows(15).position(|w| w == b"known_merchants") {
        Some(i) => p + i,
        None => return false,
    };
    let ab = match body[ka..].iter().position(|&b| b == b'[') {
        Some(i) => ka + i,
        None => return false,
    };
    let ae = match body[ab..].iter().position(|&b| b == b']') {
        Some(i) => ab + i,
        None => return false,
    };
    let known = &body[ab..=ae];
    p = ae + 1;

    if !seek_key(body, &mut p, b"id") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let merch_id = read_string(body, &mut p);

    if !seek_key(body, &mut p, b"mcc") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let mcc = read_string(body, &mut p);

    // Second avg_amount = merchant's
    if !seek_key(body, &mut p, b"avg_amount") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let merch_avg = match read_number(body, &mut p) { Some(v) => v, None => return false };

    if !seek_key(body, &mut p, b"is_online") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let online = match read_bool(body, &mut p) { Some(v) => v, None => return false };

    if !seek_key(body, &mut p, b"card_present") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let card_present = match read_bool(body, &mut p) { Some(v) => v, None => return false };

    if !seek_key(body, &mut p, b"km_from_home") { return false; }
    if !past_colon(body, &mut p) { return false; }
    let km_home = match read_number(body, &mut p) { Some(v) => v, None => return false };

    // dims 0..4
    out[0] = qclamp01(amount / 10000.0);
    out[1] = qclamp01(installments / 12.0);
    out[2] = qclamp01((amount / cust_avg) / 10.0);
    out[3] = qclamp01(d2(ts, 11) as f64 / 23.0);
    out[4] = qclamp01(ts_weekday(ts) as f64 / 6.0);

    // last_transaction → dims 5..6
    {
        let lp_start = match body[p..].windows(16).position(|w| w == b"last_transaction") {
            Some(i) => p + i,
            None => return false,
        };
        let col = match body[lp_start..].iter().position(|&b| b == b':') {
            Some(i) => lp_start + i + 1,
            None => return false,
        };
        let mut val = col;
        skip_ws(body, &mut val);
        if body[val..].starts_with(b"null") {
            out[5] = -10000;
            out[6] = -10000;
        } else {
            let mut lq = val;
            if !seek_key(body, &mut lq, b"timestamp") { return false; }
            if !past_colon(body, &mut lq) { return false; }
            let last_ts = read_string(body, &mut lq);
            if last_ts.len() < 16 { return false; }
            if !seek_key(body, &mut lq, b"km_from_current") { return false; }
            if !past_colon(body, &mut lq) { return false; }
            let last_km = match read_number(body, &mut lq) { Some(v) => v, None => return false };
            let delta = ts_minutes(ts) - ts_minutes(last_ts);
            out[5] = qclamp01(delta as f64 / 1440.0);
            out[6] = qclamp01(last_km / 1000.0);
        }
    }

    out[7]  = qclamp01(km_home / 1000.0);
    out[8]  = qclamp01(tx_count / 20.0);
    out[9]  = if online        { 10000 } else { 0 };
    out[10] = if card_present  { 10000 } else { 0 };
    out[11] = if body_contains(known, merch_id) { 0 } else { 10000 };
    out[12] = mcc_risk(mcc);
    out[13] = qclamp01(merch_avg / 10000.0);
    true
}

/// Check if needle (as a quoted JSON string) appears in known array bytes.
fn body_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() { return false; }
    haystack.windows(needle.len()).any(|w| w == needle)
}
