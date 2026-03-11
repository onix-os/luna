// Lua pattern matching engine
// Port of lua-rs/crates/luars/src/stdlib/string/pattern/
// Operates on raw bytes (each byte is a "character", matching Lua semantics).

const MAX_CAPTURES: usize = 32;
const MAX_DEPTH: usize = 200;

/// A matched capture returned to callers
#[derive(Debug, Clone, Copy)]
pub enum Capture {
    Substring(usize, usize), // byte start, byte end in source
    Position(usize),         // 1-based byte position
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CapKind {
    Unfinished,
    Position,
    Closed(usize), // length
}

#[derive(Debug, Clone, Copy)]
struct CapState {
    start: usize,
    kind: CapKind,
}

struct MatchState<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    level: usize,
    capture: [CapState; MAX_CAPTURES],
    depth: usize,
    err: Option<String>,
}

impl<'a> MatchState<'a> {
    fn new(src: &'a [u8], pat: &'a [u8]) -> Self {
        MatchState {
            src,
            pat,
            level: 0,
            capture: [CapState {
                start: 0,
                kind: CapKind::Unfinished,
            }; MAX_CAPTURES],
            depth: 0,
            err: None,
        }
    }

    fn reset(&mut self) {
        self.level = 0;
        self.depth = 0;
        self.err = None;
    }

    fn check_captures(&self) -> Result<(), String> {
        for i in 0..self.level {
            if self.capture[i].kind == CapKind::Unfinished {
                return Err("unfinished capture".to_string());
            }
        }
        Ok(())
    }

    fn get_captures(&self) -> Vec<Capture> {
        let mut result = Vec::new();
        for i in 0..self.level {
            match self.capture[i].kind {
                CapKind::Position => {
                    result.push(Capture::Position(self.capture[i].start + 1));
                }
                CapKind::Closed(len) => {
                    let s = self.capture[i].start;
                    result.push(Capture::Substring(s, s + len));
                }
                CapKind::Unfinished => {}
            }
        }
        result
    }
}

/// Check if a byte matches a Lua class character
#[inline]
fn match_class(c: u8, cl: u8) -> bool {
    let lc = cl.to_ascii_lowercase();
    let res = match lc {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => c.is_ascii_whitespace(),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        b'z' => c == 0,
        _ => return c == cl, // not a class: match literally
    };
    if cl.is_ascii_uppercase() {
        !res
    } else {
        res
    }
}

/// Match `c` against a single pattern element at `pat[pp]`.
/// Returns true if it matches.
#[inline]
fn singlematch(c: u8, pat: &[u8], pp: usize) -> bool {
    if pp >= pat.len() {
        return false;
    }
    match pat[pp] {
        b'.' => true,
        b'%' if pp + 1 < pat.len() => match_class(c, pat[pp + 1]),
        b'[' => matchset(c, pat, pp),
        ch => c == ch,
    }
}

/// Returns the index just past the current pattern element (not including repetition)
#[inline]
fn element_end(pat: &[u8], pp: usize) -> usize {
    match pat[pp] {
        b'%' => pp + 2,
        b'[' => {
            let mut i = pp + 1;
            if i < pat.len() && pat[i] == b'^' {
                i += 1;
            }
            if i < pat.len() && pat[i] == b']' {
                i += 1;
            }
            while i < pat.len() && pat[i] != b']' {
                if pat[i] == b'%' && i + 1 < pat.len() {
                    i += 1;
                }
                i += 1;
            }
            i + 1
        }
        _ => pp + 1,
    }
}

/// Match `c` against a `[set]` starting at `pat[pp]` (pp points to `[`).
fn matchset(c: u8, pat: &[u8], pp: usize) -> bool {
    let mut i = pp + 1;
    let negated = i < pat.len() && pat[i] == b'^';
    if negated {
        i += 1;
    }
    let mut matched = false;
    // ']' as first char is literal
    if i < pat.len() && pat[i] == b']' {
        if c == b']' {
            matched = true;
        }
        i += 1;
    }
    while i < pat.len() && pat[i] != b']' {
        if pat[i] == b'%' && i + 1 < pat.len() {
            i += 1;
            if match_class(c, pat[i]) {
                matched = true;
            }
            i += 1;
        } else if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            if c >= pat[i] && c <= pat[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if c == pat[i] {
                matched = true;
            }
            i += 1;
        }
    }
    if negated {
        !matched
    } else {
        matched
    }
}

/// Core recursive match function.
/// Returns `Some(si_end)` on success, `None` on failure.
fn match_impl(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    if ms.err.is_some() {
        return None;
    }
    ms.depth += 1;
    if ms.depth > MAX_DEPTH {
        ms.err = Some("pattern too complex".to_string());
        ms.depth -= 1;
        return None;
    }
    let result = match_inner(ms, si, pp);
    ms.depth -= 1;
    result
}

fn match_inner(ms: &mut MatchState, mut si: usize, mut pp: usize) -> Option<usize> {
    loop {
        if pp >= ms.pat.len() {
            return Some(si);
        }
        match ms.pat[pp] {
            b'(' => {
                if pp + 1 < ms.pat.len() && ms.pat[pp + 1] == b')' {
                    return match_position_capture(ms, si, pp + 2);
                } else {
                    return match_open_capture(ms, si, pp + 1);
                }
            }
            b')' => {
                return match_close_capture(ms, si, pp + 1);
            }
            b'$' if pp + 1 >= ms.pat.len() => {
                return if si == ms.src.len() { Some(si) } else { None };
            }
            b'%' if pp + 1 < ms.pat.len() => match ms.pat[pp + 1] {
                b'b' => return match_balanced(ms, si, pp),
                b'f' => return match_frontier(ms, si, pp),
                c if c.is_ascii_digit() => return match_backref(ms, si, pp),
                _ => {} // fall through to normal element
            },
            _ => {}
        }

        let ep = element_end(ms.pat, pp);
        let has_rep = ep < ms.pat.len();
        if has_rep {
            match ms.pat[ep] {
                b'*' => return match_greedy(ms, si, pp, ep + 1, 0),
                b'+' => return match_greedy(ms, si, pp, ep + 1, 1),
                b'-' => return match_lazy(ms, si, pp, ep + 1),
                b'?' => return match_optional(ms, si, pp, ep + 1),
                _ => {}
            }
        }

        // Single match required
        if si < ms.src.len() && singlematch(ms.src[si], ms.pat, pp) {
            si += 1;
            pp = ep;
            continue;
        }
        return None;
    }
}

fn match_greedy(ms: &mut MatchState, si: usize, pp: usize, rp: usize, min: usize) -> Option<usize> {
    let mut count = 0;
    while si + count < ms.src.len() && singlematch(ms.src[si + count], ms.pat, pp) {
        count += 1;
    }
    while count >= min {
        if let Some(end) = match_impl(ms, si + count, rp) {
            return Some(end);
        }
        if count == 0 {
            break;
        }
        count -= 1;
    }
    None
}

fn match_lazy(ms: &mut MatchState, si: usize, pp: usize, rp: usize) -> Option<usize> {
    let mut i = si;
    loop {
        if let Some(end) = match_impl(ms, i, rp) {
            return Some(end);
        }
        if i < ms.src.len() && singlematch(ms.src[i], ms.pat, pp) {
            i += 1;
        } else {
            return None;
        }
    }
}

fn match_optional(ms: &mut MatchState, si: usize, pp: usize, rp: usize) -> Option<usize> {
    if si < ms.src.len() && singlematch(ms.src[si], ms.pat, pp) {
        if let Some(end) = match_impl(ms, si + 1, rp) {
            return Some(end);
        }
    }
    match_impl(ms, si, rp)
}

fn match_open_capture(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    let n = ms.level;
    if n >= MAX_CAPTURES {
        ms.err = Some("too many captures".to_string());
        return None;
    }
    ms.capture[n] = CapState {
        start: si,
        kind: CapKind::Unfinished,
    };
    ms.level = n + 1;
    let result = match_impl(ms, si, pp);
    if result.is_none() {
        ms.level = n;
    }
    result
}

fn match_close_capture(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    let mut n = ms.level;
    loop {
        if n == 0 {
            ms.err = Some("invalid pattern capture".to_string());
            return None;
        }
        n -= 1;
        if ms.capture[n].kind == CapKind::Unfinished {
            let len = si - ms.capture[n].start;
            ms.capture[n].kind = CapKind::Closed(len);
            let result = match_impl(ms, si, pp);
            if result.is_none() {
                ms.capture[n].kind = CapKind::Unfinished;
            }
            return result;
        }
    }
}

fn match_position_capture(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    let n = ms.level;
    if n >= MAX_CAPTURES {
        ms.err = Some("too many captures".to_string());
        return None;
    }
    ms.capture[n] = CapState {
        start: si,
        kind: CapKind::Position,
    };
    ms.level = n + 1;
    let result = match_impl(ms, si, pp);
    if result.is_none() {
        ms.level = n;
    }
    result
}

fn match_balanced(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    if pp + 3 >= ms.pat.len() {
        ms.err = Some("malformed pattern (missing arguments to '%b')".to_string());
        return None;
    }
    let open = ms.pat[pp + 2];
    let close = ms.pat[pp + 3];
    if si >= ms.src.len() || ms.src[si] != open {
        return None;
    }
    let mut depth = 1i32;
    let mut i = si + 1;
    while i < ms.src.len() && depth > 0 {
        if ms.src[i] == close {
            depth -= 1;
        } else if ms.src[i] == open {
            depth += 1;
        }
        i += 1;
    }
    if depth != 0 {
        return None;
    }
    match_impl(ms, i, pp + 4)
}

fn match_frontier(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    if pp + 2 >= ms.pat.len() || ms.pat[pp + 2] != b'[' {
        ms.err = Some("missing '[' after '%f' in pattern".to_string());
        return None;
    }
    let set_start = pp + 2;
    let set_end = element_end(ms.pat, set_start);
    let prev = if si > 0 { ms.src[si - 1] } else { 0 };
    let curr = if si < ms.src.len() { ms.src[si] } else { 0 };
    if !singlematch(prev, ms.pat, set_start) && singlematch(curr, ms.pat, set_start) {
        match_impl(ms, si, set_end)
    } else {
        None
    }
}

fn match_backref(ms: &mut MatchState, si: usize, pp: usize) -> Option<usize> {
    let n = (ms.pat[pp + 1] - b'0') as usize;
    if n == 0 || n > ms.level {
        ms.err = Some(format!("invalid capture index %{}", n));
        return None;
    }
    let idx = n - 1;
    let (cap_start, cap_len) = match ms.capture[idx].kind {
        CapKind::Closed(len) => (ms.capture[idx].start, len),
        _ => {
            ms.err = Some(format!("invalid capture index %{}", n));
            return None;
        }
    };
    if si + cap_len > ms.src.len() {
        return None;
    }
    if ms.src[si..si + cap_len] != ms.src[cap_start..cap_start + cap_len] {
        return None;
    }
    match_impl(ms, si + cap_len, pp + 2)
}

/// Check if pattern has no special characters (plain text).
pub fn is_plain(pat: &[u8]) -> bool {
    !pat.iter().any(|&c| {
        matches!(
            c,
            b'%' | b'.' | b'[' | b'*' | b'+' | b'-' | b'?' | b'^' | b'$' | b'('
        )
    })
}

/// Find a pattern match in `src` starting at byte offset `init`.
/// Returns `(start, end, captures)` or `None` if no match.
pub fn find(
    src: &[u8],
    pat: &[u8],
    init: usize,
) -> Result<Option<(usize, usize, Vec<Capture>)>, String> {
    if init > src.len() {
        return Ok(None);
    }

    // Fast path: plain pattern
    if is_plain(pat) {
        if pat.is_empty() {
            return Ok(Some((init, init, vec![])));
        }
        return Ok(find_plain(src, pat, init).map(|pos| (pos, pos + pat.len(), vec![])));
    }

    let anchored = !pat.is_empty() && pat[0] == b'^';
    let pp_start = if anchored { 1 } else { 0 };

    // Validate pattern
    validate_pattern(pat, pp_start)?;

    let mut ms = MatchState::new(src, pat);
    let mut si = init;
    loop {
        ms.reset();
        if let Some(end) = match_impl(&mut ms, si, pp_start) {
            if let Some(err) = ms.err.take() {
                return Err(err);
            }
            ms.check_captures()?;
            let caps = ms.get_captures();
            return Ok(Some((si, end, caps)));
        }
        if let Some(err) = ms.err.take() {
            return Err(err);
        }
        if anchored || si >= src.len() {
            return Ok(None);
        }
        si += 1;
    }
}

fn find_plain(src: &[u8], pat: &[u8], init: usize) -> Option<usize> {
    if pat.is_empty() {
        return Some(init);
    }
    if pat.len() > src.len().saturating_sub(init) {
        return None;
    }
    src[init..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| init + p)
}

/// Validate a pattern for obvious errors before matching.
fn validate_pattern(pat: &[u8], start: usize) -> Result<(), String> {
    let mut i = start;
    while i < pat.len() {
        match pat[i] {
            b'%' => {
                if i + 1 >= pat.len() {
                    return Err("malformed pattern (ends with '%')".to_string());
                }
                match pat[i + 1] {
                    b'b' => {
                        if i + 3 >= pat.len() {
                            return Err("malformed pattern (missing arguments to '%b')".to_string());
                        }
                        i += 4;
                        continue;
                    }
                    b'f' => {
                        i += 2;
                        if i >= pat.len() || pat[i] != b'[' {
                            return Err("missing '[' after '%f' in pattern".to_string());
                        }
                        i = skip_set(pat, i)?;
                        continue;
                    }
                    _ => {
                        i += 2;
                        if i < pat.len() && matches!(pat[i], b'*' | b'+' | b'-' | b'?') {
                            i += 1;
                        }
                        continue;
                    }
                }
            }
            b'[' => {
                i = skip_set(pat, i)?;
                if i < pat.len() && matches!(pat[i], b'*' | b'+' | b'-' | b'?') {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
        if i < pat.len() && matches!(pat[i], b'*' | b'+' | b'-' | b'?') {
            i += 1;
        }
    }
    Ok(())
}

fn skip_set(pat: &[u8], i: usize) -> Result<usize, String> {
    let mut j = i + 1;
    if j < pat.len() && pat[j] == b'^' {
        j += 1;
    }
    if j < pat.len() && pat[j] == b']' {
        j += 1;
    }
    while j < pat.len() && pat[j] != b']' {
        if pat[j] == b'%' {
            j += 1;
            if j >= pat.len() {
                return Err("malformed pattern (ends with '%')".to_string());
            }
        }
        j += 1;
    }
    if j >= pat.len() {
        return Err("malformed pattern (missing ']')".to_string());
    }
    Ok(j + 1)
}

/// Substitute captures in a replacement string.
/// `%0` = whole match, `%1`-`%9` = captures, `%%` = `%`.
pub fn apply_replacement(
    repl: &[u8],
    src: &[u8],
    match_start: usize,
    match_end: usize,
    captures: &[Capture],
) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < repl.len() {
        if repl[i] == b'%' {
            if i + 1 >= repl.len() {
                result.push(b'%');
                i += 1;
                continue;
            }
            match repl[i + 1] {
                b'%' => {
                    result.push(b'%');
                    i += 2;
                }
                c if c.is_ascii_digit() => {
                    let n = (c - b'0') as usize;
                    if n == 0 {
                        result.extend_from_slice(&src[match_start..match_end]);
                    } else if n <= captures.len() {
                        match captures[n - 1] {
                            Capture::Substring(s, e) => result.extend_from_slice(&src[s..e]),
                            Capture::Position(p) => {
                                result.extend_from_slice(p.to_string().as_bytes())
                            }
                        }
                    } else {
                        return Err(format!("invalid capture index %{}", n));
                    }
                    i += 2;
                }
                _ => {
                    return Err("invalid use of '%' in replacement string".to_string());
                }
            }
        } else {
            result.push(repl[i]);
            i += 1;
        }
    }
    Ok(result)
}

/// A match result for gsub
pub struct MatchResult {
    pub start: usize,
    pub end: usize,
    pub captures: Vec<Capture>,
}

/// Find the next match after `init`, skipping if end == last_match_end.
/// Returns the match, or None if no more matches.
pub fn find_next(
    src: &[u8],
    pat: &[u8],
    init: usize,
    last_end: Option<usize>,
) -> Result<Option<MatchResult>, String> {
    if init > src.len() {
        return Ok(None);
    }

    let anchored = !pat.is_empty() && pat[0] == b'^';
    let pp_start = if anchored { 1 } else { 0 };

    if is_plain(pat) {
        if pat.is_empty() {
            // empty pattern: match at every position
            if let Some(le) = last_end {
                if init == le && init <= src.len() {
                    // skip to avoid infinite loop
                    let next = init + 1;
                    if next > src.len() {
                        return Ok(None);
                    }
                    return Ok(Some(MatchResult {
                        start: next,
                        end: next,
                        captures: vec![],
                    }));
                }
            }
            return Ok(Some(MatchResult {
                start: init,
                end: init,
                captures: vec![],
            }));
        }
        return Ok(find_plain(src, pat, init).map(|pos| MatchResult {
            start: pos,
            end: pos + pat.len(),
            captures: vec![],
        }));
    }

    if !anchored {
        validate_pattern(pat, pp_start)?;
    }

    let mut ms = MatchState::new(src, pat);
    let mut si = init;
    loop {
        ms.reset();
        if let Some(end) = match_impl(&mut ms, si, pp_start) {
            if let Some(err) = ms.err.take() {
                return Err(err);
            }
            ms.check_captures()?;
            // Skip if end == last_end (empty match at same position)
            if Some(end) == last_end && end == si {
                if si >= src.len() {
                    return Ok(None);
                }
                si += 1;
                continue;
            }
            let caps = ms.get_captures();
            return Ok(Some(MatchResult {
                start: si,
                end,
                captures: caps,
            }));
        }
        if let Some(err) = ms.err.take() {
            return Err(err);
        }
        if anchored || si >= src.len() {
            return Ok(None);
        }
        si += 1;
    }
}
